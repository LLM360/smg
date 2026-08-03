//! Startup wiring for the priority scheduler: builds the admission mode
//! the route layer branches on.

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::http::HeaderMap;
use tokio::sync::watch;
use tracing::{error, info};

use super::{
    Class, PriorityScheduler, SchedulerSettings, StaticTenantPolicyResolver, TenantPolicyResolver,
};
use crate::{
    config::types::RouterConfig,
    middleware::token_bucket::TokenBucket,
    observability::metrics::Metrics,
    worker::{CapacityTrackerSettings, WorkerCapacity, WorkerRegistry},
};

/// How often the metrics sampler refreshes the capacity / autoscaling gauges.
const SAMPLER_INTERVAL: Duration = Duration::from_secs(5);

/// Trusted selector injected by the Comet proxy after it strips any
/// client-supplied value. Ordinary requests carry their model id; requests
/// routed to a private reservation carry `private`.
pub const ADMISSION_PARTITION_HEADER: &str = "x-smg-admission-partition";

#[derive(Clone)]
pub struct SchedulerPartition {
    pub name: Arc<str>,
    pub scheduler: Arc<PriorityScheduler>,
}

/// State handed to `priority_admission_middleware` via `from_fn_with_state`.
/// Cheap to clone (all `Arc`).
pub struct SchedulerState {
    /// Default scheduler, retained as a direct field for the original
    /// unpartitioned API and tests.
    pub scheduler: Arc<PriorityScheduler>,
    partitions: HashMap<String, SchedulerPartition>,
    default_partition: Arc<str>,
    pub resolver: Arc<dyn TenantPolicyResolver>,
    /// Per-second RPS sibling check, run before admission. Set only when an
    /// explicit `rate_limit_tokens_per_second` is configured; the bucket's
    /// concurrency-cap role is owned by the scheduler, so we must not consult
    /// it as a concurrency limiter (that would double-limit). `None` =
    /// no RPS limit.
    pub rate_limiter: Option<Arc<TokenBucket>>,
}

impl SchedulerState {
    /// Select an admission partition using only a trusted, exact header
    /// match. Missing, malformed, and unknown selectors use the configured
    /// default partition.
    pub fn partition_for(&self, headers: &HeaderMap) -> SchedulerPartition {
        let requested = headers
            .get(ADMISSION_PARTITION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        requested
            .and_then(|name| self.partitions.get(name))
            .cloned()
            .unwrap_or_else(|| SchedulerPartition {
                name: Arc::clone(&self.default_partition),
                scheduler: Arc::clone(&self.scheduler),
            })
    }
}

/// Which admission path the protected routes use. Chosen once at startup.
#[derive(Clone)]
pub enum AdmissionMode {
    /// Legacy `concurrency_limit_middleware` (default; zero behavior change).
    Legacy,
    /// Priority scheduler enabled.
    Priority(Arc<SchedulerState>),
}

impl AdmissionMode {
    /// Build the admission mode from runtime config.
    ///
    /// When `priority_scheduler_enabled` is false, returns `Legacy` without
    /// constructing anything. When true, constructs `WorkerCapacity` over
    /// the worker fleet, builds the scheduler against its current capacity,
    /// spawns the dispatcher on its watch channel, and returns
    /// `Priority(..)`.
    ///
    /// On any startup error (bad YAML, reservations exceed capacity), logs
    /// at ERROR and falls back to `Legacy` rather than aborting the whole
    /// gateway — a misconfigured scheduler must not take the data plane down.
    pub fn from_config(
        rc: &RouterConfig,
        registry: Arc<WorkerRegistry>,
        rate_limiter: Option<Arc<TokenBucket>>,
    ) -> Self {
        if !rc.priority_scheduler_enabled {
            return Self::Legacy;
        }
        match Self::try_build_priority(rc, registry, rate_limiter) {
            Ok(mode) => {
                info!("priority scheduler enabled");
                mode
            }
            Err(e) => {
                error!(
                    error = %e,
                    "priority scheduler failed to start; falling back to legacy admission"
                );
                Self::Legacy
            }
        }
    }

    fn try_build_priority(
        rc: &RouterConfig,
        registry: Arc<WorkerRegistry>,
        rate_limiter: Option<Arc<TokenBucket>>,
    ) -> Result<Self, String> {
        // The configured concurrency value is one global ceiling across the
        // entire healthy worker fleet. Worker-reported capacity may lower the
        // scheduler limit, but it must never raise it above this contract.
        let configured_max = if rc.max_concurrent_requests > 0 {
            Some(u16::try_from(rc.max_concurrent_requests).unwrap_or(u16::MAX))
        } else {
            None
        };
        let cap_settings = CapacityTrackerSettings {
            max_capacity: configured_max,
            legacy_max_concurrent_requests: configured_max.unwrap_or_else(|| {
                CapacityTrackerSettings::default().legacy_max_concurrent_requests
            }),
            ..CapacityTrackerSettings::default()
        };
        let worker_capacity = WorkerCapacity::spawn(registry, cap_settings);
        // Keep a receiver alive as soon as the tracker exists. Tokio's
        // `watch::Sender::send` does not retain a value when no receiver is
        // subscribed, so parsing the scheduler configuration must not create
        // a gap where the first fleet update is lost.
        let capacity_watch = worker_capacity.watch();

        let default_max_class = Class::parse_header(&rc.priority_scheduler_default_max_class);
        let yaml = load_yaml(rc.priority_scheduler_config.as_deref())?;
        let mut settings = SchedulerSettings::from_cli_and_yaml(
            true,
            default_max_class,
            rc.priority_scheduler_tenant_metric_top_n,
            yaml.as_ref(),
        )
        .map_err(|e| e.to_string())?;
        if yaml.is_none() {
            settings = settings.with_global_queue_budget(rc.queue_size);
        }

        let resolver: Arc<dyn TenantPolicyResolver> =
            Arc::new(StaticTenantPolicyResolver::from_settings(&settings));

        // The scheduler owns concurrency, so the shared bucket only survives
        // as an RPS check when an explicit per-second limit is configured.
        let rate_limiter = match rc.rate_limit_tokens_per_second {
            Some(rps) if rps > 0 => rate_limiter,
            _ => None,
        };

        let Some(partition_config) = yaml
            .as_ref()
            .filter(|yaml| !yaml.admission_partitions.is_empty())
        else {
            // The atomic value covers any update that won the race before the
            // receiver subscribed; subsequent updates remain queued for the
            // dispatcher through `capacity_watch`.
            let scheduler = PriorityScheduler::new(&settings, worker_capacity.current())
                .map_err(|e| e.to_string())?;
            scheduler.spawn_dispatcher_retaining_capacity(capacity_watch, worker_capacity);
            scheduler.spawn_sampler(SAMPLER_INTERVAL);
            return Ok(Self::Priority(Arc::new(SchedulerState {
                scheduler,
                partitions: HashMap::new(),
                default_partition: Arc::from("global"),
                resolver,
                rate_limiter,
            })));
        };

        let configured_max = configured_max.unwrap_or_else(|| worker_capacity.current());
        validate_partitions(
            &partition_config.admission_partitions,
            &partition_config.default_admission_partition,
            configured_max,
            rc.queue_size,
        )?;

        let limits: Vec<(String, u16)> = {
            let mut entries: Vec<_> = partition_config
                .admission_partitions
                .iter()
                .map(|(name, config)| (name.clone(), config.max_concurrent_requests))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        };
        let initial = allocate_partition_capacities(&limits, worker_capacity.current());
        let mut partitions = HashMap::new();
        let mut capacity_senders = Vec::with_capacity(limits.len());

        for (name, limit) in &limits {
            let config = partition_config
                .admission_partitions
                .get(name)
                .ok_or_else(|| format!("partition {name} disappeared during startup"))?;
            let partition_settings = settings.clone().for_admission_partition(
                *limit,
                configured_max,
                config.queue_size as usize,
            );
            let capacity = initial.get(name).copied().unwrap_or(0);
            let scheduler = PriorityScheduler::new(&partition_settings, capacity)
                .map_err(|e| format!("partition {name}: {e}"))?;
            let (capacity_tx, capacity_rx) = watch::channel(capacity);
            scheduler.spawn_dispatcher(capacity_rx);
            capacity_senders.push((name.clone(), capacity_tx));
            partitions.insert(
                name.clone(),
                SchedulerPartition {
                    name: Arc::from(name.as_str()),
                    scheduler,
                },
            );
            info!(
                admission.partition = %name,
                admission.max_concurrent_requests = *limit,
                admission.initial_capacity = capacity,
                admission.queue_size = config.queue_size,
                "priority admission partition enabled"
            );
        }

        let default_partition_name = partition_config.default_admission_partition.clone();
        let default_scheduler = Arc::clone(
            &partitions
                .get(&default_partition_name)
                .ok_or_else(|| {
                    format!(
                        "default admission partition {default_partition_name:?} disappeared during startup"
                    )
                })?
                .scheduler,
        );
        spawn_partition_capacity_coordinator(
            capacity_watch,
            worker_capacity,
            limits,
            capacity_senders,
        );
        spawn_partition_metrics_sampler(partitions.values().cloned().collect());

        Ok(Self::Priority(Arc::new(SchedulerState {
            scheduler: default_scheduler,
            partitions,
            default_partition: Arc::from(default_partition_name),
            resolver,
            rate_limiter,
        })))
    }
}

fn spawn_partition_metrics_sampler(partitions: Vec<SchedulerPartition>) {
    let partitions: Vec<_> = partitions
        .into_iter()
        .map(|partition| (partition.name, Arc::downgrade(&partition.scheduler)))
        .collect();
    #[expect(
        clippy::disallowed_methods,
        reason = "sampler holds only weak scheduler references and exits after all partitions drop"
    )]
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SAMPLER_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut live = 0_usize;
            let mut total_capacity = 0_u16;
            let mut total_queue_capacity = 0_usize;
            let mut total_inflight = [0_u16; 4];
            let mut total_queue_depth = [0_usize; 4];
            let mut total_queue_limit = [0_usize; 4];
            let mut max_retry_after = [0_u64; 4];
            let mut max_pressure = [0.0_f64; 4];

            for (name, weak) in &partitions {
                let Some(scheduler) = weak.upgrade() else {
                    continue;
                };
                live += 1;
                let snapshot = scheduler.metrics_snapshot();
                total_capacity = total_capacity.saturating_add(snapshot.capacity);
                total_queue_capacity = total_queue_capacity.saturating_add(snapshot.queue_capacity);
                let partition_inflight: u32 = snapshot
                    .inflight
                    .iter()
                    .map(|value| u32::from(*value))
                    .sum();
                let utilization = if snapshot.capacity == 0 {
                    0.0
                } else {
                    f64::from(partition_inflight) / f64::from(snapshot.capacity)
                };
                super::metrics::set_partition_capacity(name, snapshot.capacity);
                super::metrics::set_partition_utilization(name, utilization);

                for class in Class::ALL {
                    let index = class as usize;
                    super::metrics::set_partition_inflight(name, class, snapshot.inflight[index]);
                    super::metrics::set_partition_queue_depth(
                        name,
                        class,
                        snapshot.queue_depth[index],
                    );
                    super::metrics::set_partition_queue_size_limit(
                        name,
                        class,
                        snapshot.queue_limit[index],
                    );
                    total_inflight[index] =
                        total_inflight[index].saturating_add(snapshot.inflight[index]);
                    total_queue_depth[index] =
                        total_queue_depth[index].saturating_add(snapshot.queue_depth[index]);
                    total_queue_limit[index] =
                        total_queue_limit[index].saturating_add(snapshot.queue_limit[index]);
                    max_retry_after[index] =
                        max_retry_after[index].max(snapshot.retry_after_secs[index]);
                    max_pressure[index] = max_pressure[index].max(snapshot.class_pressure[index]);
                }
            }

            if live == 0 {
                break;
            }
            let aggregate_inflight: u32 =
                total_inflight.iter().map(|value| u32::from(*value)).sum();
            for class in Class::ALL {
                let index = class as usize;
                super::metrics::set_inflight(class, total_inflight[index]);
                super::metrics::set_queue_depth(class, total_queue_depth[index]);
                super::metrics::set_queue_size_limit(class, total_queue_limit[index]);
                super::metrics::set_retry_after_seconds(class, max_retry_after[index]);
                super::metrics::set_class_capacity_pressure(class, max_pressure[index]);
            }
            Metrics::set_http_admission_limit(usize::from(total_capacity));
            Metrics::set_http_admission_queue_capacity(total_queue_capacity);
            super::metrics::set_utilization(if total_capacity == 0 {
                0.0
            } else {
                f64::from(aggregate_inflight) / f64::from(total_capacity)
            });
        }
    });
}

fn validate_partitions(
    partitions: &HashMap<String, super::AdmissionPartitionConfig>,
    default_partition: &str,
    global_max: u16,
    global_queue_size: usize,
) -> Result<(), String> {
    if !partitions.contains_key(default_partition) {
        return Err(format!(
            "default admission partition {default_partition:?} is not configured"
        ));
    }
    let mut total_capacity = 0_u32;
    let mut total_queue = 0_u64;
    for (name, config) in partitions {
        if name.is_empty() || name.trim() != name {
            return Err(format!("invalid admission partition name {name:?}"));
        }
        if config.max_concurrent_requests == 0 {
            return Err(format!(
                "partition {name}: max_concurrent_requests must be > 0"
            ));
        }
        total_capacity += u32::from(config.max_concurrent_requests);
        total_queue += u64::from(config.queue_size);
    }
    if total_capacity > u32::from(global_max) {
        return Err(format!(
            "admission partition capacities sum to {total_capacity}, above global max {global_max}"
        ));
    }
    if total_queue > global_queue_size as u64 {
        return Err(format!(
            "admission partition queues sum to {total_queue}, above global queue size {global_queue_size}"
        ));
    }
    Ok(())
}

/// Proportionally shrink configured caps to the live global capacity using
/// largest remainders. The result is deterministic and always sums to
/// `min(global_capacity, sum(configured caps))`.
fn allocate_partition_capacities(
    limits: &[(String, u16)],
    global_capacity: u16,
) -> HashMap<String, u16> {
    let total_limit: u64 = limits.iter().map(|(_, limit)| u64::from(*limit)).sum();
    if total_limit == 0 {
        return HashMap::new();
    }
    let target = u64::from(global_capacity).min(total_limit);
    let mut allocated = 0_u64;
    let mut rows: Vec<(String, u16, u64)> = limits
        .iter()
        .map(|(name, limit)| {
            let numerator = u64::from(*limit) * target;
            let base = (numerator / total_limit) as u16;
            allocated += u64::from(base);
            (name.clone(), base, numerator % total_limit)
        })
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let mut remainder = target.saturating_sub(allocated);
    for (_, base, _) in &mut rows {
        if remainder == 0 {
            break;
        }
        *base = base.saturating_add(1);
        remainder -= 1;
    }
    rows.into_iter()
        .map(|(name, capacity, _)| (name, capacity))
        .collect()
}

fn spawn_partition_capacity_coordinator(
    mut capacity_watch: watch::Receiver<u16>,
    worker_capacity: Arc<WorkerCapacity>,
    limits: Vec<(String, u16)>,
    capacity_senders: Vec<(String, watch::Sender<u16>)>,
) {
    #[expect(
        clippy::disallowed_methods,
        reason = "gateway-lifetime coordinator owns the capacity tracker and exits when its watch closes"
    )]
    tokio::spawn(async move {
        let _worker_capacity = worker_capacity;
        while capacity_watch.changed().await.is_ok() {
            let allocations = allocate_partition_capacities(&limits, *capacity_watch.borrow());
            for (name, sender) in &capacity_senders {
                if let Some(capacity) = allocations.get(name) {
                    sender.send_replace(*capacity);
                }
            }
        }
    });
}

/// Load + parse the optional priority-scheduler YAML file.
fn load_yaml(path: Option<&str>) -> Result<Option<super::PrioritySchedulerYaml>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let contents = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let parsed = serde_yaml::from_str(&contents).map_err(|e| format!("parsing {path}: {e}"))?;
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, io::Write, time::Instant};

    use axum::http::{HeaderMap, HeaderValue};
    use smg_auth::RequestId;
    use tempfile::NamedTempFile;
    use tokio::time::{sleep, Duration};

    use super::*;
    use crate::worker::BasicWorkerBuilder;

    #[tokio::test]
    async fn priority_mode_applies_capacity_changes_after_startup() {
        let registry = Arc::new(WorkerRegistry::new());
        let config = RouterConfig {
            max_concurrent_requests: 256,
            priority_scheduler_enabled: true,
            ..RouterConfig::default()
        };
        let AdmissionMode::Priority(state) =
            AdmissionMode::try_build_priority(&config, Arc::clone(&registry), None).unwrap()
        else {
            panic!("priority scheduler should start");
        };

        let mut labels = HashMap::new();
        labels.insert("max_running_requests".to_string(), "1".to_string());
        let worker = Arc::new(
            BasicWorkerBuilder::new("http://capacity-test:8000")
                .labels(labels)
                .status(openai_protocol::worker::WorkerStatus::Ready)
                .build(),
        );
        registry.register(worker).expect("worker should register");

        // The dispatcher must retain the tracker and apply its update. Once
        // capacity drops to one, a second System request cannot acquire a
        // slot while the first is still in flight.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut attempt = 0;
        loop {
            let first = state.scheduler.acquire_inflight(
                Class::System,
                RequestId(format!("capacity-first-{attempt}")),
            );
            let second = state.scheduler.acquire_inflight(
                Class::System,
                RequestId(format!("capacity-second-{attempt}")),
            );
            let updated = first.is_some() && second.is_none();
            drop(second);
            drop(first);

            if updated {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "scheduler did not apply the worker capacity update"
            );
            attempt += 1;
            sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn priority_mode_caps_aggregate_worker_capacity_at_configured_maximum() {
        let registry = Arc::new(WorkerRegistry::new());
        let mut labels = HashMap::new();
        labels.insert("max_running_requests".to_string(), "512".to_string());
        let worker = Arc::new(
            BasicWorkerBuilder::new("http://capacity-test:8000")
                .labels(labels)
                .status(openai_protocol::worker::WorkerStatus::Ready)
                .build(),
        );
        registry.register(worker).expect("worker should register");

        let config = RouterConfig {
            max_concurrent_requests: 256,
            priority_scheduler_enabled: true,
            ..RouterConfig::default()
        };
        let AdmissionMode::Priority(state) =
            AdmissionMode::try_build_priority(&config, registry, None).unwrap()
        else {
            panic!("priority scheduler should start");
        };

        let permits: Vec<_> = (0..256)
            .map(|index| {
                state
                    .scheduler
                    .acquire_inflight(Class::System, RequestId(format!("cap-{index}")))
                    .expect("configured capacity should remain available")
            })
            .collect();
        let overflow = state
            .scheduler
            .acquire_inflight(Class::System, RequestId("cap-overflow".into()));
        assert!(overflow.is_none());
        drop(permits);
    }

    #[test]
    fn proportional_partition_allocation_preserves_global_ceiling() {
        let limits = vec![
            ("default".to_string(), 1),
            ("dsv4".to_string(), 2),
            ("kimi-k3".to_string(), 7),
        ];
        let full = allocate_partition_capacities(&limits, 10);
        assert_eq!(full.values().copied().sum::<u16>(), 10);
        assert_eq!(full["kimi-k3"], 7);

        let drained = allocate_partition_capacities(&limits, 5);
        assert_eq!(drained.values().copied().sum::<u16>(), 5);
        assert_eq!(drained["kimi-k3"], 3);
        assert_eq!(drained["dsv4"], 1);
        assert_eq!(drained["default"], 1);
    }

    #[test]
    fn partition_validation_rejects_budget_inflation() {
        let partitions = HashMap::from([
            (
                "default".to_string(),
                super::super::AdmissionPartitionConfig {
                    max_concurrent_requests: 4,
                    queue_size: 2,
                },
            ),
            (
                "kimi-k3".to_string(),
                super::super::AdmissionPartitionConfig {
                    max_concurrent_requests: 7,
                    queue_size: 3,
                },
            ),
        ]);
        assert!(validate_partitions(&partitions, "default", 10, 5)
            .unwrap_err()
            .contains("above global max"));
        assert!(validate_partitions(&partitions, "default", 11, 4)
            .unwrap_err()
            .contains("above global queue size"));
    }

    #[tokio::test]
    async fn saturated_model_partition_does_not_block_another_model() {
        let mut yaml = NamedTempFile::new().unwrap();
        write!(
            yaml,
            r#"
admission_partitions:
  kimi-k3:
    max_concurrent_requests: 6
    queue_size: 3
  deepseek-v4-flash-0731:
    max_concurrent_requests: 1
    queue_size: 1
  private:
    max_concurrent_requests: 1
    queue_size: 0
  default:
    max_concurrent_requests: 1
    queue_size: 0
default_admission_partition: default
"#
        )
        .unwrap();
        let config = RouterConfig {
            max_concurrent_requests: 9,
            queue_size: 4,
            priority_scheduler_enabled: true,
            priority_scheduler_config: Some(yaml.path().to_string_lossy().into_owned()),
            ..RouterConfig::default()
        };
        let AdmissionMode::Priority(state) =
            AdmissionMode::try_build_priority(&config, Arc::new(WorkerRegistry::new()), None)
                .unwrap()
        else {
            panic!("priority scheduler should start");
        };

        let mut k3_headers = HeaderMap::new();
        k3_headers.insert(
            ADMISSION_PARTITION_HEADER,
            HeaderValue::from_static("kimi-k3"),
        );
        let k3 = state.partition_for(&k3_headers);
        let k3_permits: Vec<_> = (0..6)
            .map(|index| {
                k3.scheduler
                    .acquire_inflight(Class::System, RequestId(format!("k3-{index}")))
                    .expect("K3 partition slot")
            })
            .collect();
        assert!(k3
            .scheduler
            .acquire_inflight(Class::System, RequestId("k3-full".into()))
            .is_none());

        let mut dsv4_headers = HeaderMap::new();
        dsv4_headers.insert(
            ADMISSION_PARTITION_HEADER,
            HeaderValue::from_static("deepseek-v4-flash-0731"),
        );
        let dsv4 = state.partition_for(&dsv4_headers);
        assert_eq!(&*dsv4.name, "deepseek-v4-flash-0731");
        let dsv4_permit = dsv4
            .scheduler
            .acquire_inflight(Class::System, RequestId("dsv4".into()));
        assert!(dsv4_permit.is_some());

        let mut unknown_headers = HeaderMap::new();
        unknown_headers.insert(
            ADMISSION_PARTITION_HEADER,
            HeaderValue::from_static("new-model"),
        );
        assert_eq!(&*state.partition_for(&unknown_headers).name, "default");
        drop(dsv4_permit);
        drop(k3_permits);
    }
}
