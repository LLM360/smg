//! Adaptive admission shared by the gRPC and HTTP serving paths.
//!
//! The existing priority scheduler remains the infrastructure safety layer.
//! The original strategy predicts output-token work. The engine-feedback
//! strategy instead learns each partition's useful running-concurrency knee
//! from live throughput, probes just above it, and backs off on engine queue or
//! KV pressure. Shadow mode exercises either state machine without delaying or
//! rejecting traffic.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::{Duration, Instant},
};

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use openai_protocol::worker::WorkerLoadResponse;
use parking_lot::Mutex;
use tokio::sync::watch;

use crate::{
    config::{
        AdaptiveAdmissionConfig, AdaptiveAdmissionMode, AdaptiveAdmissionStrategy,
        DISTRIBUTION_HEADROOM_MAX_AGE_SECS,
    },
    middleware::scheduler::{state::AdaptiveCapacityProvider, ADMISSION_PARTITION_LABEL},
    observability::metrics::intern_string,
    worker::{monitor::ObservedWorkerLoad, ConnectionMode, WorkerRegistry, WorkerType},
};

const PREDICTIONS_TOTAL: &str = "smg_adaptive_admission_predictions_total";
const PREDICTED_OUTPUT_TOKENS: &str = "smg_adaptive_admission_predicted_output_tokens";
const OBSERVED_OUTPUT_TOKENS: &str = "smg_adaptive_admission_observed_output_tokens";
const ABSOLUTE_ERROR_TOKENS: &str = "smg_adaptive_admission_absolute_error_tokens";
const DECISIONS_TOTAL: &str = "smg_adaptive_admission_decisions_total";
const OUTSTANDING_TOKENS: &str = "smg_adaptive_admission_outstanding_tokens";
const ROUTER_OUTSTANDING_TOKENS: &str = "smg_adaptive_admission_router_outstanding_tokens";
const ENGINE_ESTIMATED_TOKENS: &str = "smg_adaptive_admission_engine_estimated_tokens";
const WORK_BUDGET_TOKENS: &str = "smg_adaptive_admission_work_budget_tokens";
const DRAIN_SECONDS: &str = "smg_adaptive_admission_predicted_drain_seconds";
const LOAD_COVERAGE: &str = "smg_adaptive_admission_load_coverage";
const GEN_THROUGHPUT: &str = "smg_adaptive_admission_generation_tokens_per_second";
const LEARNED_CAPACITY: &str = "smg_adaptive_admission_learned_capacity_tokens_per_second";
const ENGINE_RUNNING: &str = "smg_adaptive_admission_engine_running_requests";
const ENGINE_WAITING: &str = "smg_adaptive_admission_engine_waiting_requests";
const ENGINE_WAITING_TOKENS: &str = "smg_adaptive_admission_engine_waiting_uncached_tokens";
const ENGINE_TOKEN_USAGE: &str = "smg_adaptive_admission_engine_max_token_usage";
const ENGINE_MEAN_TOKEN_USAGE: &str = "smg_adaptive_admission_engine_mean_token_usage";
const ENGINE_MAX_RUNNING: &str = "smg_adaptive_admission_engine_max_running_requests";
const ENGINE_MAX_RUNNING_COVERAGE: &str =
    "smg_adaptive_admission_engine_max_running_requests_coverage";
const DISTRIBUTION_HEADROOM: &str = "smg_adaptive_admission_distribution_headroom_requests";
const DISTRIBUTION_HEADROOM_COVERAGE: &str =
    "smg_adaptive_admission_distribution_headroom_coverage";
const DISTRIBUTION_HEADROOM_ACTIVE: &str =
    "smg_adaptive_admission_distribution_headroom_active_requests";
const DISTRIBUTION_OVERRIDES_TOTAL: &str = "smg_adaptive_admission_distribution_overrides_total";
const ROUTER_OUTSTANDING_REQUESTS: &str = "smg_adaptive_admission_router_outstanding_requests";
const FEEDBACK_RUNNING_LIMIT: &str = "smg_adaptive_admission_feedback_running_limit";
const FEEDBACK_KNEE_PER_REPLICA: &str = "smg_adaptive_admission_feedback_knee_requests_per_replica";
const SEGMENTS: &str = "smg_adaptive_admission_estimator_segments";
const DISTRIBUTION_HEADROOM_MAX_AGE: Duration =
    Duration::from_secs(DISTRIBUTION_HEADROOM_MAX_AGE_SECS);

pub(crate) const FLAG_MULTIPLE_COMPLETIONS: u16 = 1 << 0;
pub(crate) const FLAG_TOOLS: u16 = 1 << 1;
pub(crate) const FLAG_STRUCTURED_OUTPUT: u16 = 1 << 2;
pub(crate) const FLAG_REASONING: u16 = 1 << 3;
pub(crate) const FLAG_STREAMING: u16 = 1 << 4;

pub(crate) fn describe_metrics() {
    describe_counter!(
        PREDICTIONS_TOTAL,
        "Adaptive output-token predictions by model and fallback level"
    );
    describe_histogram!(
        PREDICTED_OUTPUT_TOKENS,
        "Predicted completion tokens per adaptive-admission request"
    );
    describe_histogram!(
        OBSERVED_OUTPUT_TOKENS,
        "Observed completion tokens for requests learned by adaptive admission"
    );
    describe_histogram!(
        ABSOLUTE_ERROR_TOKENS,
        "Absolute adaptive output-token prediction error"
    );
    describe_counter!(
        DECISIONS_TOTAL,
        "Adaptive token-work admission decisions, including shadow decisions"
    );
    describe_gauge!(
        OUTSTANDING_TOKENS,
        "Effective outstanding output-token estimate used for adaptive admission"
    );
    describe_gauge!(
        ROUTER_OUTSTANDING_TOKENS,
        "Predicted output tokens reserved by this router process"
    );
    describe_gauge!(
        ENGINE_ESTIMATED_TOKENS,
        "Estimated output tokens already running or waiting on engines"
    );
    describe_gauge!(
        WORK_BUDGET_TOKENS,
        "Live output-token work budget derived from engine throughput and the configured horizon"
    );
    describe_gauge!(
        DRAIN_SECONDS,
        "Predicted seconds to drain outstanding output work at current engine throughput"
    );
    describe_gauge!(
        LOAD_COVERAGE,
        "Fraction of healthy replicas with fresh engine-load telemetry"
    );
    describe_gauge!(
        GEN_THROUGHPUT,
        "Aggregate generation throughput reported by engines in an admission partition"
    );
    describe_gauge!(
        LEARNED_CAPACITY,
        "Recent decayed-peak generation capacity learned from engine telemetry"
    );
    describe_gauge!(
        ENGINE_RUNNING,
        "Engine-reported running requests in an admission partition"
    );
    describe_gauge!(
        ENGINE_WAITING,
        "Engine-reported waiting requests in an admission partition"
    );
    describe_gauge!(
        ENGINE_WAITING_TOKENS,
        "Engine-reported waiting uncached tokens in an admission partition"
    );
    describe_gauge!(
        ENGINE_TOKEN_USAGE,
        "Maximum engine-reported token usage in an admission partition"
    );
    describe_gauge!(
        ENGINE_MEAN_TOKEN_USAGE,
        "Mean engine-reported token usage in an admission partition"
    );
    describe_gauge!(
        ENGINE_MAX_RUNNING,
        "Sum of engine-reported maximum running requests in an admission partition"
    );
    describe_gauge!(
        ENGINE_MAX_RUNNING_COVERAGE,
        "Fraction of healthy replicas contributing a maximum-running-requests ceiling"
    );
    describe_gauge!(
        DISTRIBUTION_HEADROOM,
        "Safe cache-policy-agnostic request headroom across fully observed workers"
    );
    describe_gauge!(
        DISTRIBUTION_HEADROOM_COVERAGE,
        "Fraction of healthy replicas contributing strict per-worker headroom"
    );
    describe_gauge!(
        DISTRIBUTION_HEADROOM_ACTIVE,
        "Pre-dispatch distribution-headroom permits active in this gateway"
    );
    describe_counter!(
        DISTRIBUTION_OVERRIDES_TOTAL,
        "Rejected adaptive admissions explicitly authorized for an exact distribution route"
    );
    describe_gauge!(
        ROUTER_OUTSTANDING_REQUESTS,
        "Requests currently tracked by this router process"
    );
    describe_gauge!(
        FEEDBACK_RUNNING_LIMIT,
        "Dynamic request limit selected by engine-feedback admission"
    );
    describe_gauge!(
        FEEDBACK_KNEE_PER_REPLICA,
        "Learned running requests per replica at the throughput knee"
    );
    describe_gauge!(
        SEGMENTS,
        "Current bounded in-memory output estimator segment count"
    );
}

#[derive(Debug, Clone)]
pub(crate) struct PredictionFeatures {
    pub model: String,
    pub user: String,
    pub workload_type: String,
    pub endpoint: &'static str,
    pub prompt_tokens: u32,
    /// Total upper bound after multiplying per-completion limits by request
    /// multiplicity. `None` means the client did not supply an upper bound.
    pub max_output_tokens: Option<u32>,
    pub generation_flags: u16,
}

impl PredictionFeatures {
    fn prompt_bucket(&self) -> u8 {
        if self.prompt_tokens == 0 {
            0
        } else {
            (u32::BITS - self.prompt_tokens.leading_zeros()) as u8
        }
    }

    fn output_limit_bucket(&self) -> u8 {
        self.max_output_tokens.map_or(0, |tokens| {
            if tokens == 0 {
                0
            } else {
                (u32::BITS - tokens.leading_zeros()) as u8
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PredictionSource {
    ColdStart,
    Model,
    UserOrWorkload,
    UserWorkload,
    Full,
}

impl PredictionSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::ColdStart => "cold_start",
            Self::Model => "model",
            Self::UserOrWorkload => "user_or_workload",
            Self::UserWorkload => "user_workload",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone)]
struct Prediction {
    output_tokens: u32,
    model_output_tokens: u32,
    source: PredictionSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SegmentKey {
    Model(String),
    Workload(String, String),
    User(String, String),
    UserWorkload(String, String, String),
    Full {
        model: String,
        user: String,
        workload: String,
        endpoint: &'static str,
        prompt_bucket: u8,
        output_limit_bucket: u8,
        generation_flags: u16,
    },
}

#[derive(Debug, Clone)]
struct DecayedMean {
    weight: f64,
    weighted_sum: f64,
    last_update: Instant,
}

impl DecayedMean {
    fn new(value: f64, now: Instant) -> Self {
        Self {
            weight: 1.0,
            weighted_sum: value,
            last_update: now,
        }
    }

    fn decay_factor(&self, now: Instant, half_life_secs: f64) -> f64 {
        let elapsed = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64();
        2.0_f64.powf(-elapsed / half_life_secs)
    }

    fn effective(&self, now: Instant, half_life_secs: f64) -> (f64, f64) {
        let decay = self.decay_factor(now, half_life_secs);
        (self.weight * decay, self.weighted_sum * decay)
    }

    fn update(&mut self, value: f64, now: Instant, half_life_secs: f64) {
        let decay = self.decay_factor(now, half_life_secs);
        self.weight = self.weight * decay + 1.0;
        self.weighted_sum = self.weighted_sum * decay + value;
        self.last_update = now;
    }
}

#[derive(Debug)]
struct HierarchicalPredictor {
    half_life_secs: f64,
    prior_observations: f64,
    cold_start_output_tokens: u32,
    max_segments: usize,
    segments: HashMap<SegmentKey, DecayedMean>,
}

impl HierarchicalPredictor {
    fn new(config: &AdaptiveAdmissionConfig) -> Self {
        Self {
            half_life_secs: config.estimator_half_life_secs,
            prior_observations: config.prior_observations,
            cold_start_output_tokens: config.cold_start_output_tokens,
            max_segments: config.max_segments,
            segments: HashMap::new(),
        }
    }

    fn keys(features: &PredictionFeatures) -> [SegmentKey; 5] {
        [
            SegmentKey::Model(features.model.clone()),
            SegmentKey::Workload(features.model.clone(), features.workload_type.clone()),
            SegmentKey::User(features.model.clone(), features.user.clone()),
            SegmentKey::UserWorkload(
                features.model.clone(),
                features.user.clone(),
                features.workload_type.clone(),
            ),
            SegmentKey::Full {
                model: features.model.clone(),
                user: features.user.clone(),
                workload: features.workload_type.clone(),
                endpoint: features.endpoint,
                prompt_bucket: features.prompt_bucket(),
                output_limit_bucket: features.output_limit_bucket(),
                generation_flags: features.generation_flags,
            },
        ]
    }

    fn estimate(&self, key: &SegmentKey, now: Instant) -> Option<(f64, f64)> {
        let (weight, sum) = self.segments.get(key)?.effective(now, self.half_life_secs);
        (weight > f64::EPSILON).then_some((sum / weight, weight))
    }

    fn blend(&self, prior: f64, estimate: Option<(f64, f64)>) -> (f64, bool) {
        let Some((mean, weight)) = estimate else {
            return (prior, false);
        };
        let denominator = weight + self.prior_observations;
        if denominator <= f64::EPSILON {
            return (mean, true);
        }
        (
            (mean * weight + prior * self.prior_observations) / denominator,
            true,
        )
    }

    fn predict_at(&self, features: &PredictionFeatures, now: Instant) -> Prediction {
        let [model, workload, user, user_workload, full] = Self::keys(features);
        let cold = f64::from(self.cold_start_output_tokens);
        let (model_prediction, has_model) = self.blend(cold, self.estimate(&model, now));

        let workload_estimate = self.estimate(&workload, now);
        let user_estimate = self.estimate(&user, now);
        let parent = match (workload_estimate, user_estimate) {
            (Some((workload_mean, workload_weight)), Some((user_mean, user_weight))) => {
                let total = workload_weight + user_weight;
                if total <= f64::EPSILON {
                    model_prediction
                } else {
                    (workload_mean * workload_weight + user_mean * user_weight) / total
                }
            }
            (Some((mean, _)), None) | (None, Some((mean, _))) => mean,
            (None, None) => model_prediction,
        };
        let has_user_or_workload = workload_estimate.is_some() || user_estimate.is_some();
        let (user_workload_prediction, has_user_workload) =
            self.blend(parent, self.estimate(&user_workload, now));
        let (full_prediction, has_full) =
            self.blend(user_workload_prediction, self.estimate(&full, now));

        let source = if has_full {
            PredictionSource::Full
        } else if has_user_workload {
            PredictionSource::UserWorkload
        } else if has_user_or_workload {
            PredictionSource::UserOrWorkload
        } else if has_model {
            PredictionSource::Model
        } else {
            PredictionSource::ColdStart
        };
        let mut output_tokens = full_prediction.round().clamp(1.0, f64::from(u32::MAX)) as u32;
        if let Some(maximum) = features.max_output_tokens {
            output_tokens = output_tokens.min(maximum.max(1));
        }
        Prediction {
            output_tokens,
            model_output_tokens: model_prediction.round().clamp(1.0, f64::from(u32::MAX)) as u32,
            source,
        }
    }

    fn observe_at(&mut self, features: &PredictionFeatures, output_tokens: u32, now: Instant) {
        for key in Self::keys(features) {
            self.segments
                .entry(key)
                .and_modify(|mean| {
                    mean.update(f64::from(output_tokens), now, self.half_life_secs);
                })
                .or_insert_with(|| DecayedMean::new(f64::from(output_tokens), now));
        }
        self.evict_oldest();
    }

    fn evict_oldest(&mut self) {
        if self.segments.len() <= self.max_segments {
            return;
        }
        // Evict a batch so a stream of one-off users does not sort the entire
        // table on every completion after reaching the limit.
        let batch = (self.max_segments / 10).max(1);
        let target = self.max_segments.saturating_sub(batch);
        let excess = self.segments.len().saturating_sub(target);
        let mut oldest: Vec<_> = self
            .segments
            .iter()
            .map(|(key, value)| (key.clone(), value.last_update))
            .collect();
        oldest.sort_unstable_by_key(|(_, updated)| *updated);
        for (key, _) in oldest.into_iter().take(excess) {
            self.segments.remove(&key);
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PartitionLoad {
    model_ids: HashSet<String>,
    healthy_replicas: u32,
    observed_replicas: u32,
    generation_tokens_per_second: f64,
    learned_capacity_tokens_per_second: f64,
    running_requests: i64,
    waiting_requests: i64,
    waiting_uncached_tokens: i64,
    max_token_usage: f64,
    token_usage_sum: f64,
    max_running_requests: i64,
    max_running_observed_replicas: u32,
    strict_max_running_requests: i64,
    strict_effective_occupancy: i64,
    headroom_healthy_replicas: u32,
    headroom_observed_replicas: u32,
    issuable_headroom_requests: i64,
    worker_headroom_requests: HashMap<String, WorkerHeadroom>,
    worker_router_load_observations: HashMap<(String, u64, u64), usize>,
    headroom_epoch: u64,
    observed_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct WorkerHeadroom {
    requests: i64,
    generation_id: u64,
    revision: u64,
    router_load_at_observation: usize,
}

#[derive(Debug, Clone, Copy)]
struct StrictWorkerCapacity {
    running_requests: i64,
    waiting_requests: i64,
    max_running_requests: i64,
    max_pressure: f64,
}

fn strict_worker_capacity(
    load: &WorkerLoadResponse,
    registered_max_running_requests: Option<u16>,
) -> Option<StrictWorkerCapacity> {
    let rank_count = usize::try_from(load.dp_rank_count).ok()?;
    if rank_count == 0 || rank_count != load.loads.len() {
        return None;
    }
    let mut ranks = HashSet::with_capacity(rank_count);
    let mut capacity = StrictWorkerCapacity {
        running_requests: 0,
        waiting_requests: 0,
        max_running_requests: 0,
        max_pressure: 0.0,
    };
    for rank in &load.loads {
        if rank.max_running_requests < 0 {
            return None;
        }
        let max_running_requests = if rank.max_running_requests > 0 {
            rank.max_running_requests
        } else if rank_count == 1 {
            // Older SGLang HTTP `/get_load` payloads omit this field even
            // though worker discovery publishes the exact engine-wide cap.
            // The metadata fallback is unambiguous only for a single DP rank;
            // multi-rank workers remain fail closed unless every rank reports
            // its own capacity.
            i32::from(registered_max_running_requests?)
        } else {
            return None;
        };
        if !ranks.insert(rank.dp_rank)
            || rank.num_running_reqs < 0
            || rank.num_waiting_reqs < 0
            || !rank.token_usage.is_finite()
            || !rank.utilization.is_finite()
            || !(0.0..=1.0).contains(&rank.token_usage)
            || !(0.0..=1.0).contains(&rank.utilization)
        {
            return None;
        }
        capacity.running_requests = capacity
            .running_requests
            .saturating_add(i64::from(rank.num_running_reqs));
        capacity.waiting_requests = capacity
            .waiting_requests
            .saturating_add(i64::from(rank.num_waiting_reqs));
        capacity.max_running_requests = capacity
            .max_running_requests
            .saturating_add(i64::from(max_running_requests));
        capacity.max_pressure = capacity
            .max_pressure
            .max(rank.token_usage.max(rank.utilization));
    }
    Some(capacity)
}

#[derive(Debug, Clone)]
struct CapacityEstimate {
    per_replica_tokens_per_second: f64,
    last_update: Instant,
}

impl CapacityEstimate {
    fn effective(&self, now: Instant, half_life_secs: f64) -> f64 {
        let elapsed = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64();
        self.per_replica_tokens_per_second * 2.0_f64.powf(-elapsed / half_life_secs)
    }

    fn observe(&mut self, value: f64, now: Instant, half_life_secs: f64) {
        self.per_replica_tokens_per_second = self.effective(now, half_life_secs).max(value);
        self.last_update = now;
    }
}

#[derive(Debug, Clone)]
struct FeedbackEstimate {
    peak_tokens_per_second_per_replica: f64,
    running_requests_per_replica_at_peak: f64,
    /// True after the partition has crossed a live pressure boundary. This
    /// keeps a tiny idle-throughput sample from becoming a restrictive learned
    /// cap when the engine does not publish `max_running_requests`.
    pressure_observed: bool,
    last_update: Instant,
}

impl FeedbackEstimate {
    fn effective_peak(&self, now: Instant, half_life_secs: f64) -> f64 {
        let elapsed = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64();
        self.peak_tokens_per_second_per_replica * 2.0_f64.powf(-elapsed / half_life_secs)
    }

    fn observe(
        &mut self,
        throughput_per_replica: f64,
        running_per_replica: f64,
        now: Instant,
        half_life_secs: f64,
        improvement_ratio: f64,
        under_pressure: bool,
    ) {
        self.pressure_observed |= under_pressure;
        let effective_peak = self.effective_peak(now, half_life_secs);
        let raises_peak =
            throughput_per_replica > effective_peak * (1.0 + improvement_ratio.max(0.0));
        let same_plateau =
            throughput_per_replica >= effective_peak * (1.0 - improvement_ratio.clamp(0.0, 1.0));
        if raises_peak || same_plateau {
            self.peak_tokens_per_second_per_replica = effective_peak.max(throughput_per_replica);
            if raises_peak || running_per_replica < self.running_requests_per_replica_at_peak {
                self.running_requests_per_replica_at_peak = running_per_replica;
            }
            self.last_update = now;
        }
    }
}

impl PartitionLoad {
    fn coverage(&self) -> f64 {
        if self.healthy_replicas == 0 {
            0.0
        } else {
            f64::from(self.observed_replicas) / f64::from(self.healthy_replicas)
        }
    }

    fn mean_token_usage(&self) -> f64 {
        if self.observed_replicas == 0 {
            0.0
        } else {
            self.token_usage_sum / f64::from(self.observed_replicas)
        }
    }

    fn max_running_coverage(&self) -> f64 {
        if self.healthy_replicas == 0 {
            0.0
        } else {
            f64::from(self.max_running_observed_replicas) / f64::from(self.healthy_replicas)
        }
    }

    fn scaled_max_running_requests(&self) -> f64 {
        if self.max_running_observed_replicas == 0 {
            0.0
        } else {
            (self.max_running_requests as f64 * f64::from(self.healthy_replicas)
                / f64::from(self.max_running_observed_replicas))
            .floor()
        }
    }

    fn headroom_coverage(&self) -> f64 {
        if self.headroom_healthy_replicas == 0 {
            0.0
        } else {
            f64::from(self.headroom_observed_replicas) / f64::from(self.headroom_healthy_replicas)
        }
    }

    fn headroom_is_fresh(&self) -> bool {
        self.observed_at
            .is_some_and(|observed_at| observed_at.elapsed() <= DISTRIBUTION_HEADROOM_MAX_AGE)
    }
}

#[derive(Debug, Default)]
struct ActiveDistributionHeadroom {
    total: u32,
    by_worker: HashMap<(String, u64, u64), u32>,
}

#[derive(Debug, Default)]
struct WorkState {
    outstanding_tokens: HashMap<String, u64>,
    outstanding_requests: HashMap<String, u64>,
    loads: HashMap<String, PartitionLoad>,
    capacities: HashMap<String, CapacityEstimate>,
    feedback_estimates: HashMap<String, FeedbackEstimate>,
    active_distribution_headroom: HashMap<String, ActiveDistributionHeadroom>,
    headroom_epoch: u64,
}

#[derive(Debug, Clone, Copy)]
struct EngineFeedbackConstraints {
    telemetry_usable: bool,
    running_limit: Option<f64>,
    pressure_reason: Option<&'static str>,
}

fn distribution_challenge_reason(reason: &str) -> bool {
    matches!(reason, "engine_waiting" | "running_limit")
}

fn engine_feedback_constraints(
    config: &AdaptiveAdmissionConfig,
    load: &PartitionLoad,
    feedback_estimate: Option<&FeedbackEstimate>,
) -> EngineFeedbackConstraints {
    let telemetry_usable = load.coverage() >= config.min_load_coverage;
    let engine_limit = if load.max_running_coverage() >= config.min_load_coverage {
        Some(load.scaled_max_running_requests())
    } else {
        None
    };
    let learned_limit = feedback_estimate
        .filter(|estimate| engine_limit.is_some() || estimate.pressure_observed)
        .map(|estimate| {
            (estimate.running_requests_per_replica_at_peak * f64::from(load.healthy_replicas))
                .ceil()
                + f64::from(
                    config
                        .feedback_probe_requests_per_healthy_replica
                        .saturating_mul(load.healthy_replicas),
                )
        });
    let running_limit = match (learned_limit, engine_limit) {
        (Some(learned), Some(engine)) => Some(learned.max(1.0).min(engine)),
        (Some(learned), None) => Some(learned.max(1.0)),
        (None, engine) => engine,
    };
    let waiting_limit = i64::from(config.feedback_max_waiting_requests_per_healthy_replica)
        * i64::from(load.observed_replicas);
    let pressure_reason = if load.mean_token_usage() >= config.feedback_max_token_usage {
        Some("token_pressure")
    } else if load.waiting_requests > waiting_limit {
        Some("engine_waiting")
    } else {
        None
    };
    EngineFeedbackConstraints {
        telemetry_usable,
        running_limit,
        pressure_reason,
    }
}

#[derive(Debug)]
struct PredictionSampleState {
    skip_per_model: u64,
    limit_per_model: u64,
    seen_per_model: HashMap<String, u64>,
}

impl PredictionSampleState {
    fn from_env() -> Option<Self> {
        let limit_per_model =
            std::env::var("SMG_ADAPTIVE_ADMISSION_PREDICTION_SAMPLE_LIMIT_PER_MODEL")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        if limit_per_model == 0 {
            return None;
        }
        let skip_per_model =
            std::env::var("SMG_ADAPTIVE_ADMISSION_PREDICTION_SAMPLE_SKIP_PER_MODEL")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        Some(Self {
            skip_per_model,
            limit_per_model,
            seen_per_model: HashMap::new(),
        })
    }

    fn should_record(&mut self, model: &str) -> bool {
        let seen = self.seen_per_model.entry(model.to_string()).or_default();
        let record = *seen >= self.skip_per_model
            && *seen < self.skip_per_model.saturating_add(self.limit_per_model);
        *seen = seen.saturating_add(1);
        record
    }
}

#[derive(Debug)]
pub(crate) struct AdaptiveAdmissionController {
    config: AdaptiveAdmissionConfig,
    predictor: Mutex<HierarchicalPredictor>,
    work: Mutex<WorkState>,
    prediction_samples: Option<Mutex<PredictionSampleState>>,
    registry: Arc<WorkerRegistry>,
    capacity_revision: watch::Sender<u64>,
}

/// Pre-dispatch permit for one adaptive distribution route. It is scoped to
/// one gateway process and transferred to the ordinary worker-load guard at
/// dispatch, so it remains disabled unless an explicit partition allowlist
/// enables the higher-level routing feature.
#[derive(Debug)]
pub(crate) struct DistributionHeadroomPermit {
    controller: Weak<AdaptiveAdmissionController>,
    partition: String,
    telemetry_epoch: u64,
    target: Option<(String, u64, u64)>,
    active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DistributionHeadroomCandidate {
    pub(crate) worker_url: String,
    pub(crate) worker_generation_id: u64,
    pub(crate) worker_revision: u64,
}

impl DistributionHeadroomPermit {
    /// Bind the already-reserved aggregate slot to the exact route selected by
    /// the cache policy. A telemetry or worker-generation change fails closed.
    pub(crate) fn bind_target(
        &mut self,
        target_url: &str,
        target_generation_id: u64,
        target_revision: u64,
    ) -> bool {
        if self.target.is_some() {
            return false;
        }
        let Some(controller) = self.controller.upgrade() else {
            return false;
        };
        if controller.bind_distribution_target(
            &self.partition,
            self.telemetry_epoch,
            target_url,
            target_generation_id,
            target_revision,
        ) {
            self.target = Some((
                target_url.to_string(),
                target_generation_id,
                target_revision,
            ));
            true
        } else {
            false
        }
    }

    pub(crate) fn validate_for_dispatch(&self) -> bool {
        let Some((target_url, target_generation_id, target_revision)) = self.target.as_ref() else {
            return false;
        };
        self.controller.upgrade().is_some_and(|controller| {
            controller.validate_distribution_target(
                &self.partition,
                self.telemetry_epoch,
                target_url,
                *target_generation_id,
                *target_revision,
            )
        })
    }

    /// Atomically transfer this adaptive reservation to the exact target's
    /// already-created ordinary `WorkerLoadGuard`. The controller discounts
    /// exactly that one new router-local load, revalidates the telemetry epoch
    /// and target generation, and decrements this permit while holding one work
    /// lock. On failure, consuming `self` leaves the permit active so `Drop`
    /// performs the normal fail-closed release.
    pub(crate) fn try_transfer_after_worker_load_reserved(mut self) -> bool {
        if !self.active {
            return false;
        }
        let Some((target_url, target_generation_id, target_revision)) = self.target.clone() else {
            return false;
        };
        let Some(controller) = self.controller.upgrade() else {
            return false;
        };
        if !controller.try_transfer_distribution_headroom(
            &self.partition,
            self.telemetry_epoch,
            &target_url,
            target_generation_id,
            target_revision,
        ) {
            return false;
        }
        self.active = false;
        true
    }
}

impl Drop for DistributionHeadroomPermit {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Some(controller) = self.controller.upgrade() else {
            return;
        };
        controller.release_distribution_headroom(&self.partition, self.target.as_ref());
    }
}

impl AdaptiveAdmissionController {
    pub(crate) fn new(config: AdaptiveAdmissionConfig, registry: Arc<WorkerRegistry>) -> Arc<Self> {
        let (capacity_revision, _) = watch::channel(0_u64);
        Arc::new(Self {
            predictor: Mutex::new(HierarchicalPredictor::new(&config)),
            config,
            work: Mutex::new(WorkState::default()),
            prediction_samples: PredictionSampleState::from_env().map(Mutex::new),
            registry,
            capacity_revision,
        })
    }

    pub(crate) fn mode(&self) -> AdaptiveAdmissionMode {
        self.config.mode
    }

    pub(crate) fn distribution_headroom_enabled(&self, partition: &str) -> bool {
        self.config.distribution_headroom_max_inflight > 0
            && self
                .config
                .distribution_headroom_partitions
                .iter()
                .any(|allowed| allowed == partition)
    }

    fn current_router_loads(&self) -> HashMap<(String, u64, u64), usize> {
        self.registry
            .get_all()
            .into_iter()
            .filter(|worker| {
                worker.is_healthy()
                    && worker.generation_id() != 0
                    && *worker.worker_type() == WorkerType::Regular
                    && *worker.connection_mode() == ConnectionMode::Http
            })
            .map(|worker| {
                (
                    (
                        worker.url().to_string(),
                        worker.generation_id(),
                        worker.revision(),
                    ),
                    worker.load(),
                )
            })
            .collect()
    }

    fn adjusted_distribution_headroom(
        load: &PartitionLoad,
        current: &HashMap<(String, u64, u64), usize>,
    ) -> Option<(i64, i64)> {
        if load.worker_router_load_observations.len()
            != usize::try_from(load.headroom_healthy_replicas).ok()?
        {
            return None;
        }
        let router_delta = load.worker_router_load_observations.iter().try_fold(
            0_i64,
            |total, (identity, observed)| {
                let current = *current.get(identity)?;
                let delta = i64::try_from(current.saturating_sub(*observed)).ok()?;
                Some(total.saturating_add(delta))
            },
        )?;
        let aggregate_gap = load
            .strict_max_running_requests
            .saturating_sub(load.strict_effective_occupancy)
            .saturating_sub(router_delta)
            .max(0);
        let eligible_worker_gap =
            load.worker_headroom_requests
                .iter()
                .try_fold(0_i64, |total, (url, headroom)| {
                    let current =
                        *current.get(&(url.clone(), headroom.generation_id, headroom.revision))?;
                    let delta =
                        i64::try_from(current.saturating_sub(headroom.router_load_at_observation))
                            .ok()?;
                    Some(total.saturating_add(headroom.requests.saturating_sub(delta).max(0)))
                })?;
        Some((router_delta, aggregate_gap.min(eligible_worker_gap)))
    }

    /// Atomically reserve aggregate distribution headroom and return the exact
    /// worker generations eligible at that telemetry epoch. Cache ownership is
    /// applied later by the routing policy.
    pub(crate) fn try_acquire_distribution_headroom(
        self: &Arc<Self>,
        partition: &str,
        model: &str,
    ) -> Option<(
        DistributionHeadroomPermit,
        Vec<DistributionHeadroomCandidate>,
    )> {
        if partition != model || !self.distribution_headroom_enabled(partition) {
            return None;
        }
        let max_inflight = self.config.distribution_headroom_max_inflight;
        let current_router_loads = self.current_router_loads();
        let mut work = self.work.lock();
        let load = work.loads.get(partition)?.clone();
        if !load.headroom_is_fresh()
            || load.coverage() < 1.0
            || load.headroom_coverage() < 1.0
            || load.model_ids.len() != 1
            || !load.model_ids.contains(model)
        {
            return None;
        }
        let telemetry_epoch = load.headroom_epoch;
        let headroom = load.worker_headroom_requests.clone();
        let active = work
            .active_distribution_headroom
            .entry(partition.to_string())
            .or_default();
        let (_, adjusted_headroom) =
            Self::adjusted_distribution_headroom(&load, &current_router_loads)?;
        let available = u32::try_from(adjusted_headroom.max(0))
            .unwrap_or(u32::MAX)
            .min(u32::from(max_inflight));
        if available <= active.total {
            return None;
        }
        let candidates: Vec<_> = headroom
            .iter()
            .filter(|(url, headroom)| {
                let identity = (url.to_string(), headroom.generation_id, headroom.revision);
                let active_for_worker = active.by_worker.get(&identity).copied().unwrap_or(0);
                let current = current_router_loads
                    .get(&identity)
                    .copied()
                    .unwrap_or(usize::MAX);
                let post_observation =
                    i64::try_from(current.saturating_sub(headroom.router_load_at_observation))
                        .unwrap_or(i64::MAX);
                let adjusted_headroom = headroom.requests.saturating_sub(post_observation).max(0);
                u32::try_from(adjusted_headroom).unwrap_or(u32::MAX) > active_for_worker
            })
            .map(|(url, headroom)| DistributionHeadroomCandidate {
                worker_url: url.clone(),
                worker_generation_id: headroom.generation_id,
                worker_revision: headroom.revision,
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        active.total = active.total.saturating_add(1);
        gauge!(DISTRIBUTION_HEADROOM_ACTIVE, "partition" => intern_string(partition))
            .set(f64::from(active.total));
        Some((
            DistributionHeadroomPermit {
                controller: Arc::downgrade(self),
                partition: partition.to_string(),
                telemetry_epoch,
                target: None,
                active: true,
            },
            candidates,
        ))
    }

    fn bind_distribution_target(
        &self,
        partition: &str,
        telemetry_epoch: u64,
        target_url: &str,
        target_generation_id: u64,
        target_revision: u64,
    ) -> bool {
        let Some(worker) = self.registry.get_by_url(target_url) else {
            return false;
        };
        if target_generation_id == 0
            || worker.generation_id() != target_generation_id
            || worker.revision() != target_revision
            || !worker.is_available()
        {
            return false;
        }
        let current_router_loads = self.current_router_loads();
        let mut work = self.work.lock();
        let Some(load) = work.loads.get(partition) else {
            return false;
        };
        let Some((_, adjusted_headroom)) =
            Self::adjusted_distribution_headroom(load, &current_router_loads)
        else {
            return false;
        };
        let aggregate_limit = u32::try_from(adjusted_headroom.max(0))
            .unwrap_or(u32::MAX)
            .min(u32::from(self.config.distribution_headroom_max_inflight));
        if load.headroom_epoch != telemetry_epoch
            || !load.headroom_is_fresh()
            || load.coverage() < 1.0
            || load.headroom_coverage() < 1.0
            || load.model_ids.len() != 1
            || aggregate_limit == 0
        {
            return false;
        }
        let Some(headroom) = load.worker_headroom_requests.get(target_url) else {
            return false;
        };
        if headroom.generation_id != target_generation_id || headroom.revision != target_revision {
            return false;
        }
        if worker.generation_id() != target_generation_id
            || worker.revision() != target_revision
            || !worker.is_available()
        {
            return false;
        }
        let current_target_load = current_router_loads
            .get(&(
                target_url.to_string(),
                target_generation_id,
                target_revision,
            ))
            .copied()
            .unwrap_or(usize::MAX);
        let post_observation =
            i64::try_from(current_target_load.saturating_sub(headroom.router_load_at_observation))
                .unwrap_or(i64::MAX);
        let worker_limit = u32::try_from(headroom.requests.saturating_sub(post_observation).max(0))
            .unwrap_or(u32::MAX);
        let Some(active) = work.active_distribution_headroom.get_mut(partition) else {
            return false;
        };
        if active.total > aggregate_limit {
            return false;
        }
        let worker_active = active
            .by_worker
            .entry((
                target_url.to_string(),
                target_generation_id,
                target_revision,
            ))
            .or_default();
        if *worker_active >= worker_limit {
            return false;
        }
        *worker_active = worker_active.saturating_add(1);
        true
    }

    fn validate_distribution_target(
        &self,
        partition: &str,
        telemetry_epoch: u64,
        target_url: &str,
        target_generation_id: u64,
        target_revision: u64,
    ) -> bool {
        let current_router_loads = self.current_router_loads();
        self.validate_distribution_target_with_loads(
            partition,
            telemetry_epoch,
            target_url,
            target_generation_id,
            target_revision,
            &current_router_loads,
        )
    }

    fn validate_distribution_target_with_loads(
        &self,
        partition: &str,
        telemetry_epoch: u64,
        target_url: &str,
        target_generation_id: u64,
        target_revision: u64,
        current_router_loads: &HashMap<(String, u64, u64), usize>,
    ) -> bool {
        let work = self.work.lock();
        self.validate_distribution_target_locked(
            &work,
            partition,
            telemetry_epoch,
            (target_url, target_generation_id, target_revision),
            current_router_loads,
        )
    }

    fn validate_distribution_target_locked(
        &self,
        work: &WorkState,
        partition: &str,
        telemetry_epoch: u64,
        target: (&str, u64, u64),
        current_router_loads: &HashMap<(String, u64, u64), usize>,
    ) -> bool {
        let (target_url, target_generation_id, target_revision) = target;
        let Some(worker) = self.registry.get_by_url(target_url) else {
            return false;
        };
        if target_generation_id == 0
            || worker.generation_id() != target_generation_id
            || worker.revision() != target_revision
            || !worker.is_available()
        {
            return false;
        }
        let Some(load) = work.loads.get(partition) else {
            return false;
        };
        let Some((_, adjusted_headroom)) =
            Self::adjusted_distribution_headroom(load, current_router_loads)
        else {
            return false;
        };
        let aggregate_limit = u32::try_from(adjusted_headroom.max(0))
            .unwrap_or(u32::MAX)
            .min(u32::from(self.config.distribution_headroom_max_inflight));
        if load.headroom_epoch != telemetry_epoch
            || !load.headroom_is_fresh()
            || load.coverage() < 1.0
            || load.headroom_coverage() < 1.0
            || load.model_ids.len() != 1
            || aggregate_limit == 0
        {
            return false;
        }
        if work
            .active_distribution_headroom
            .get(partition)
            .is_none_or(|active| active.total > aggregate_limit)
        {
            return false;
        }
        let Some(headroom) = load.worker_headroom_requests.get(target_url) else {
            return false;
        };
        if headroom.generation_id != target_generation_id || headroom.revision != target_revision {
            return false;
        }
        let current_target_load = current_router_loads
            .get(&(
                target_url.to_string(),
                target_generation_id,
                target_revision,
            ))
            .copied()
            .unwrap_or(usize::MAX);
        let post_observation =
            i64::try_from(current_target_load.saturating_sub(headroom.router_load_at_observation))
                .unwrap_or(i64::MAX);
        let worker_limit = u32::try_from(headroom.requests.saturating_sub(post_observation).max(0))
            .unwrap_or(u32::MAX);
        let key = (
            target_url.to_string(),
            target_generation_id,
            target_revision,
        );
        if work
            .active_distribution_headroom
            .get(partition)
            .and_then(|active| active.by_worker.get(&key))
            .is_none_or(|active| *active > worker_limit)
        {
            return false;
        }
        self.registry.get_by_url(target_url).is_some_and(|current| {
            current.generation_id() == target_generation_id
                && current.revision() == target_revision
                && current.is_available()
        })
    }

    fn try_transfer_distribution_headroom(
        &self,
        partition: &str,
        telemetry_epoch: u64,
        target_url: &str,
        target_generation_id: u64,
        target_revision: u64,
    ) -> bool {
        let mut work = self.work.lock();
        let mut current_router_loads = self.current_router_loads();
        let Some(target_load) = current_router_loads.get_mut(&(
            target_url.to_string(),
            target_generation_id,
            target_revision,
        )) else {
            return false;
        };
        let Some(discounted) = target_load.checked_sub(1) else {
            return false;
        };
        *target_load = discounted;
        if !self.validate_distribution_target_locked(
            &work,
            partition,
            telemetry_epoch,
            (target_url, target_generation_id, target_revision),
            &current_router_loads,
        ) {
            return false;
        }
        let Some(remaining) = Self::transfer_distribution_headroom_locked(
            &mut work,
            partition,
            target_url,
            target_generation_id,
            target_revision,
        ) else {
            return false;
        };
        gauge!(DISTRIBUTION_HEADROOM_ACTIVE, "partition" => intern_string(partition))
            .set(f64::from(remaining));
        true
    }

    fn transfer_distribution_headroom_locked(
        work: &mut WorkState,
        partition: &str,
        target_url: &str,
        target_generation_id: u64,
        target_revision: u64,
    ) -> Option<u32> {
        let remaining = {
            let active = work.active_distribution_headroom.get_mut(partition)?;
            if active.total == 0 {
                return None;
            }
            let key = (
                target_url.to_string(),
                target_generation_id,
                target_revision,
            );
            let worker_active = active.by_worker.get_mut(&key)?;
            if *worker_active == 0 {
                return None;
            }
            *worker_active -= 1;
            if *worker_active == 0 {
                active.by_worker.remove(&key);
            }
            active.total -= 1;
            active.total
        };
        if remaining == 0 {
            work.active_distribution_headroom.remove(partition);
        }
        Some(remaining)
    }

    fn release_distribution_headroom(&self, partition: &str, target: Option<&(String, u64, u64)>) {
        let mut work = self.work.lock();
        let Some(active) = work.active_distribution_headroom.get_mut(partition) else {
            return;
        };
        active.total = active.total.saturating_sub(1);
        if let Some((target_url, target_generation_id, target_revision)) = target {
            let key = (target_url.clone(), *target_generation_id, *target_revision);
            if let Some(worker_active) = active.by_worker.get_mut(&key) {
                *worker_active = worker_active.saturating_sub(1);
                if *worker_active == 0 {
                    active.by_worker.remove(&key);
                }
            }
        }
        let remaining = active.total;
        if remaining == 0 {
            work.active_distribution_headroom.remove(partition);
        }
        gauge!(DISTRIBUTION_HEADROOM_ACTIVE, "partition" => intern_string(partition))
            .set(f64::from(remaining));
    }

    pub(crate) fn start_load_updates(
        self: &Arc<Self>,
        mut loads: watch::Receiver<HashMap<String, WorkerLoadResponse>>,
        mut observed_loads: watch::Receiver<HashMap<String, ObservedWorkerLoad>>,
    ) {
        self.update_loads(&loads.borrow(), &observed_loads.borrow());
        let controller = Arc::downgrade(self);
        #[expect(
            clippy::disallowed_methods,
            reason = "controller task holds only a weak reference and exits with the gateway"
        )]
        tokio::spawn(async move {
            let mut expiry = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    changed = loads.changed() => {
                        let Some(controller) = controller.upgrade() else {
                            break;
                        };
                        if changed.is_err() {
                            controller.update_loads(&HashMap::new(), &HashMap::new());
                            break;
                        }
                        controller.update_loads(&loads.borrow(), &observed_loads.borrow());
                    }
                    changed = observed_loads.changed() => {
                        let Some(controller) = controller.upgrade() else {
                            break;
                        };
                        if changed.is_err() {
                            controller.update_loads(&loads.borrow(), &HashMap::new());
                            break;
                        }
                        controller.update_loads(&loads.borrow(), &observed_loads.borrow());
                    }
                    _ = expiry.tick() => {
                        let Some(controller) = controller.upgrade() else {
                            break;
                        };
                        controller.expire_distribution_headroom();
                    }
                }
            }
        });
    }

    fn expire_distribution_headroom(&self) {
        let mut changed = false;
        let mut work = self.work.lock();
        work.headroom_epoch = work.headroom_epoch.wrapping_add(1);
        let next_epoch = work.headroom_epoch;
        for load in work.loads.values_mut() {
            if !load.headroom_is_fresh()
                && (load.issuable_headroom_requests != 0
                    || !load.worker_headroom_requests.is_empty())
            {
                load.issuable_headroom_requests = 0;
                load.worker_headroom_requests.clear();
                load.headroom_epoch = next_epoch;
                changed = true;
            }
        }
        drop(work);
        if changed {
            self.capacity_revision
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    fn update_loads(
        &self,
        loads: &HashMap<String, WorkerLoadResponse>,
        observed_loads: &HashMap<String, ObservedWorkerLoad>,
    ) {
        let now = Instant::now();
        let mut partitions: HashMap<String, PartitionLoad> = HashMap::new();
        for worker in self
            .registry
            .get_all()
            .into_iter()
            .filter(|worker| worker.is_healthy())
        {
            let partition = worker
                .metadata()
                .spec
                .labels
                .get(ADMISSION_PARTITION_LABEL)
                .map(String::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| worker.model_id())
                .to_string();
            let aggregate = partitions.entry(partition).or_default();
            aggregate.healthy_replicas = aggregate.healthy_replicas.saturating_add(1);
            let Some(load) = loads.get(worker.url()) else {
                continue;
            };
            aggregate.observed_replicas = aggregate.observed_replicas.saturating_add(1);
            aggregate.generation_tokens_per_second += load.total_gen_throughput().max(0.0);
            let running_requests = load
                .loads
                .iter()
                .map(|rank| i64::from(rank.num_running_reqs.max(0)))
                .sum::<i64>();
            let waiting_requests = load
                .loads
                .iter()
                .map(|rank| i64::from(rank.num_waiting_reqs.max(0)))
                .sum::<i64>();
            aggregate.running_requests += running_requests;
            aggregate.waiting_requests += waiting_requests;
            aggregate.waiting_uncached_tokens += load.total_waiting_uncached_tokens().max(0);
            let token_usage = load.effective_token_usage().clamp(0.0, 1.0);
            aggregate.token_usage_sum += token_usage;
            aggregate.max_token_usage = aggregate.max_token_usage.max(token_usage);
            let reported_max_running = load
                .loads
                .iter()
                .map(|rank| i64::from(rank.max_running_requests.max(0)))
                .sum::<i64>();
            let max_running_requests = if reported_max_running > 0 {
                reported_max_running
            } else {
                worker.max_running_requests().map_or(0, i64::from)
            };
            if max_running_requests > 0 {
                aggregate.max_running_requests += max_running_requests;
                aggregate.max_running_observed_replicas =
                    aggregate.max_running_observed_replicas.saturating_add(1);
            }
        }

        // Build the stricter distribution-headroom view separately so the
        // default-off feature does not change ordinary adaptive admission.
        for worker in self.registry.get_all().into_iter().filter(|worker| {
            worker.is_healthy()
                && *worker.worker_type() == WorkerType::Regular
                && *worker.connection_mode() == ConnectionMode::Http
        }) {
            let partition = worker
                .metadata()
                .spec
                .labels
                .get(ADMISSION_PARTITION_LABEL)
                .map(String::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| worker.model_id())
                .to_string();
            let aggregate = partitions.entry(partition).or_default();
            aggregate.headroom_healthy_replicas =
                aggregate.headroom_healthy_replicas.saturating_add(1);
            aggregate.model_ids.insert(worker.model_id().to_string());
            let Some(observed) = observed_loads.get(worker.url()) else {
                continue;
            };
            if !observed.authorizes_distribution_headroom() {
                continue;
            }
            let worker_generation_id = worker.generation_id();
            if worker_generation_id == 0
                || observed.worker_generation_id != worker_generation_id
                || observed.worker_revision != worker.revision()
                || observed.observed_at.elapsed() > DISTRIBUTION_HEADROOM_MAX_AGE
            {
                continue;
            }
            aggregate.observed_at = Some(
                aggregate
                    .observed_at
                    .map_or(observed.observed_at, |oldest| {
                        oldest.min(observed.observed_at)
                    }),
            );
            let Some(strict) =
                strict_worker_capacity(&observed.response, worker.max_running_requests())
            else {
                continue;
            };
            aggregate.headroom_observed_replicas =
                aggregate.headroom_observed_replicas.saturating_add(1);
            aggregate.strict_max_running_requests = aggregate
                .strict_max_running_requests
                .saturating_add(strict.max_running_requests);
            let occupancy = strict
                .running_requests
                .saturating_add(strict.waiting_requests);
            let effective_occupancy = occupancy
                .max(i64::try_from(observed.router_load_at_observation).unwrap_or(i64::MAX));
            aggregate.strict_effective_occupancy = aggregate
                .strict_effective_occupancy
                .saturating_add(effective_occupancy);
            aggregate.worker_router_load_observations.insert(
                (
                    worker.url().to_string(),
                    observed.worker_generation_id,
                    observed.worker_revision,
                ),
                observed.router_load_at_observation,
            );
            let worker_headroom = strict
                .max_running_requests
                .saturating_sub(effective_occupancy)
                .max(0);
            let waiting_limit = i64::from(
                self.config
                    .feedback_max_waiting_requests_per_healthy_replica,
            );
            if worker_headroom > 0
                && worker.is_available()
                && strict.waiting_requests <= waiting_limit
                && strict.max_pressure < self.config.feedback_max_token_usage
            {
                aggregate.worker_headroom_requests.insert(
                    worker.url().to_string(),
                    WorkerHeadroom {
                        requests: worker_headroom,
                        generation_id: observed.worker_generation_id,
                        revision: observed.worker_revision,
                        router_load_at_observation: observed.router_load_at_observation,
                    },
                );
            }
        }

        for load in partitions.values_mut() {
            let full_coverage = load.headroom_observed_replicas == load.headroom_healthy_replicas
                && load.headroom_healthy_replicas > 0
                && load.model_ids.len() == 1;
            if full_coverage {
                let aggregate_gap = load
                    .strict_max_running_requests
                    .saturating_sub(load.strict_effective_occupancy)
                    .max(0);
                let eligible_worker_gap = load
                    .worker_headroom_requests
                    .values()
                    .map(|headroom| headroom.requests)
                    .fold(0_i64, i64::saturating_add);
                load.issuable_headroom_requests = aggregate_gap.min(eligible_worker_gap);
            }
        }

        let mut work = self.work.lock();
        work.headroom_epoch = work.headroom_epoch.wrapping_add(1);
        let headroom_epoch = work.headroom_epoch;
        for load in partitions.values_mut() {
            load.headroom_epoch = headroom_epoch;
        }
        work.capacities
            .retain(|partition, _| partitions.contains_key(partition));
        work.feedback_estimates
            .retain(|partition, _| partitions.contains_key(partition));
        for (partition, load) in &mut partitions {
            if load.observed_replicas > 0 && load.generation_tokens_per_second > 0.0 {
                let per_replica =
                    load.generation_tokens_per_second / f64::from(load.observed_replicas);
                work.capacities
                    .entry(partition.clone())
                    .and_modify(|capacity| {
                        capacity.observe(per_replica, now, self.config.estimator_half_life_secs);
                    })
                    .or_insert(CapacityEstimate {
                        per_replica_tokens_per_second: per_replica,
                        last_update: now,
                    });
                if load.running_requests > 0 {
                    let running_per_replica =
                        load.running_requests as f64 / f64::from(load.observed_replicas);
                    let waiting_limit = i64::from(
                        self.config
                            .feedback_max_waiting_requests_per_healthy_replica,
                    ) * i64::from(load.observed_replicas);
                    let under_pressure = load.mean_token_usage()
                        >= self.config.feedback_max_token_usage
                        || load.waiting_requests > waiting_limit;
                    work.feedback_estimates
                        .entry(partition.clone())
                        .and_modify(|estimate| {
                            estimate.observe(
                                per_replica,
                                running_per_replica,
                                now,
                                self.config.estimator_half_life_secs,
                                self.config.feedback_throughput_improvement_ratio,
                                under_pressure,
                            );
                        })
                        .or_insert(FeedbackEstimate {
                            peak_tokens_per_second_per_replica: per_replica,
                            running_requests_per_replica_at_peak: running_per_replica,
                            pressure_observed: under_pressure,
                            last_update: now,
                        });
                }
            }
            if let Some(capacity) = work.capacities.get(partition) {
                load.learned_capacity_tokens_per_second = capacity
                    .effective(now, self.config.estimator_half_life_secs)
                    * f64::from(load.healthy_replicas);
            }
        }
        work.loads = partitions;
        for (partition, load) in &work.loads {
            let partition_label = intern_string(partition);
            gauge!(LOAD_COVERAGE, "partition" => Arc::clone(&partition_label)).set(load.coverage());
            gauge!(GEN_THROUGHPUT, "partition" => Arc::clone(&partition_label))
                .set(load.generation_tokens_per_second);
            gauge!(LEARNED_CAPACITY, "partition" => Arc::clone(&partition_label))
                .set(load.learned_capacity_tokens_per_second);
            gauge!(ENGINE_RUNNING, "partition" => Arc::clone(&partition_label))
                .set(load.running_requests as f64);
            gauge!(ENGINE_WAITING, "partition" => Arc::clone(&partition_label))
                .set(load.waiting_requests as f64);
            gauge!(ENGINE_WAITING_TOKENS, "partition" => Arc::clone(&partition_label))
                .set(load.waiting_uncached_tokens as f64);
            gauge!(ENGINE_TOKEN_USAGE, "partition" => Arc::clone(&partition_label))
                .set(load.max_token_usage);
            gauge!(ENGINE_MEAN_TOKEN_USAGE, "partition" => Arc::clone(&partition_label))
                .set(load.mean_token_usage());
            gauge!(ENGINE_MAX_RUNNING, "partition" => Arc::clone(&partition_label))
                .set(load.max_running_requests as f64);
            gauge!(ENGINE_MAX_RUNNING_COVERAGE, "partition" => Arc::clone(&partition_label))
                .set(load.max_running_coverage());
            gauge!(DISTRIBUTION_HEADROOM, "partition" => Arc::clone(&partition_label))
                .set(load.issuable_headroom_requests as f64);
            gauge!(DISTRIBUTION_HEADROOM_COVERAGE, "partition" => Arc::clone(&partition_label))
                .set(load.headroom_coverage());
            gauge!(DISTRIBUTION_HEADROOM_ACTIVE, "partition" => Arc::clone(&partition_label)).set(
                work.active_distribution_headroom
                    .get(partition)
                    .map_or(0.0, |active| f64::from(active.total)),
            );
            gauge!(FEEDBACK_KNEE_PER_REPLICA, "partition" => partition_label).set(
                work.feedback_estimates
                    .get(partition)
                    .map_or(0.0, |estimate| {
                        estimate.running_requests_per_replica_at_peak
                    }),
            );
        }
        drop(work);
        self.capacity_revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        partition: String,
        features: PredictionFeatures,
    ) -> AdaptiveRequestTracker {
        // Comet injects a trusted partition header, but standalone SMG users
        // can send arbitrary headers. Only retain a selector already known
        // from fresh worker telemetry; otherwise fall back to the request's
        // model. This keeps state and metric cardinality bounded.
        let requested_partition = partition;
        let partition = {
            let work = self.work.lock();
            if requested_partition == features.model
                || work.loads.contains_key(&requested_partition)
            {
                requested_partition.clone()
            } else {
                features.model.clone()
            }
        };
        let requested_partition_exact = requested_partition == partition;
        let now = Instant::now();
        let prediction = if self.config.strategy == AdaptiveAdmissionStrategy::PredictedWork {
            let prediction = self.predictor.lock().predict_at(&features, now);
            let model_label = intern_string(&features.model);
            counter!(
                PREDICTIONS_TOTAL,
                "model" => Arc::clone(&model_label),
                "source" => prediction.source.as_str()
            )
            .increment(1);
            histogram!(PREDICTED_OUTPUT_TOKENS, "model" => model_label)
                .record(f64::from(prediction.output_tokens));
            prediction
        } else {
            // Engine feedback does not predict completion length. Retain a
            // zero reservation only so both strategies share the same tracker
            // lifecycle without adding predictor lock contention.
            Prediction {
                output_tokens: 0,
                model_output_tokens: 0,
                source: PredictionSource::ColdStart,
            }
        };

        let decision = {
            let mut work = self.work.lock();
            let load = work.loads.get(&partition).cloned().unwrap_or_default();
            let feedback_estimate = work.feedback_estimates.get(&partition).cloned();
            let outstanding = work
                .outstanding_tokens
                .entry(partition.clone())
                .or_default();
            let prior_outstanding = *outstanding;
            *outstanding = outstanding.saturating_add(u64::from(prediction.output_tokens));
            let router_outstanding = *outstanding as f64;
            let outstanding_requests = work
                .outstanding_requests
                .entry(partition.clone())
                .or_default();
            *outstanding_requests = outstanding_requests.saturating_add(1);
            let router_outstanding_requests = *outstanding_requests as f64;
            let coverage = load.coverage();
            let engine_request_count = load
                .running_requests
                .saturating_add(load.waiting_requests)
                .max(0) as f64;
            let partition_label = intern_string(&partition);
            gauge!(ROUTER_OUTSTANDING_REQUESTS, "partition" => Arc::clone(&partition_label))
                .set(router_outstanding_requests);

            match self.config.strategy {
                AdaptiveAdmissionStrategy::PredictedWork => {
                    let telemetry_usable = coverage >= self.config.min_load_coverage
                        && load.learned_capacity_tokens_per_second.is_finite()
                        && load.learned_capacity_tokens_per_second > 0.0;
                    let budget = if telemetry_usable {
                        load.learned_capacity_tokens_per_second * self.config.work_horizon_secs
                    } else {
                        f64::INFINITY
                    };
                    let engine_estimated =
                        engine_request_count * f64::from(prediction.model_output_tokens);
                    // `max` avoids counting work both in router reservations
                    // and in a later engine poll. Include the incoming request
                    // because it is not in the engine snapshot yet.
                    let projected = router_outstanding
                        .max(engine_estimated + f64::from(prediction.output_tokens));
                    let drain_seconds = if telemetry_usable {
                        projected / load.learned_capacity_tokens_per_second
                    } else {
                        0.0
                    };
                    let would_admit =
                        !telemetry_usable || projected <= budget || prior_outstanding == 0;
                    gauge!(OUTSTANDING_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(projected);
                    gauge!(ROUTER_OUTSTANDING_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(router_outstanding);
                    gauge!(ENGINE_ESTIMATED_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(engine_estimated);
                    gauge!(WORK_BUDGET_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(if budget.is_finite() { budget } else { 0.0 });
                    gauge!(DRAIN_SECONDS, "partition" => Arc::clone(&partition_label))
                        .set(drain_seconds);
                    gauge!(FEEDBACK_RUNNING_LIMIT, "partition" => partition_label).set(0.0);
                    AdmissionDecision {
                        would_admit,
                        telemetry_usable,
                        reason: if !telemetry_usable {
                            "telemetry_fallback"
                        } else if would_admit {
                            "within_work_budget"
                        } else {
                            "work_budget"
                        },
                        retry_after_secs: if would_admit || !telemetry_usable {
                            0
                        } else {
                            ((projected - budget) / load.learned_capacity_tokens_per_second)
                                .ceil()
                                .clamp(1.0, f64::from(u32::MAX)) as u32
                        },
                    }
                }
                AdaptiveAdmissionStrategy::EngineFeedback => {
                    let constraints = engine_feedback_constraints(
                        &self.config,
                        &load,
                        feedback_estimate.as_ref(),
                    );
                    let projected_requests =
                        router_outstanding_requests.max(engine_request_count + 1.0);
                    let reason = if !constraints.telemetry_usable {
                        "telemetry_fallback"
                    } else if let Some(reason) = constraints.pressure_reason {
                        reason
                    } else if constraints
                        .running_limit
                        .is_some_and(|running_limit| projected_requests > running_limit)
                    {
                        "running_limit"
                    } else {
                        "within_feedback_limit"
                    };
                    let would_admit =
                        matches!(reason, "telemetry_fallback" | "within_feedback_limit");
                    gauge!(FEEDBACK_RUNNING_LIMIT, "partition" => Arc::clone(&partition_label))
                        .set(constraints.running_limit.unwrap_or(0.0));
                    gauge!(OUTSTANDING_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(router_outstanding);
                    gauge!(ROUTER_OUTSTANDING_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(router_outstanding);
                    gauge!(ENGINE_ESTIMATED_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(0.0);
                    gauge!(WORK_BUDGET_TOKENS, "partition" => Arc::clone(&partition_label))
                        .set(0.0);
                    gauge!(DRAIN_SECONDS, "partition" => partition_label).set(0.0);
                    AdmissionDecision {
                        would_admit,
                        telemetry_usable: constraints.telemetry_usable,
                        reason,
                        retry_after_secs: u32::from(!would_admit),
                    }
                }
            }
        };

        let outcome = if !decision.telemetry_usable {
            "telemetry_fallback"
        } else if decision.would_admit {
            "would_admit"
        } else {
            "would_reject"
        };
        counter!(
            DECISIONS_TOTAL,
            "partition" => intern_string(&partition),
            "mode" => match self.config.mode {
                AdaptiveAdmissionMode::Off => "off",
                AdaptiveAdmissionMode::Shadow => "shadow",
                AdaptiveAdmissionMode::Enforce => "enforce",
            },
            "strategy" => match self.config.strategy {
                AdaptiveAdmissionStrategy::PredictedWork => "predicted_work",
                AdaptiveAdmissionStrategy::EngineFeedback => "engine_feedback",
            },
            "reason" => decision.reason,
            "outcome" => outcome
        )
        .increment(1);

        AdaptiveRequestTracker {
            inner: Some(TrackerInner {
                controller: Arc::downgrade(self),
                partition,
                features,
                prediction,
                decision,
                requested_partition_exact,
                distribution_authorized: AtomicBool::new(false),
                resolved: AtomicBool::new(false),
            }),
        }
    }

    fn finish(&self, inner: &TrackerInner, observed_output_tokens: Option<u32>) {
        {
            let mut work = self.work.lock();
            let outstanding = work
                .outstanding_tokens
                .entry(inner.partition.clone())
                .or_default();
            *outstanding = outstanding.saturating_sub(u64::from(inner.prediction.output_tokens));
            gauge!(OUTSTANDING_TOKENS, "partition" => intern_string(&inner.partition))
                .set(*outstanding as f64);
            let outstanding_requests = work
                .outstanding_requests
                .entry(inner.partition.clone())
                .or_default();
            *outstanding_requests = outstanding_requests.saturating_sub(1);
            gauge!(ROUTER_OUTSTANDING_REQUESTS, "partition" => intern_string(&inner.partition))
                .set(*outstanding_requests as f64);
        }
        let Some(observed) = observed_output_tokens else {
            return;
        };
        if self.config.strategy == AdaptiveAdmissionStrategy::EngineFeedback {
            return;
        }
        self.predictor
            .lock()
            .observe_at(&inner.features, observed, Instant::now());
        let model_label = intern_string(&inner.features.model);
        histogram!(OBSERVED_OUTPUT_TOKENS, "model" => Arc::clone(&model_label))
            .record(f64::from(observed));
        histogram!(ABSOLUTE_ERROR_TOKENS, "model" => model_label)
            .record((f64::from(observed) - f64::from(inner.prediction.output_tokens)).abs());
        gauge!(SEGMENTS).set(self.predictor.lock().segments.len() as f64);

        let Some(samples) = &self.prediction_samples else {
            return;
        };
        if samples.lock().should_record(&inner.features.model) {
            tracing::info!(
                target: "smg::adaptive_admission_prediction_sample",
                model = %inner.features.model,
                predicted_output_tokens = inner.prediction.output_tokens,
                observed_output_tokens = observed,
                prediction_source = inner.prediction.source.as_str(),
                prompt_tokens = inner.features.prompt_tokens,
                output_limit_tokens = inner.features.max_output_tokens.unwrap_or(0),
                generation_flags = inner.features.generation_flags,
                "adaptive admission prediction sample"
            );
        }
    }
}

impl AdaptiveCapacityProvider for AdaptiveAdmissionController {
    fn effective_capacity(&self, partition: &str, static_capacity: u16) -> u16 {
        if self.config.mode != AdaptiveAdmissionMode::Enforce
            || self.config.strategy != AdaptiveAdmissionStrategy::EngineFeedback
        {
            return static_capacity;
        }
        let distribution_enabled = self.distribution_headroom_enabled(partition);
        let current_router_loads = distribution_enabled.then(|| self.current_router_loads());
        let work = self.work.lock();
        let load = work.loads.get(partition).cloned().unwrap_or_default();
        let constraints = engine_feedback_constraints(
            &self.config,
            &load,
            work.feedback_estimates.get(partition),
        );
        if !constraints.telemetry_usable {
            return if distribution_enabled {
                0
            } else {
                static_capacity
            };
        }
        let ordinary_capacity = if constraints.pressure_reason.is_some() {
            0
        } else if let Some(running_limit) = constraints.running_limit {
            (running_limit.floor().clamp(0.0, f64::from(u16::MAX)) as u16).min(static_capacity)
        } else {
            static_capacity
        };
        let distribution_can_raise_capacity = distribution_enabled
            && (constraints.pressure_reason == Some("engine_waiting")
                || (constraints.pressure_reason.is_none() && constraints.running_limit.is_some()));
        if distribution_can_raise_capacity {
            let Some(current_router_loads) = current_router_loads.as_ref() else {
                return ordinary_capacity;
            };
            if !load.headroom_is_fresh()
                || load.coverage() < 1.0
                || load.headroom_coverage() < 1.0
                || load.model_ids.len() != 1
                || !load.model_ids.contains(partition)
            {
                return ordinary_capacity;
            }
            let Some((router_delta, adjusted_headroom)) =
                Self::adjusted_distribution_headroom(&load, current_router_loads)
            else {
                return ordinary_capacity;
            };
            // The scheduler capacity is an absolute ceiling, not a count of
            // newly issuable credits. Expose at most the configured number of
            // distribution transitions above the strict observed occupancy.
            // Otherwise a single idle replica with a large gap could let the
            // allocator mint that entire gap while the routing layer can hold
            // only `distribution_headroom_max_inflight` permits, turning the
            // remainder into local 429/refund churn.
            let route_limit =
                adjusted_headroom.min(i64::from(self.config.distribution_headroom_max_inflight));
            let active = i64::from(
                work.active_distribution_headroom
                    .get(partition)
                    .map_or(0, |active| active.total),
            );
            if route_limit == 0 || active > route_limit {
                return ordinary_capacity;
            }
            let remaining = route_limit.saturating_sub(active);
            let bounded_headroom = active.saturating_add(remaining);
            let safe_total = load
                .strict_effective_occupancy
                .saturating_add(router_delta)
                .saturating_add(bounded_headroom)
                .clamp(0, i64::from(u16::MAX)) as u16;
            let distribution_capacity = safe_total
                .min(
                    load.strict_max_running_requests
                        .clamp(0, i64::from(u16::MAX)) as u16,
                )
                .min(static_capacity);
            return ordinary_capacity.max(distribution_capacity);
        }
        ordinary_capacity
    }

    fn subscribe_capacity_changes(&self) -> watch::Receiver<u64> {
        self.capacity_revision.subscribe()
    }
}

#[derive(Debug, Clone, Copy)]
struct AdmissionDecision {
    would_admit: bool,
    telemetry_usable: bool,
    reason: &'static str,
    retry_after_secs: u32,
}

struct TrackerInner {
    controller: Weak<AdaptiveAdmissionController>,
    partition: String,
    features: PredictionFeatures,
    prediction: Prediction,
    decision: AdmissionDecision,
    requested_partition_exact: bool,
    distribution_authorized: AtomicBool,
    resolved: AtomicBool,
}

/// Per-request adaptive-admission state. Dropping an unfinished tracker
/// releases its request reservation. Predicted-work mode also releases its
/// token reservation without teaching the estimator from a partial or failed
/// response.
pub(crate) struct AdaptiveRequestTracker {
    inner: Option<TrackerInner>,
}

impl AdaptiveRequestTracker {
    /// Return the exact configured partition only for an enforced
    /// engine-feedback rejection that the distribution path is allowed to
    /// challenge. Unknown trusted selectors never fall back into this path.
    pub(crate) fn distribution_headroom_partition(&self) -> Option<&str> {
        let inner = self.inner.as_ref()?;
        let controller = inner.controller.upgrade()?;
        (controller.mode() == AdaptiveAdmissionMode::Enforce
            && controller.config.strategy == AdaptiveAdmissionStrategy::EngineFeedback
            && inner.requested_partition_exact
            && inner.partition == inner.features.model
            && inner.decision.telemetry_usable
            && !inner.decision.would_admit
            && distribution_challenge_reason(inner.decision.reason)
            && controller.distribution_headroom_enabled(&inner.partition))
        .then_some(inner.partition.as_str())
    }

    /// Authorize the already-proved exact route exactly once. This only
    /// changes the local adaptive decision; it does not create scheduler or
    /// worker capacity and must be called after those independent proofs.
    pub(crate) fn authorize_distribution_headroom(&self) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        if self.distribution_headroom_partition().is_none()
            || inner
                .distribution_authorized
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        counter!(
            DISTRIBUTION_OVERRIDES_TOTAL,
            "partition" => intern_string(&inner.partition),
            "reason" => inner.decision.reason,
        )
        .increment(1);
        true
    }

    pub(crate) fn should_reject(&self) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        inner.controller.upgrade().is_some_and(|controller| {
            controller.mode() == AdaptiveAdmissionMode::Enforce
                && inner.decision.telemetry_usable
                && !inner.decision.would_admit
                && !inner.distribution_authorized.load(Ordering::Acquire)
        })
    }

    pub(crate) fn retry_after_secs(&self) -> u32 {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.decision.retry_after_secs)
    }

    pub(crate) fn complete(mut self, observed_output_tokens: u32) {
        self.resolve(Some(observed_output_tokens));
    }

    fn resolve(&mut self, observed_output_tokens: Option<u32>) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        if inner.resolved.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(controller) = inner.controller.upgrade() {
            controller.finish(&inner, observed_output_tokens);
        }
    }
}

impl Drop for AdaptiveRequestTracker {
    fn drop(&mut self) {
        self.resolve(None);
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Barrier, time::Duration};

    use openai_protocol::{
        model_card::ModelCard,
        worker::{HealthCheckConfig, SchedulerLoadSnapshot},
    };

    use super::*;
    use crate::worker::{monitor::WorkerLoadSource, BasicWorkerBuilder, Worker, WorkerLoadGuard};

    fn config() -> AdaptiveAdmissionConfig {
        AdaptiveAdmissionConfig {
            mode: AdaptiveAdmissionMode::Shadow,
            strategy: AdaptiveAdmissionStrategy::PredictedWork,
            work_horizon_secs: 10.0,
            estimator_half_life_secs: 60.0,
            prior_observations: 2.0,
            max_segments: 20,
            min_load_coverage: 0.8,
            cold_start_output_tokens: 100,
            feedback_probe_requests_per_healthy_replica: 2,
            feedback_max_waiting_requests_per_healthy_replica: 2,
            feedback_max_token_usage: 0.9,
            feedback_throughput_improvement_ratio: 0.02,
            distribution_headroom_partitions: Vec::new(),
            distribution_headroom_max_inflight: 0,
        }
    }

    fn features(user: &str, prompt_tokens: u32, maximum: Option<u32>) -> PredictionFeatures {
        PredictionFeatures {
            model: "model".to_string(),
            user: user.to_string(),
            workload_type: "rollout".to_string(),
            endpoint: "chat",
            prompt_tokens,
            max_output_tokens: maximum,
            generation_flags: 0,
        }
    }

    fn distribution_controller_with_max_inflight(
        max_inflight: u16,
    ) -> (
        Arc<AdaptiveAdmissionController>,
        Arc<dyn Worker>,
        Arc<dyn Worker>,
    ) {
        let registry = Arc::new(WorkerRegistry::new());
        let no_health_check = HealthCheckConfig {
            disable_health_check: true,
            ..Default::default()
        };
        for url in ["http://hot:8000", "http://idle:8000"] {
            registry.register(Arc::new(
                BasicWorkerBuilder::new(url)
                    .model(ModelCard::new("model"))
                    .health_config(no_health_check.clone())
                    .label(ADMISSION_PARTITION_LABEL, "model")
                    .build(),
            ));
        }
        let hot = registry.get_by_url("http://hot:8000").unwrap();
        let idle = registry.get_by_url("http://idle:8000").unwrap();
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        settings.distribution_headroom_partitions = vec!["model".to_string()];
        settings.distribution_headroom_max_inflight = max_inflight;
        let controller = AdaptiveAdmissionController::new(settings, registry);
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                model_ids: HashSet::from(["model".to_string()]),
                healthy_replicas: 2,
                observed_replicas: 2,
                running_requests: 38,
                waiting_requests: 36,
                token_usage_sum: 1.0,
                max_token_usage: 0.5,
                max_running_requests: 76,
                max_running_observed_replicas: 2,
                strict_max_running_requests: 76,
                strict_effective_occupancy: 74,
                headroom_healthy_replicas: 2,
                headroom_observed_replicas: 2,
                issuable_headroom_requests: 2,
                worker_headroom_requests: HashMap::from([(
                    idle.url().to_string(),
                    WorkerHeadroom {
                        requests: 2,
                        generation_id: idle.generation_id(),
                        revision: idle.revision(),
                        router_load_at_observation: 0,
                    },
                )]),
                worker_router_load_observations: HashMap::from([
                    (
                        (hot.url().to_string(), hot.generation_id(), hot.revision()),
                        0,
                    ),
                    (
                        (
                            idle.url().to_string(),
                            idle.generation_id(),
                            idle.revision(),
                        ),
                        0,
                    ),
                ]),
                headroom_epoch: 1,
                observed_at: Some(Instant::now()),
                ..PartitionLoad::default()
            },
        );
        (controller, hot, idle)
    }

    fn provenance_controller() -> (
        Arc<AdaptiveAdmissionController>,
        Arc<dyn Worker>,
        WorkerLoadResponse,
    ) {
        let registry = Arc::new(WorkerRegistry::new());
        registry
            .register(Arc::new(
                BasicWorkerBuilder::new("http://worker:8000")
                    .model(ModelCard::new("model"))
                    .health_config(HealthCheckConfig {
                        disable_health_check: true,
                        ..Default::default()
                    })
                    .label(ADMISSION_PARTITION_LABEL, "model")
                    .build(),
            ))
            .unwrap();
        let worker = registry.get_by_url("http://worker:8000").unwrap();
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        settings.distribution_headroom_partitions = vec!["model".to_string()];
        settings.distribution_headroom_max_inflight = 1;
        let response = WorkerLoadResponse {
            dp_rank_count: 1,
            loads: vec![SchedulerLoadSnapshot {
                dp_rank: 0,
                token_usage: 0.1,
                utilization: 0.1,
                max_running_requests: 2,
                ..Default::default()
            }],
            ..Default::default()
        };
        (
            AdaptiveAdmissionController::new(settings, registry),
            worker,
            response,
        )
    }

    fn distribution_controller() -> (
        Arc<AdaptiveAdmissionController>,
        Arc<dyn Worker>,
        Arc<dyn Worker>,
    ) {
        distribution_controller_with_max_inflight(8)
    }

    #[test]
    fn prediction_samples_skip_warmup_and_cap_each_model() {
        let mut samples = PredictionSampleState {
            skip_per_model: 2,
            limit_per_model: 2,
            seen_per_model: HashMap::new(),
        };

        assert!(!samples.should_record("model-a"));
        assert!(!samples.should_record("model-a"));
        assert!(samples.should_record("model-a"));
        assert!(samples.should_record("model-a"));
        assert!(!samples.should_record("model-a"));
        assert!(!samples.should_record("model-b"));
        assert!(!samples.should_record("model-b"));
        assert!(samples.should_record("model-b"));
    }

    #[test]
    fn cold_start_is_clamped_by_request_limit() {
        let predictor = HierarchicalPredictor::new(&config());
        let prediction = predictor.predict_at(&features("u", 10, Some(32)), Instant::now());
        assert_eq!(prediction.output_tokens, 32);
        assert_eq!(prediction.source, PredictionSource::ColdStart);
    }

    #[test]
    fn user_history_beats_model_mean_and_decays() {
        let mut predictor = HierarchicalPredictor::new(&config());
        let start = Instant::now();
        let user_a = features("a", 1000, None);
        let user_b = features("b", 1000, None);
        for i in 0..20 {
            let now = start + Duration::from_secs(i);
            predictor.observe_at(&user_a, 20, now);
            predictor.observe_at(&user_b, 200, now);
        }
        let prediction = predictor.predict_at(&user_a, start + Duration::from_secs(21));
        assert!(prediction.output_tokens < 80, "{prediction:?}");
        assert!(matches!(
            prediction.source,
            PredictionSource::UserWorkload | PredictionSource::Full
        ));

        predictor.observe_at(&user_a, 400, start + Duration::from_secs(600));
        let shifted = predictor.predict_at(&user_a, start + Duration::from_secs(601));
        assert!(shifted.output_tokens > prediction.output_tokens);
    }

    #[test]
    fn estimator_state_is_bounded() {
        let mut predictor = HierarchicalPredictor::new(&config());
        let start = Instant::now();
        for i in 0..100 {
            predictor.observe_at(
                &features(&format!("user-{i}"), i + 1, None),
                i + 1,
                start + Duration::from_secs(u64::from(i)),
            );
        }
        assert!(predictor.segments.len() <= predictor.max_segments);
        assert!(predictor.segments.len() >= predictor.max_segments / 2);
    }

    #[test]
    fn learned_capacity_tracks_recent_peak_and_decays() {
        let start = Instant::now();
        let mut capacity = CapacityEstimate {
            per_replica_tokens_per_second: 100.0,
            last_update: start,
        };
        capacity.observe(40.0, start + Duration::from_secs(60), 60.0);
        assert!((capacity.per_replica_tokens_per_second - 50.0).abs() < f64::EPSILON);
        capacity.observe(80.0, start + Duration::from_secs(61), 60.0);
        assert!((capacity.per_replica_tokens_per_second - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_partition_header_falls_back_to_model() {
        let controller =
            AdaptiveAdmissionController::new(config(), Arc::new(WorkerRegistry::new()));
        let tracker = controller.begin("attacker-controlled".to_string(), features("a", 10, None));
        assert_eq!(tracker.inner.as_ref().unwrap().partition, "model");
    }

    #[test]
    fn work_horizon_decision_uses_throughput_not_request_count() {
        let registry = Arc::new(WorkerRegistry::new());
        let controller = AdaptiveAdmissionController::new(config(), registry);
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 1,
                observed_replicas: 1,
                generation_tokens_per_second: 10.0,
                learned_capacity_tokens_per_second: 10.0,
                ..PartitionLoad::default()
            },
        );

        let first = controller.begin("model".to_string(), features("a", 10, None));
        assert!(!first.should_reject(), "shadow mode never rejects");
        let second = controller.begin("model".to_string(), features("b", 10, None));
        assert!(!second.inner.as_ref().unwrap().decision.would_admit);
        drop(second);
        drop(first);
        assert_eq!(
            controller.work.lock().outstanding_tokens.get("model"),
            Some(&0)
        );
    }

    #[test]
    fn enforce_rejects_only_after_live_work_budget_is_exhausted() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 1,
                observed_replicas: 1,
                generation_tokens_per_second: 10.0,
                learned_capacity_tokens_per_second: 10.0,
                ..PartitionLoad::default()
            },
        );

        let first = controller.begin("model".to_string(), features("a", 10, None));
        assert!(!first.should_reject());
        let second = controller.begin("model".to_string(), features("b", 10, None));
        assert!(second.should_reject());
        assert_eq!(second.retry_after_secs(), 10);
    }

    #[test]
    fn enforce_fails_open_when_engine_load_coverage_is_incomplete() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 2,
                observed_replicas: 1,
                generation_tokens_per_second: 10.0,
                learned_capacity_tokens_per_second: 20.0,
                ..PartitionLoad::default()
            },
        );

        let first = controller.begin("model".to_string(), features("a", 10, None));
        let second = controller.begin("model".to_string(), features("b", 10, None));
        assert!(!first.should_reject());
        assert!(!second.should_reject());
        assert!(!second.inner.as_ref().unwrap().decision.telemetry_usable);
    }

    #[test]
    fn engine_backlog_survives_router_reservation_loss() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.work_horizon_secs = 30.0;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 1,
                observed_replicas: 1,
                generation_tokens_per_second: 10.0,
                learned_capacity_tokens_per_second: 10.0,
                running_requests: 3,
                ..PartitionLoad::default()
            },
        );

        let starvation_probe = controller.begin("model".to_string(), features("a", 10, None));
        assert!(!starvation_probe.should_reject());
        let next = controller.begin("model".to_string(), features("b", 10, None));
        assert!(next.should_reject());
    }

    #[test]
    fn engine_feedback_uses_learned_knee_plus_probe_margin() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        let now = Instant::now();
        let mut work = controller.work.lock();
        work.loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 1,
                observed_replicas: 1,
                generation_tokens_per_second: 100.0,
                running_requests: 10,
                max_running_requests: 64,
                max_running_observed_replicas: 1,
                max_token_usage: 0.5,
                ..PartitionLoad::default()
            },
        );
        work.feedback_estimates.insert(
            "model".to_string(),
            FeedbackEstimate {
                peak_tokens_per_second_per_replica: 100.0,
                running_requests_per_replica_at_peak: 10.0,
                pressure_observed: false,
                last_update: now,
            },
        );
        drop(work);

        let trackers: Vec<_> = (0..12)
            .map(|i| controller.begin("model".to_string(), features(&format!("u-{i}"), 10, None)))
            .collect();
        assert!(trackers.iter().all(|tracker| !tracker.should_reject()));
        let excess = controller.begin("model".to_string(), features("excess", 10, None));
        assert!(excess.should_reject());
        assert_eq!(excess.retry_after_secs(), 1);
    }

    #[test]
    fn engine_feedback_backs_off_on_waiting_or_token_pressure() {
        for (waiting_requests, token_usage, expected_reason) in
            [(3, 0.5, "engine_waiting"), (0, 0.91, "token_pressure")]
        {
            let mut settings = config();
            settings.mode = AdaptiveAdmissionMode::Enforce;
            settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
            let controller =
                AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
            controller.work.lock().loads.insert(
                "model".to_string(),
                PartitionLoad {
                    healthy_replicas: 1,
                    observed_replicas: 1,
                    generation_tokens_per_second: 100.0,
                    running_requests: 1,
                    waiting_requests,
                    max_running_requests: 64,
                    max_running_observed_replicas: 1,
                    max_token_usage: token_usage,
                    token_usage_sum: token_usage,
                    ..PartitionLoad::default()
                },
            );

            let tracker = controller.begin("model".to_string(), features("u", 10, None));
            assert!(tracker.should_reject());
            assert_eq!(
                tracker.inner.as_ref().unwrap().decision.reason,
                expected_reason
            );
        }
    }

    #[test]
    fn engine_feedback_cold_start_is_bounded_by_engine_limit() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 1,
                observed_replicas: 1,
                max_running_requests: 2,
                max_running_observed_replicas: 1,
                ..PartitionLoad::default()
            },
        );

        let first = controller.begin("model".to_string(), features("a", 10, None));
        let second = controller.begin("model".to_string(), features("b", 10, None));
        let third = controller.begin("model".to_string(), features("c", 10, None));
        assert!(!first.should_reject());
        assert!(!second.should_reject());
        assert!(third.should_reject());
    }

    #[test]
    fn engine_feedback_scales_only_sufficient_max_running_coverage() {
        let sufficiently_covered = PartitionLoad {
            healthy_replicas: 5,
            observed_replicas: 4,
            max_running_requests: 256,
            max_running_observed_replicas: 4,
            ..PartitionLoad::default()
        };
        assert_eq!(sufficiently_covered.coverage(), 0.8);
        assert_eq!(sufficiently_covered.max_running_coverage(), 0.8);
        assert_eq!(sufficiently_covered.scaled_max_running_requests(), 320.0);

        let insufficiently_covered = PartitionLoad {
            healthy_replicas: 5,
            observed_replicas: 5,
            max_running_requests: 192,
            max_running_observed_replicas: 3,
            ..PartitionLoad::default()
        };
        assert_eq!(insufficiently_covered.coverage(), 1.0);
        assert_eq!(insufficiently_covered.max_running_coverage(), 0.6);
    }

    #[test]
    fn engine_feedback_uses_load_when_max_running_coverage_is_incomplete() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 5,
                observed_replicas: 5,
                max_running_requests: 192,
                max_running_observed_replicas: 3,
                ..PartitionLoad::default()
            },
        );

        let tracker = controller.begin("model".to_string(), features("u", 10, None));
        assert!(!tracker.should_reject());
        assert!(tracker.inner.as_ref().unwrap().decision.telemetry_usable);
        assert_eq!(
            tracker.inner.as_ref().unwrap().decision.reason,
            "within_feedback_limit"
        );
    }

    #[test]
    fn engine_feedback_enforces_pressure_without_max_running_limit() {
        for (waiting_requests, token_usage, expected_reason) in
            [(3, 0.5, "engine_waiting"), (0, 0.91, "token_pressure")]
        {
            let mut settings = config();
            settings.mode = AdaptiveAdmissionMode::Enforce;
            settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
            let controller =
                AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
            controller.work.lock().loads.insert(
                "model".to_string(),
                PartitionLoad {
                    healthy_replicas: 1,
                    observed_replicas: 1,
                    running_requests: 10,
                    waiting_requests,
                    max_token_usage: token_usage,
                    token_usage_sum: token_usage,
                    ..PartitionLoad::default()
                },
            );

            let tracker = controller.begin("model".to_string(), features("u", 10, None));
            assert!(tracker.should_reject());
            assert!(tracker.inner.as_ref().unwrap().decision.telemetry_usable);
            assert_eq!(
                tracker.inner.as_ref().unwrap().decision.reason,
                expected_reason
            );
        }
    }

    #[test]
    fn engine_feedback_ignores_idle_knee_without_max_running_limit() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        let now = Instant::now();
        let mut work = controller.work.lock();
        work.loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 4,
                observed_replicas: 4,
                ..PartitionLoad::default()
            },
        );
        work.feedback_estimates.insert(
            "model".to_string(),
            FeedbackEstimate {
                peak_tokens_per_second_per_replica: 0.001,
                running_requests_per_replica_at_peak: 0.25,
                pressure_observed: false,
                last_update: now,
            },
        );
        drop(work);

        let trackers: Vec<_> = (0..20)
            .map(|i| controller.begin("model".to_string(), features(&format!("u-{i}"), 10, None)))
            .collect();
        assert!(trackers.iter().all(|tracker| !tracker.should_reject()));
    }

    #[test]
    fn engine_feedback_uses_busy_knee_without_max_running_limit() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        let now = Instant::now();
        let mut work = controller.work.lock();
        work.loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 1,
                observed_replicas: 1,
                running_requests: 10,
                ..PartitionLoad::default()
            },
        );
        work.feedback_estimates.insert(
            "model".to_string(),
            FeedbackEstimate {
                peak_tokens_per_second_per_replica: 100.0,
                running_requests_per_replica_at_peak: 10.0,
                pressure_observed: true,
                last_update: now,
            },
        );
        drop(work);

        let trackers: Vec<_> = (0..12)
            .map(|i| controller.begin("model".to_string(), features(&format!("u-{i}"), 10, None)))
            .collect();
        assert!(trackers.iter().all(|tracker| !tracker.should_reject()));
        let excess = controller.begin("model".to_string(), features("excess", 10, None));
        assert!(excess.should_reject());
        assert_eq!(
            excess.inner.as_ref().unwrap().decision.reason,
            "running_limit"
        );
    }

    #[test]
    fn capacity_provider_reports_total_ceiling_without_request_double_accounting() {
        let mut settings = config();
        settings.mode = AdaptiveAdmissionMode::Enforce;
        settings.strategy = AdaptiveAdmissionStrategy::EngineFeedback;
        let controller =
            AdaptiveAdmissionController::new(settings, Arc::new(WorkerRegistry::new()));
        controller.work.lock().loads.insert(
            "model".to_string(),
            PartitionLoad {
                healthy_replicas: 1,
                observed_replicas: 1,
                running_requests: 63,
                max_running_requests: 64,
                max_running_observed_replicas: 1,
                token_usage_sum: 0.5,
                max_token_usage: 0.5,
                ..PartitionLoad::default()
            },
        );

        assert_eq!(controller.effective_capacity("model", 100), 64);
        controller
            .work
            .lock()
            .loads
            .get_mut("model")
            .unwrap()
            .token_usage_sum = 0.95;
        assert_eq!(controller.effective_capacity("model", 100), 0);
    }

    #[test]
    fn strict_distribution_math_exposes_exactly_two_of_seventy_six_slots() {
        let (controller, _hot, idle) = distribution_controller();

        assert_eq!(controller.effective_capacity("model", 100), 76);
        let (mut first, first_candidates) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert_eq!(first_candidates.len(), 1);
        assert_eq!(first_candidates[0].worker_url, idle.url());
        assert!(first.bind_target(idle.url(), idle.generation_id(), idle.revision()));

        let (mut second, second_candidates) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert_eq!(second_candidates.len(), 1);
        assert!(second.bind_target(idle.url(), idle.generation_id(), idle.revision()));
        assert!(controller
            .try_acquire_distribution_headroom("model", "model")
            .is_none());

        // Transfer each adaptive reservation to the ordinary worker load
        // accounting before dispatch. At every step the next slot remains
        // visible exactly once, never zero times or twice.
        assert!(first.validate_for_dispatch());
        let first_guard = WorkerLoadGuard::new(idle.clone(), None);
        assert!(first.try_transfer_after_worker_load_reserved());
        assert!(second.validate_for_dispatch());
        let second_guard = WorkerLoadGuard::new(idle.clone(), None);
        assert!(second.try_transfer_after_worker_load_reserved());
        assert!(controller
            .try_acquire_distribution_headroom("model", "model")
            .is_none());

        drop(first_guard);
        assert!(controller
            .try_acquire_distribution_headroom("model", "model")
            .is_some());
        drop(second_guard);
    }

    #[test]
    fn distribution_capacity_never_advertises_more_than_transition_limit() {
        let (controller, _hot, idle) = distribution_controller_with_max_inflight(1);

        // Strict occupancy is 74 and clean-peer telemetry exposes two slots,
        // but the scheduler may issue only one transition credit at a time.
        assert_eq!(controller.effective_capacity("model", 100), 75);
        let (permit, candidates) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].worker_url, idle.url());
        assert_eq!(
            controller.effective_capacity("model", 100),
            75,
            "a held route permit is already inside the absolute scheduler ceiling"
        );
        assert!(controller
            .try_acquire_distribution_headroom("model", "model")
            .is_none());

        drop(permit);
        assert!(controller
            .try_acquire_distribution_headroom("model", "model")
            .is_some());
    }

    #[test]
    fn distribution_capacity_never_bypasses_token_pressure() {
        let (controller, _hot, _idle) = distribution_controller_with_max_inflight(1);
        {
            let mut work = controller.work.lock();
            let load = work.loads.get_mut("model").unwrap();
            load.token_usage_sum = 1.9;
            load.max_token_usage = 0.95;
        }

        assert_eq!(controller.effective_capacity("model", 100), 0);
        let tracker = controller.begin("model".to_string(), features("u", 10, None));
        assert!(tracker.should_reject());
        assert_eq!(
            tracker.inner.as_ref().unwrap().decision.reason,
            "token_pressure"
        );
        assert_eq!(tracker.distribution_headroom_partition(), None);
    }

    #[test]
    fn distribution_capacity_never_raises_without_an_eligible_clean_peer() {
        let (waiting, _hot, _idle) = distribution_controller_with_max_inflight(1);
        waiting
            .work
            .lock()
            .loads
            .get_mut("model")
            .unwrap()
            .worker_headroom_requests
            .clear();
        assert_eq!(waiting.effective_capacity("model", 100), 0);

        let (limited, _hot, _idle) = distribution_controller_with_max_inflight(1);
        {
            let mut work = limited.work.lock();
            let load = work.loads.get_mut("model").unwrap();
            load.running_requests = 40;
            load.waiting_requests = 0;
            load.strict_effective_occupancy = 74;
            load.worker_headroom_requests.clear();
            work.feedback_estimates.insert(
                "model".to_string(),
                FeedbackEstimate {
                    peak_tokens_per_second_per_replica: 100.0,
                    running_requests_per_replica_at_peak: 18.0,
                    pressure_observed: true,
                    last_update: Instant::now(),
                },
            );
        }
        assert_eq!(limited.effective_capacity("model", 100), 40);
    }

    #[test]
    fn distribution_capacity_raises_learned_limit_only_at_the_boundary() {
        let (controller, _hot, _idle) = distribution_controller_with_max_inflight(1);
        {
            let mut work = controller.work.lock();
            let load = work.loads.get_mut("model").unwrap();
            load.running_requests = 40;
            load.waiting_requests = 0;
            load.strict_effective_occupancy = 40;
            work.feedback_estimates.insert(
                "model".to_string(),
                FeedbackEstimate {
                    peak_tokens_per_second_per_replica: 100.0,
                    running_requests_per_replica_at_peak: 18.0,
                    pressure_observed: true,
                    last_update: Instant::now(),
                },
            );
        }

        // Learned ordinary limit is 18*2 + 2*2 = 40. At that exact
        // boundary, one verified clean-peer transition raises it to 41 and
        // the corresponding request is challengeable for `running_limit`.
        assert_eq!(controller.effective_capacity("model", 100), 41);
        let tracker = controller.begin("model".to_string(), features("u", 10, None));
        assert!(tracker.should_reject());
        assert_eq!(
            tracker.inner.as_ref().unwrap().decision.reason,
            "running_limit"
        );
        assert_eq!(tracker.distribution_headroom_partition(), Some("model"));

        let (below, _hot, _idle) = distribution_controller_with_max_inflight(1);
        {
            let mut work = below.work.lock();
            let load = work.loads.get_mut("model").unwrap();
            load.running_requests = 20;
            load.waiting_requests = 0;
            load.strict_effective_occupancy = 20;
            work.feedback_estimates.insert(
                "model".to_string(),
                FeedbackEstimate {
                    peak_tokens_per_second_per_replica: 100.0,
                    running_requests_per_replica_at_peak: 18.0,
                    pressure_observed: true,
                    last_update: Instant::now(),
                },
            );
        }
        assert_eq!(below.effective_capacity("model", 100), 40);
        let ordinary = below.begin("model".to_string(), features("u", 10, None));
        assert!(!ordinary.should_reject());
        assert_eq!(ordinary.distribution_headroom_partition(), None);
    }

    #[test]
    fn distribution_permit_rejects_same_url_re_registration_aba() {
        let (controller, _hot, idle) = distribution_controller();
        let (mut permit, candidates) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        let candidate = candidates.first().unwrap();
        let target_url = idle.url().to_string();
        let old_generation_id = candidate.worker_generation_id;
        let old_revision = candidate.worker_revision;
        assert_eq!(old_generation_id, idle.generation_id());
        assert_eq!(old_revision, idle.revision());

        assert!(controller.registry.remove_by_url(&target_url).is_some());
        let replacement: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new(&target_url)
                .model(ModelCard::new("model"))
                .health_config(HealthCheckConfig {
                    disable_health_check: true,
                    ..Default::default()
                })
                .label(ADMISSION_PARTITION_LABEL, "model")
                .build(),
        );
        assert_eq!(replacement.revision(), old_revision);
        assert_ne!(replacement.generation_id(), old_generation_id);
        assert!(controller.registry.register(replacement.clone()).is_some());

        // URL and revision have both returned to their old values, but neither
        // the stale generation nor the unobserved replacement may be bound.
        assert!(!permit.bind_target(&target_url, old_generation_id, old_revision));
        assert!(!permit.bind_target(
            &target_url,
            replacement.generation_id(),
            replacement.revision(),
        ));
    }

    #[test]
    fn distribution_transfer_discounts_exactly_the_new_worker_guard() {
        let (controller, _hot, idle) = distribution_controller();
        let (mut first, _) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert!(first.bind_target(idle.url(), idle.generation_id(), idle.revision()));
        let (mut second, _) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert!(second.bind_target(idle.url(), idle.generation_id(), idle.revision()));

        // One unrelated ordinary request plus this caller's newly-created guard
        // consume both observed slots. Discounting only the caller still leaves
        // one unit of router-local load, so two adaptive permits cannot transfer.
        let unrelated_guard = WorkerLoadGuard::new(idle.clone(), None);
        let caller_guard = WorkerLoadGuard::new(idle.clone(), None);
        assert!(!first.try_transfer_after_worker_load_reserved());
        assert_eq!(
            controller
                .work
                .lock()
                .active_distribution_headroom
                .get("model")
                .unwrap()
                .total,
            1
        );

        drop(second);
        drop(caller_guard);
        drop(unrelated_guard);
        assert!(!controller
            .work
            .lock()
            .active_distribution_headroom
            .contains_key("model"));
    }

    #[test]
    fn distribution_transfer_fails_closed_when_telemetry_is_revoked_while_waiting() {
        let (controller, _hot, idle) = distribution_controller();
        let (mut permit, _) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert!(permit.bind_target(idle.url(), idle.generation_id(), idle.revision()));

        let mut work = controller.work.lock();
        let barrier = Arc::new(Barrier::new(2));
        let thread_barrier = Arc::clone(&barrier);
        let thread_idle = idle.clone();
        let handle = std::thread::spawn(move || {
            let guard = WorkerLoadGuard::new(thread_idle, None);
            thread_barrier.wait();
            let transferred = permit.try_transfer_after_worker_load_reserved();
            drop(guard);
            transferred
        });
        barrier.wait();
        work.loads.get_mut("model").unwrap().headroom_epoch += 1;
        drop(work);

        assert!(!handle.join().unwrap());
        assert!(!controller
            .work
            .lock()
            .active_distribution_headroom
            .contains_key("model"));
    }

    #[test]
    fn distribution_transfer_fails_closed_when_target_is_revoked_while_waiting() {
        let (controller, _hot, idle) = distribution_controller();
        let (mut permit, _) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert!(permit.bind_target(idle.url(), idle.generation_id(), idle.revision()));

        let work = controller.work.lock();
        let barrier = Arc::new(Barrier::new(2));
        let thread_barrier = Arc::clone(&barrier);
        let thread_idle = idle.clone();
        let handle = std::thread::spawn(move || {
            let guard = WorkerLoadGuard::new(thread_idle, None);
            thread_barrier.wait();
            let transferred = permit.try_transfer_after_worker_load_reserved();
            drop(guard);
            transferred
        });
        barrier.wait();
        for _ in 0..5 {
            idle.record_outcome(503);
        }
        assert!(!idle.is_available());
        drop(work);

        assert!(!handle.join().unwrap());
        assert!(!controller
            .work
            .lock()
            .active_distribution_headroom
            .contains_key("model"));
    }

    #[test]
    fn concurrent_distribution_acquisition_has_exactly_two_winners() {
        let (controller, _hot, _idle) = distribution_controller();
        let barrier = Arc::new(Barrier::new(32));
        let handles: Vec<_> = (0..32)
            .map(|_| {
                let controller = Arc::clone(&controller);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    controller
                        .try_acquire_distribution_headroom("model", "model")
                        .map(|(permit, _)| permit)
                })
            })
            .collect();
        let winners: Vec<_> = handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 2);
        drop(winners);
        assert!(controller
            .try_acquire_distribution_headroom("model", "model")
            .is_some());
    }

    #[test]
    fn circuit_open_owner_still_contributes_occupancy_but_is_not_a_target() {
        let (controller, hot, idle) = distribution_controller();
        for _ in 0..5 {
            hot.record_outcome(503);
        }
        assert!(hot.is_healthy());
        assert!(!hot.is_available());
        assert!(controller.current_router_loads().contains_key(&(
            hot.url().to_string(),
            hot.generation_id(),
            hot.revision(),
        )));

        let (_permit, candidates) = controller
            .try_acquire_distribution_headroom("model", "model")
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].worker_url, idle.url());
    }

    #[test]
    fn distribution_challenge_requires_exact_partition_and_one_shot_authorization() {
        let (controller, _hot, _idle) = distribution_controller();
        let tracker = controller.begin("model".to_string(), features("u", 10, None));
        assert!(tracker.should_reject());
        assert_eq!(tracker.distribution_headroom_partition(), Some("model"));
        assert!(tracker.authorize_distribution_headroom());
        assert!(!tracker.authorize_distribution_headroom());
        assert!(!tracker.should_reject());

        let fallback =
            controller.begin("attacker-controlled".to_string(), features("u2", 10, None));
        assert!(fallback.should_reject());
        assert_eq!(fallback.distribution_headroom_partition(), None);

        let mut mismatched_model = features("u3", 10, None);
        mismatched_model.model = "other-model".to_string();
        let mismatched = controller.begin("model".to_string(), mismatched_model);
        assert!(mismatched.should_reject());
        assert_eq!(mismatched.distribution_headroom_partition(), None);
    }

    #[test]
    fn distribution_capacity_requires_partition_to_equal_sole_model() {
        let (controller, _hot, _idle) = distribution_controller();
        controller
            .work
            .lock()
            .loads
            .get_mut("model")
            .unwrap()
            .model_ids = HashSet::from(["other-model".to_string()]);

        assert_eq!(controller.effective_capacity("model", 100), 0);
        assert!(controller
            .try_acquire_distribution_headroom("model", "other-model")
            .is_none());
    }

    #[test]
    fn distribution_capacity_fails_closed_on_partial_or_stale_strict_view() {
        let (controller, _hot, _idle) = distribution_controller();
        {
            let mut work = controller.work.lock();
            work.loads
                .get_mut("model")
                .unwrap()
                .headroom_observed_replicas = 1;
        }
        assert_eq!(controller.effective_capacity("model", 100), 0);

        {
            let mut work = controller.work.lock();
            let load = work.loads.get_mut("model").unwrap();
            load.headroom_observed_replicas = 2;
            load.observed_at = Some(Instant::now() - Duration::from_secs(6));
        }
        assert_eq!(controller.effective_capacity("model", 100), 0);
        assert!(controller
            .try_acquire_distribution_headroom("model", "model")
            .is_none());
    }

    #[test]
    fn distribution_headroom_requires_canonical_scheduler_provenance() {
        for (source, scheduler_counts_present) in [
            (WorkerLoadSource::PrometheusOnly, false),
            (WorkerLoadSource::PrometheusOnly, true),
            (WorkerLoadSource::NativeLoads, false),
            (WorkerLoadSource::GrpcGetLoads, true),
        ] {
            let (controller, worker, response) = provenance_controller();
            controller.update_loads(
                &HashMap::from([(worker.url().to_string(), response.clone())]),
                &HashMap::from([(
                    worker.url().to_string(),
                    ObservedWorkerLoad {
                        response: response.clone(),
                        observed_at: Instant::now(),
                        worker_generation_id: worker.generation_id(),
                        worker_revision: worker.revision(),
                        router_load_at_observation: 0,
                        source,
                        scheduler_counts_present,
                    },
                )]),
            );

            let work = controller.work.lock();
            let load = work.loads.get("model").unwrap();
            assert_eq!(load.observed_replicas, 1);
            assert_eq!(load.max_running_requests, 2);
            assert_eq!(load.headroom_observed_replicas, 0);
            assert_eq!(load.issuable_headroom_requests, 0);
            drop(work);
            assert!(controller
                .try_acquire_distribution_headroom("model", "model")
                .is_none());
        }

        for source in [
            WorkerLoadSource::NativeLoads,
            WorkerLoadSource::SglangGetLoad,
        ] {
            let (controller, worker, response) = provenance_controller();
            controller.update_loads(
                &HashMap::from([(worker.url().to_string(), response.clone())]),
                &HashMap::from([(
                    worker.url().to_string(),
                    ObservedWorkerLoad {
                        response,
                        observed_at: Instant::now(),
                        worker_generation_id: worker.generation_id(),
                        worker_revision: worker.revision(),
                        router_load_at_observation: 0,
                        source,
                        scheduler_counts_present: true,
                    },
                )]),
            );
            assert!(controller
                .try_acquire_distribution_headroom("model", "model")
                .is_some());
        }
    }

    #[test]
    fn strict_worker_capacity_rejects_malformed_rank_telemetry() {
        let valid = WorkerLoadResponse {
            dp_rank_count: 1,
            loads: vec![SchedulerLoadSnapshot {
                dp_rank: 0,
                token_usage: 0.5,
                utilization: 0.5,
                num_running_reqs: 1,
                max_running_requests: 2,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(strict_worker_capacity(&valid, None).is_some());

        let mut zero_rank_count = valid.clone();
        zero_rank_count.dp_rank_count = 0;
        assert!(strict_worker_capacity(&zero_rank_count, None).is_none());
        let mut mismatched_rank_count = valid.clone();
        mismatched_rank_count.dp_rank_count = 2;
        assert!(strict_worker_capacity(&mismatched_rank_count, None).is_none());

        let mut negative = valid.clone();
        negative.loads[0].num_waiting_reqs = -1;
        assert!(strict_worker_capacity(&negative, None).is_none());
        let mut negative_capacity = valid.clone();
        negative_capacity.loads[0].max_running_requests = -1;
        assert!(strict_worker_capacity(&negative_capacity, Some(38)).is_none());
        let mut non_finite = valid.clone();
        non_finite.loads[0].token_usage = f64::NAN;
        assert!(strict_worker_capacity(&non_finite, None).is_none());
        let mut duplicate_rank = valid;
        duplicate_rank.dp_rank_count = 2;
        duplicate_rank.loads.push(duplicate_rank.loads[0].clone());
        assert!(strict_worker_capacity(&duplicate_rank, None).is_none());
    }

    #[test]
    fn strict_worker_capacity_uses_registered_cap_only_for_single_dp_rank() {
        let missing_cap = WorkerLoadResponse {
            dp_rank_count: 1,
            loads: vec![SchedulerLoadSnapshot {
                dp_rank: 0,
                token_usage: 0.5,
                utilization: 0.5,
                num_running_reqs: 1,
                max_running_requests: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let capacity = strict_worker_capacity(&missing_cap, Some(38)).unwrap();
        assert_eq!(capacity.max_running_requests, 38);
        assert!(strict_worker_capacity(&missing_cap, None).is_none());

        let mut ambiguous_multi_rank = missing_cap.clone();
        ambiguous_multi_rank.dp_rank_count = 2;
        ambiguous_multi_rank.loads.push(SchedulerLoadSnapshot {
            dp_rank: 1,
            ..ambiguous_multi_rank.loads[0].clone()
        });
        assert!(strict_worker_capacity(&ambiguous_multi_rank, Some(38)).is_none());
    }

    #[test]
    fn capacity_revision_changes_on_load_samples_not_per_request() {
        let controller =
            AdaptiveAdmissionController::new(config(), Arc::new(WorkerRegistry::new()));
        let revision = controller.subscribe_capacity_changes();
        let tracker = controller.begin("model".to_string(), features("a", 10, None));
        assert!(!revision.has_changed().unwrap());
        drop(tracker);
        assert!(!revision.has_changed().unwrap());

        controller.update_loads(&HashMap::new(), &HashMap::new());
        assert!(revision.has_changed().unwrap());
    }

    #[test]
    fn feedback_knee_moves_down_on_same_throughput_plateau() {
        let start = Instant::now();
        let mut estimate = FeedbackEstimate {
            peak_tokens_per_second_per_replica: 100.0,
            running_requests_per_replica_at_peak: 20.0,
            pressure_observed: false,
            last_update: start,
        };
        estimate.observe(99.0, 12.0, start + Duration::from_secs(1), 60.0, 0.02, true);
        assert_eq!(estimate.running_requests_per_replica_at_peak, 12.0);
        assert!(estimate.pressure_observed);
    }
}
