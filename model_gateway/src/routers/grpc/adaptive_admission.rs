//! Adaptive admission shared by the gRPC and HTTP serving paths.
//!
//! The existing priority scheduler remains the infrastructure safety layer.
//! The original strategy predicts output-token work. The engine-feedback
//! strategy instead learns each partition's useful running-concurrency knee
//! from live throughput, probes just above it, and backs off on engine queue or
//! KV pressure. Shadow mode exercises either state machine without delaying or
//! rejecting traffic.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::Instant,
};

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use openai_protocol::worker::WorkerLoadResponse;
use parking_lot::Mutex;
use tokio::sync::watch;

use crate::{
    config::{AdaptiveAdmissionConfig, AdaptiveAdmissionMode, AdaptiveAdmissionStrategy},
    observability::metrics::intern_string,
    worker::WorkerRegistry,
};

const ADMISSION_PARTITION_LABEL: &str = "admission_partition";

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
const ROUTER_OUTSTANDING_REQUESTS: &str = "smg_adaptive_admission_router_outstanding_requests";
const FEEDBACK_RUNNING_LIMIT: &str = "smg_adaptive_admission_feedback_running_limit";
const FEEDBACK_KNEE_PER_REPLICA: &str = "smg_adaptive_admission_feedback_knee_requests_per_replica";
const SEGMENTS: &str = "smg_adaptive_admission_estimator_segments";

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
    ) {
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
}

#[derive(Debug, Default)]
struct WorkState {
    outstanding_tokens: HashMap<String, u64>,
    outstanding_requests: HashMap<String, u64>,
    loads: HashMap<String, PartitionLoad>,
    capacities: HashMap<String, CapacityEstimate>,
    feedback_estimates: HashMap<String, FeedbackEstimate>,
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
}

impl AdaptiveAdmissionController {
    pub(crate) fn new(config: AdaptiveAdmissionConfig, registry: Arc<WorkerRegistry>) -> Arc<Self> {
        Arc::new(Self {
            predictor: Mutex::new(HierarchicalPredictor::new(&config)),
            config,
            work: Mutex::new(WorkState::default()),
            prediction_samples: PredictionSampleState::from_env().map(Mutex::new),
            registry,
        })
    }

    pub(crate) fn mode(&self) -> AdaptiveAdmissionMode {
        self.config.mode
    }

    pub(crate) fn start_load_updates(
        self: &Arc<Self>,
        mut loads: watch::Receiver<HashMap<String, WorkerLoadResponse>>,
    ) {
        self.update_loads(&loads.borrow());
        let controller = Arc::downgrade(self);
        #[expect(
            clippy::disallowed_methods,
            reason = "controller task holds only a weak reference and exits with the gateway"
        )]
        tokio::spawn(async move {
            loop {
                if loads.changed().await.is_err() {
                    break;
                }
                let Some(controller) = controller.upgrade() else {
                    break;
                };
                controller.update_loads(&loads.borrow());
            }
        });
    }

    fn update_loads(&self, loads: &HashMap<String, WorkerLoadResponse>) {
        let mut partitions: HashMap<String, PartitionLoad> = HashMap::new();
        for worker in self
            .registry
            .get_all()
            .into_iter()
            .filter(|w| w.is_healthy())
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
            aggregate.running_requests += load
                .loads
                .iter()
                .map(|rank| i64::from(rank.num_running_reqs.max(0)))
                .sum::<i64>();
            aggregate.waiting_requests += load
                .loads
                .iter()
                .map(|rank| i64::from(rank.num_waiting_reqs.max(0)))
                .sum::<i64>();
            aggregate.waiting_uncached_tokens += load.total_waiting_uncached_tokens().max(0);
            let token_usage = load.effective_token_usage().clamp(0.0, 1.0);
            aggregate.token_usage_sum += token_usage;
            aggregate.max_token_usage = aggregate.max_token_usage.max(token_usage);
            let reported_max_running = load
                .loads
                .iter()
                .map(|rank| i64::from(rank.max_running_requests.max(0)))
                .sum::<i64>();
            aggregate.max_running_requests += if reported_max_running > 0 {
                reported_max_running
            } else {
                worker.max_running_requests().map_or(0, i64::from)
            };
        }

        let now = Instant::now();
        let mut work = self.work.lock();
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
                    work.feedback_estimates
                        .entry(partition.clone())
                        .and_modify(|estimate| {
                            estimate.observe(
                                per_replica,
                                running_per_replica,
                                now,
                                self.config.estimator_half_life_secs,
                                self.config.feedback_throughput_improvement_ratio,
                            );
                        })
                        .or_insert(FeedbackEstimate {
                            peak_tokens_per_second_per_replica: per_replica,
                            running_requests_per_replica_at_peak: running_per_replica,
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
            gauge!(FEEDBACK_KNEE_PER_REPLICA, "partition" => partition_label).set(
                work.feedback_estimates
                    .get(partition)
                    .map_or(0.0, |estimate| {
                        estimate.running_requests_per_replica_at_peak
                    }),
            );
        }
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
        let partition = {
            let work = self.work.lock();
            if partition == features.model || work.loads.contains_key(&partition) {
                partition
            } else {
                features.model.clone()
            }
        };
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
                    let telemetry_usable = coverage >= self.config.min_load_coverage
                        && load.observed_replicas > 0
                        && load.max_running_requests > 0;
                    let engine_limit = if telemetry_usable {
                        (load.max_running_requests as f64 * f64::from(load.healthy_replicas)
                            / f64::from(load.observed_replicas))
                        .floor()
                    } else {
                        0.0
                    };
                    let learned_limit = feedback_estimate.as_ref().map(|estimate| {
                        (estimate.running_requests_per_replica_at_peak
                            * f64::from(load.healthy_replicas))
                        .ceil()
                            + f64::from(
                                self.config
                                    .feedback_probe_requests_per_healthy_replica
                                    .saturating_mul(load.healthy_replicas),
                            )
                    });
                    // Before a busy sample exists, the engine's own hard
                    // running limit is the bounded cold-start ceiling.
                    let running_limit = learned_limit
                        .map_or(engine_limit, |learned| learned.max(1.0).min(engine_limit));
                    let projected_requests =
                        router_outstanding_requests.max(engine_request_count + 1.0);
                    let waiting_limit = i64::from(
                        self.config
                            .feedback_max_waiting_requests_per_healthy_replica,
                    ) * i64::from(load.observed_replicas);
                    let reason = if !telemetry_usable {
                        "telemetry_fallback"
                    } else if load.mean_token_usage() >= self.config.feedback_max_token_usage {
                        "token_pressure"
                    } else if load.waiting_requests > waiting_limit {
                        "engine_waiting"
                    } else if projected_requests > running_limit {
                        "running_limit"
                    } else {
                        "within_feedback_limit"
                    };
                    let would_admit =
                        matches!(reason, "telemetry_fallback" | "within_feedback_limit");
                    gauge!(FEEDBACK_RUNNING_LIMIT, "partition" => Arc::clone(&partition_label))
                        .set(running_limit);
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
                        telemetry_usable,
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
    pub(crate) fn should_reject(&self) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        inner.controller.upgrade().is_some_and(|controller| {
            controller.mode() == AdaptiveAdmissionMode::Enforce
                && inner.decision.telemetry_usable
                && !inner.decision.would_admit
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
    use std::time::Duration;

    use super::*;

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
                max_token_usage: 0.5,
                ..PartitionLoad::default()
            },
        );
        work.feedback_estimates.insert(
            "model".to_string(),
            FeedbackEstimate {
                peak_tokens_per_second_per_replica: 100.0,
                running_requests_per_replica_at_peak: 10.0,
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
    fn feedback_knee_moves_down_on_same_throughput_plateau() {
        let start = Instant::now();
        let mut estimate = FeedbackEstimate {
            peak_tokens_per_second_per_replica: 100.0,
            running_requests_per_replica_at_peak: 20.0,
            last_update: start,
        };
        estimate.observe(99.0, 12.0, start + Duration::from_secs(1), 60.0, 0.02);
        assert_eq!(estimate.running_requests_per_replica_at_peak, 12.0);
    }
}
