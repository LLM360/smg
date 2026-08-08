/*
    Cache-Aware Load Balancing Router

    Uses cache-aware routing whenever a suitable longest-prefix owner exists.
    When no owner exists, or every owner is pressured, uses size-aware P2C.
    The legacy count-based imbalance signal fires when both:
        (max - min) > abs_threshold  AND  max > rel_threshold * min

    Three types of cache-aware routing (mutually exclusive, selected by
    worker connection mode and KV event availability):

    1. Event-Driven (gRPC + KV events)
    -------------------------------------------
    Uses PositionalIndexer overlap scoring from KvEventMonitor. Routes based
    on actual backend KV cache state. Selects the worker with the highest
    overlap count, then chooses the least-loaded matching owner.
    Falls back to size-aware power-of-two when no cache overlap exists.

    2. Approximate Token Tree (gRPC, no KV events)
    -------------------------------------------
    Maintains a TokenTree per model tracking which token prefixes were routed
    where. If match_rate > cache_threshold, routes to the best-matching worker.
    Otherwise routes to the worker with the smallest tree (most cache capacity).

    3. Approximate String Tree (HTTP)
    -------------------------------------------
    Same algorithm as (2) but operates on raw text characters instead of
    token IDs, avoiding tokenization overhead. Both approximate paths retain
    every recorded owner of the longest match and balance within that set.

    Load Balancing (Size-Aware Power-of-Two)
    -------------------------------------------
    Cache misses and stale owners use size-aware power-of-two. A pressured
    owner set may add one bounded replica. The selected fallback becomes
    another prefix owner.

    Engine Pressure Guard (Optional)
    -------------------------------------------
    Restricts cached-owner candidates to workers within 10 percentage points of
    the least-pressured owner and below 90% pressure. Non-owner candidates use
    the equivalent fleet-scoped guard for cold fallback. Pressure is the largest
    of KV token usage, utilization, and scheduler occupancy when every engine
    reports a running-request cap for every DP rank. Missing, partial, or stale
    telemetry fails open to existing owners.

    Configuration Parameters:
    ------------------------
    cache_threshold:         Min prefix match ratio for highest-match routing (0.0-1.0)
    balance_abs_threshold:   Absolute load diff threshold for imbalance detection
    balance_rel_threshold:   Relative load ratio threshold for imbalance detection
    eviction_interval_secs:  Interval between LRU eviction cycles
    max_tree_size:           Max nodes per approximate tree before eviction
    block_size:              Backend KV cache block size for event-driven routing
    engine_load:             Enable the engine pressure guard
    max_cached_owners_per_prefix:
                             Replication ceiling for one prefix (0 disables)
    cache_owner_spill_cooldown_secs:
                             Minimum interval between pressure-driven owners
*/

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use dashmap::{mapref::entry::Entry, DashMap};
use kv_index::{compute_request_content_hashes, PositionalIndexer, TokenTree, Tree};
use openai_protocol::worker::WorkerLoadResponse;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, warn};

use super::{
    normalize_model_key, utils::PeriodicTask, CacheAwareConfig, LoadBalancingPolicy,
    SelectWorkerInfo, SizeAwarePowerOfTwoPolicy,
};
use crate::{
    mesh::adapters::tree_sync::{RepairEntry, TreeRepairPage},
    observability::metrics::Metrics,
    worker::{KvEventMonitor, Worker},
};

const ENGINE_LOAD_MAX_AGE: Duration = Duration::from_secs(30);
const ENGINE_PRESSURE_SLACK: f64 = 0.10;
const ENGINE_PRESSURE_HIGH_WATERMARK: f64 = 0.90;
const ADMISSION_PARTITION_HEADER: &str = "x-smg-admission-partition";

static NEXT_CACHE_POLICY_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImbalanceReason {
    BackendOverload,
    BackendSpread,
    ReservedWork,
    RequestCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PrefixKind {
    String,
    Token,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PrefixBudgetKey {
    model_hash: u64,
    prefix_hash: u64,
    kind: PrefixKind,
}

#[derive(Debug, Clone)]
struct PrefixReplicationState {
    last_spill: Instant,
    provisional_owner: String,
}

#[derive(Debug, Clone)]
struct PendingDistributionSeed {
    lease_id: u64,
    prefix_key: PrefixBudgetKey,
    model_id: Arc<str>,
    target_url: Arc<str>,
    target_generation_id: u64,
    target_revision: u64,
    request_digest: blake3::Hash,
    request_len: usize,
    creates_owner: bool,
}

#[derive(Debug)]
struct CacheDistributionState {
    policy_instance_id: u64,
    next_lease_id: AtomicU64,
    pending_by_partition: DashMap<String, PendingDistributionSeed>,
}

impl CacheDistributionState {
    fn new() -> Self {
        // Pointer equality on `CacheDistributionState` is the authoritative
        // instance binding. This monotonic label makes diagnostics and tests
        // readable; wrapping after u64::MAX policy constructions is harmless.
        let policy_instance_id = NEXT_CACHE_POLICY_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            policy_instance_id,
            next_lease_id: AtomicU64::new(1),
            pending_by_partition: DashMap::new(),
        }
    }

    fn next_lease_id(&self) -> Option<u64> {
        self.next_lease_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .ok()
    }

    fn release_if_current(&self, partition: &str, lease_id: u64) -> bool {
        self.pending_by_partition
            .remove_if(partition, |_, pending| pending.lease_id == lease_id)
            .is_some()
    }
}

/// One exact, success-committed cache-distribution decision.
///
/// The lease deliberately does not implement `Clone`. Dropping it before a
/// successful commit releases only its own pending partition entry. A stale
/// lease ID can therefore never clear a newer decision for the same partition.
#[derive(Debug)]
pub(crate) struct CacheDistributionLease {
    state: Arc<CacheDistributionState>,
    policy_instance_id: u64,
    lease_id: u64,
    partition: Arc<str>,
    prefix_key: PrefixBudgetKey,
    model_id: Arc<str>,
    target_url: Arc<str>,
    target_generation_id: u64,
    target_revision: u64,
    request_digest: blake3::Hash,
    request_len: usize,
    creates_owner: bool,
    finished: bool,
}

impl CacheDistributionLease {
    pub(super) fn partition(&self) -> &str {
        &self.partition
    }

    pub(super) fn target_url(&self) -> &str {
        &self.target_url
    }

    pub(super) fn target_revision(&self) -> u64 {
        self.target_revision
    }

    pub(super) fn target_generation_id(&self) -> u64 {
        self.target_generation_id
    }

    fn request_matches(&self, text: &str) -> bool {
        text.len() == self.request_len && blake3::hash(text.as_bytes()) == self.request_digest
    }

    fn pending_matches(&self) -> bool {
        self.policy_instance_id == self.state.policy_instance_id
            && self
                .state
                .pending_by_partition
                .get(self.partition.as_ref())
                .is_some_and(|pending| {
                    pending.lease_id == self.lease_id
                        && pending.prefix_key == self.prefix_key
                        && pending.model_id == self.model_id
                        && pending.target_url == self.target_url
                        && pending.target_generation_id == self.target_generation_id
                        && pending.target_revision == self.target_revision
                        && pending.request_digest == self.request_digest
                        && pending.request_len == self.request_len
                        && pending.creates_owner == self.creates_owner
                })
    }

    fn finish(&mut self) -> bool {
        let removed = self
            .state
            .release_if_current(self.partition.as_ref(), self.lease_id);
        if removed {
            self.finished = true;
        }
        removed
    }
}

impl Drop for CacheDistributionLease {
    fn drop(&mut self) {
        if !self.finished {
            self.state
                .release_if_current(self.partition.as_ref(), self.lease_id);
        }
    }
}

#[derive(Debug)]
struct PrefixOwnership {
    key: PrefixBudgetKey,
    owners: Vec<usize>,
    /// Whether the authoritative/approximate index matched at least one owner,
    /// including an owner that is currently not executable.
    has_known_owner: bool,
}

#[derive(Debug, Clone)]
struct TimedWorkerLoad {
    response: WorkerLoadResponse,
    observed_at: Instant,
}

#[derive(Debug)]
struct EnginePressurePlan {
    allowed_indices: Vec<usize>,
    best_index: usize,
    pressure_by_index: HashMap<usize, WorkerPressure>,
}

#[derive(Debug, Clone, Copy)]
struct WorkerPressure {
    pressure: f64,
    waiting_requests: i64,
}

/// Latest per-worker backend load snapshot stream, keyed by worker URL.
pub(crate) type LoadReceiver = watch::Receiver<HashMap<String, WorkerLoadResponse>>;

/// Cache-aware routing policy
///
/// Routes requests based on cache affinity when load is balanced,
/// switches to shortest-queue routing when load is imbalanced.
/// Maintains separate trees per model for multi-model support.
/// Supports mesh synchronization of tree operations across cluster nodes.
/// When mesh is not enabled, the policy works independently without synchronization.
///
/// Supports both HTTP (string-based) and gRPC (token-based) connections:
/// - HTTP requests use StringTree (character-based prefix matching)
/// - gRPC requests use TokenTree (token-based prefix matching, page-aligned)
#[derive(Debug)]
pub struct CacheAwarePolicy {
    config: CacheAwareConfig,
    /// Atomic token-work reservations used for cold-prefix fallback and for
    /// balancing requests across every healthy owner of a hot prefix.
    fallback: SizeAwarePowerOfTwoPolicy,
    /// String-based trees for HTTP connections (text input)
    string_trees: Arc<DashMap<String, Arc<Tree>>>,
    /// Token-based trees for gRPC connections (pre-tokenized input)
    token_trees: Arc<DashMap<String, Arc<TokenTree>>>,
    _eviction_task: Option<PeriodicTask>,
    /// Event-driven KV cache monitor for overlap scoring (gRPC workers only).
    kv_monitor: RwLock<Option<Arc<KvEventMonitor>>>,
    /// Latest per-worker backend load snapshot (keyed by worker URL) from the
    /// `WorkerMonitor` load poll. Read on the hot path for the KV-usage imbalance
    /// trigger. `None` until wired by the registry (then the policy stays
    /// count-only, preserving current behavior).
    load_rx: RwLock<Option<LoadReceiver>>,
    /// Model-scoped hash indexes for resolving tenant delta hashes.
    /// Outer key is the normalized model_id; inner maps hold
    /// `hash → reconstructable prefix/tokens` per tree kind.
    /// Spec §7.1 mandates model scoping: the same hash can refer
    /// to different prefixes in different models, so a global
    /// index mis-routes multi-model deployments. Bounded by
    /// eviction at `max_tree_size` total entries.
    ///
    /// Per-entry value semantics differ by populate site:
    /// - `select_worker_*` (request hot paths) store the prior
    ///   shared prefix from a pre-insert match. Bytes/entry is
    ///   bounded by tree depth, not input size — a 32K-token
    ///   request costs O(matched-prefix), not O(input).
    /// - `apply_repair_page` (cold-start replay) stores the full
    ///   inserted path because the canonical path is required to
    ///   attach remote tenants at the correct node. This path
    ///   runs at replay frequency, not request rate.
    hash_index: Arc<DashMap<String, PerModelHashIndex>>,
    /// Gate request-hot-path `hash_index` writes. The index's only
    /// consumers are mesh paths (`apply_known_remote_insert` reads,
    /// `apply_repair_page` writes). When mesh is disabled the
    /// hot-path writes accumulate with no reader and OOM the
    /// gateway. Off by default; the mesh wiring code flips it on
    /// when it attaches.
    populate_hash_index: AtomicBool,
    /// Last successful engine load snapshot per worker.
    engine_loads: RwLock<HashMap<String, TimedWorkerLoad>>,
    /// Per-prefix replication throttle. This controls creation of new owners,
    /// not the authoritative owner catalog, which always records every worker
    /// reported by the backend event stream.
    replication_state: Arc<DashMap<PrefixBudgetKey, PrefixReplicationState>>,
    _replication_gc_task: Option<PeriodicTask>,
    /// Exact, success-committed cache-distribution leases. This state is
    /// separate from the ordinary replication cooldown because acquiring a
    /// lease must not claim durable ownership before the backend succeeds.
    distribution_state: Arc<CacheDistributionState>,
    /// Serializes guarded ordinary selection with exact lease acquisition and
    /// ownership commit. Routing is cheap relative to inference, and this
    /// process-local fence closes the cold-bootstrap check/insert race without
    /// widening the critical section to backend execution.
    distribution_selection_lock: Mutex<()>,
}

/// Per-model inner container for [`CacheAwarePolicy::hash_index`].
/// Keeping both kinds in one struct per model makes the
/// "separate model-scoped hash indexes for string and token
/// trees" invariant from spec §7.1 explicit in the type.
#[derive(Debug, Default)]
struct PerModelHashIndex {
    /// path hash → matched prefix (reconstructs the string-tree node).
    string_tree: DashMap<u64, String>,
    /// token-path hash → tokens (reconstructs the token-tree node).
    token_tree: DashMap<u64, Vec<u32>>,
}

impl CacheAwarePolicy {
    pub fn new() -> Self {
        Self::with_config(CacheAwareConfig::default())
    }

    pub fn with_config(config: CacheAwareConfig) -> Self {
        let string_trees = Arc::new(DashMap::<String, Arc<Tree>>::new());
        let token_trees = Arc::new(DashMap::<String, Arc<TokenTree>>::new());
        let hash_index = Arc::new(DashMap::<String, PerModelHashIndex>::new());
        let replication_state = Arc::new(DashMap::<PrefixBudgetKey, PrefixReplicationState>::new());
        let distribution_state = Arc::new(CacheDistributionState::new());

        // Start background eviction thread if configured
        let eviction_task = if config.eviction_interval_secs > 0 {
            let string_trees_clone = Arc::clone(&string_trees);
            let token_trees_clone = Arc::clone(&token_trees);
            let hash_index_clone = Arc::clone(&hash_index);
            let max_tree_size = config.max_tree_size;

            Some(PeriodicTask::spawn(
                config.eviction_interval_secs,
                "Eviction",
                move || {
                    // Evict string trees (HTTP)
                    for tree_ref in string_trees_clone.iter() {
                        let model_id = tree_ref.key();
                        let tree = tree_ref.value();
                        tree.evict_tenant_by_size(max_tree_size);

                        debug!(
                            "String tree eviction completed for model {}, max_size: {}",
                            model_id, max_tree_size
                        );
                    }
                    // Evict token trees (gRPC)
                    for tree_ref in token_trees_clone.iter() {
                        let model_id = tree_ref.key();
                        let tree = tree_ref.value();
                        tree.evict_tenant_by_size(max_tree_size);

                        debug!(
                            "Token tree eviction completed for model {}, max_size: {}",
                            model_id, max_tree_size
                        );
                    }
                    // Evict hash index per model: `max_tree_size` is a
                    // per-tree bound, so clearing one model's overflow
                    // must not wipe other models' still-valid metadata.
                    // Each tree kind is checked independently.
                    let mut hash_total: usize = 0;
                    for entry in hash_index_clone.iter() {
                        let per_model = entry.value();
                        if per_model.string_tree.len() > max_tree_size {
                            per_model.string_tree.clear();
                            debug!(
                                model_id = entry.key(),
                                "String hash index cleared (exceeded max_tree_size: {})",
                                max_tree_size
                            );
                        }
                        if per_model.token_tree.len() > max_tree_size {
                            per_model.token_tree.clear();
                            debug!(
                                model_id = entry.key(),
                                "Token hash index cleared (exceeded max_tree_size: {})",
                                max_tree_size
                            );
                        }
                        hash_total += per_model.string_tree.len() + per_model.token_tree.len();
                    }

                    // Log tree sizes — model counts + hash-index total.
                    // DO NOT call tree.snapshot() here — it clones all
                    // edge text (~170 MB) every cycle.
                    tracing::info!(
                        "Tree memory: string_trees={} models, token_trees={} models, \
                         hash_index={} models / {} entries",
                        string_trees_clone.len(),
                        token_trees_clone.len(),
                        hash_index_clone.len(),
                        hash_total,
                    );
                },
            ))
        } else {
            None
        };

        let fallback = SizeAwarePowerOfTwoPolicy::new(config.fallback_output_token_estimate);
        let replication_gc_task = if config.max_cached_owners_per_prefix > 0
            && config.cache_owner_spill_cooldown_secs > 0
        {
            let state = Arc::clone(&replication_state);
            let cooldown = Duration::from_secs(config.cache_owner_spill_cooldown_secs);
            let retention = cooldown.saturating_mul(4).max(Duration::from_secs(60));
            Some(PeriodicTask::spawn(
                config.cache_owner_spill_cooldown_secs.max(1),
                "Prefix replication budget GC",
                move || state.retain(|_, entry| entry.last_spill.elapsed() <= retention),
            ))
        } else {
            None
        };
        Self {
            config,
            fallback,
            string_trees,
            token_trees,
            _eviction_task: eviction_task,
            kv_monitor: RwLock::new(None),
            load_rx: RwLock::new(None),
            hash_index,
            populate_hash_index: AtomicBool::new(false),
            engine_loads: RwLock::new(HashMap::new()),
            replication_state,
            _replication_gc_task: replication_gc_task,
            distribution_state,
            distribution_selection_lock: Mutex::new(()),
        }
    }

    fn engine_pressure_plan(
        &self,
        workers: &[Arc<dyn Worker>],
        healthy_indices: &[usize],
    ) -> Option<EnginePressurePlan> {
        if !self.config.engine_load {
            return None;
        }

        let loads = self.engine_loads.read();
        struct RawWorkerPressure {
            idx: usize,
            backend_pressure: f64,
            scheduler_pressure: Option<f64>,
            waiting_requests: i64,
        }

        let mut raw_pressure = Vec::with_capacity(healthy_indices.len());

        // Degrade the whole decision to legacy request-count routing unless all
        // candidates have comparable, fresh engine telemetry.
        for &idx in healthy_indices {
            let worker = &workers[idx];
            let load = loads.get(worker.url())?;
            if load.observed_at.elapsed() > ENGINE_LOAD_MAX_AGE || load.response.loads.is_empty() {
                return None;
            }

            let backend_pressure = load
                .response
                .loads
                .iter()
                .map(|rank| rank.token_usage.max(rank.utilization))
                .fold(0.0_f64, f64::max);
            if !backend_pressure.is_finite() || backend_pressure < 0.0 {
                return None;
            }
            let running_requests: i64 = load
                .response
                .loads
                .iter()
                .map(|rank| i64::from(rank.num_running_reqs.max(0)))
                .sum();
            let waiting_requests = load
                .response
                .loads
                .iter()
                .map(|rank| i64::from(rank.num_waiting_reqs.max(0)))
                .sum();
            let reported_max_running = load.response.loads.iter().try_fold(0_i64, |total, rank| {
                let rank_cap = i64::from(rank.max_running_requests);
                (rank_cap > 0).then(|| total.saturating_add(rank_cap))
            });
            let registered_single_rank_cap =
                if load.response.dp_rank_count == 1 && load.response.loads.len() == 1 {
                    let rank = &load.response.loads[0];
                    let running = rank.num_running_reqs;
                    let waiting = rank.num_waiting_reqs;
                    // Upstream SGLang `/get_load` reports one canonical DP
                    // snapshot and `combine_sglang_standard_loads` preserves its
                    // total request count. Older versions omit only the cap, which
                    // discovery still reports exactly. The Prometheus fallback,
                    // by contrast, leaves `num_total_reqs` at zero and can contain
                    // TP-duplicated request gauges. Require the canonical count
                    // identity so metadata never turns duplicated gauges into
                    // scheduler occupancy. The all-zero idle case is unambiguous.
                    if rank.max_running_requests == 0
                        && running >= 0
                        && waiting >= 0
                        && rank.num_total_reqs == running.saturating_add(waiting)
                    {
                        worker.max_running_requests().map(i64::from)
                    } else {
                        None
                    }
                } else {
                    None
                };
            let max_running_requests = reported_max_running.or(registered_single_rank_cap);
            let scheduler_pressure = max_running_requests.map(|max_running| {
                (running_requests.saturating_add(waiting_requests) as f64 / max_running as f64)
                    .clamp(0.0, 1.0)
            });

            raw_pressure.push(RawWorkerPressure {
                idx,
                backend_pressure,
                scheduler_pressure,
                waiting_requests,
            });
        }

        // Scheduler counts and caps must be comparable across the entire
        // candidate set. In particular, SGLang's Prometheus-only fallback can
        // repeat request gauges for every TP rank while omitting the cap. A
        // missing cap therefore disables scheduler occupancy for this decision
        // instead of making that worker look artificially saturated.
        let use_scheduler_pressure = raw_pressure
            .iter()
            .all(|load| load.scheduler_pressure.is_some());
        let mut pressure_by_index = HashMap::with_capacity(raw_pressure.len());
        for load in raw_pressure {
            let pressure = if use_scheduler_pressure {
                load.backend_pressure
                    .max(load.scheduler_pressure.unwrap_or_default())
            } else {
                load.backend_pressure
            };

            pressure_by_index.insert(
                load.idx,
                WorkerPressure {
                    pressure,
                    waiting_requests: load.waiting_requests,
                },
            );
        }

        let best_pressure = pressure_by_index
            .values()
            .map(|load| load.pressure)
            .min_by(f64::total_cmp)?;
        let pressure_limit = if best_pressure > ENGINE_PRESSURE_HIGH_WATERMARK {
            best_pressure
        } else {
            (best_pressure + ENGINE_PRESSURE_SLACK).min(ENGINE_PRESSURE_HIGH_WATERMARK)
        };
        let mut allowed_indices: Vec<usize> = healthy_indices
            .iter()
            .copied()
            .filter(|idx| pressure_by_index[idx].pressure <= pressure_limit)
            .collect();
        allowed_indices.sort_by(|&left_idx, &right_idx| {
            let left = pressure_by_index[&left_idx];
            let right = pressure_by_index[&right_idx];
            left.pressure
                .total_cmp(&right.pressure)
                .then_with(|| left.waiting_requests.cmp(&right.waiting_requests))
                .then_with(|| workers[left_idx].load().cmp(&workers[right_idx].load()))
        });
        let best_index = *allowed_indices.first()?;

        Some(EnginePressurePlan {
            allowed_indices,
            best_index,
            pressure_by_index,
        })
    }

    /// Return cached owners that are safe to receive another request.
    ///
    /// Pressure is evaluated only within the owner set. A cooler unrelated
    /// worker must not invalidate a usable cached owner and turn a fleet-wide
    /// KV spread into prefix churn. Missing or stale telemetry fails open to
    /// the known owners, preserving cache affinity until comparable snapshots
    /// return.
    fn suitable_cached_owners(
        workers: &[Arc<dyn Worker>],
        owner_indices: &[usize],
        pressure_plan: Option<&EnginePressurePlan>,
    ) -> Vec<usize> {
        let Some(plan) = pressure_plan else {
            return owner_indices.to_vec();
        };
        let Some(best_pressure) = owner_indices
            .iter()
            .filter_map(|idx| plan.pressure_by_index.get(idx).map(|load| load.pressure))
            .min_by(f64::total_cmp)
        else {
            return owner_indices.to_vec();
        };
        if best_pressure > ENGINE_PRESSURE_HIGH_WATERMARK {
            return Vec::new();
        }

        let pressure_limit =
            (best_pressure + ENGINE_PRESSURE_SLACK).min(ENGINE_PRESSURE_HIGH_WATERMARK);
        let mut suitable: Vec<usize> = owner_indices
            .iter()
            .copied()
            .filter(|idx| plan.pressure_by_index[idx].pressure <= pressure_limit)
            .collect();
        suitable.sort_by(|&left_idx, &right_idx| {
            let left = plan.pressure_by_index[&left_idx];
            let right = plan.pressure_by_index[&right_idx];
            left.pressure
                .total_cmp(&right.pressure)
                .then_with(|| left.waiting_requests.cmp(&right.waiting_requests))
                .then_with(|| workers[left_idx].load().cmp(&workers[right_idx].load()))
        });
        suitable
    }

    /// Enable request-hot-path `hash_index` population. Called by mesh
    /// wiring when the policy is attached to a mesh adapter; otherwise
    /// the index stays empty (its only readers are mesh-only paths).
    pub fn set_populate_hash_index(&self, enabled: bool) {
        self.populate_hash_index.store(enabled, Ordering::Relaxed);
    }

    fn should_populate_hash_index(&self) -> bool {
        self.populate_hash_index.load(Ordering::Relaxed)
    }

    /// Set event-driven KV cache monitor (thread-safe, can be called after construction).
    /// Uses interior mutability so this works on policies behind `Arc<dyn LoadBalancingPolicy>`.
    pub fn set_kv_event_monitor(&self, monitor: Option<Arc<KvEventMonitor>>) {
        *self.kv_monitor.write() = monitor;
    }

    /// Set the backend load-snapshot receiver (thread-safe, after construction).
    /// Wired from the `WorkerMonitor` via the `PolicyRegistry` so the KV-usage
    /// imbalance trigger can read fresh per-worker `token_usage`.
    pub fn set_load_receiver(&self, rx: Option<LoadReceiver>) {
        *self.load_rx.write() = rx;
    }

    /// Disabled eviction is required for the singleton distribution proof so
    /// an owner cannot disappear between selection and commit. Fail closed
    /// once the configured per-tenant tree bound is crossed instead of letting
    /// that canary-only mode grow without limit. One accepted request can
    /// overshoot the bound by at most the router's payload limit; the next
    /// guarded selection is rejected before another insertion.
    fn distribution_index_within_bound(&self, model_id: &str) -> bool {
        self.string_trees.get(model_id).is_none_or(|tree| {
            tree.tenant_char_count
                .iter()
                .all(|entry| *entry.value() <= self.config.max_tree_size)
        })
    }

    /// Begin one exact HTTP cache-distribution decision.
    ///
    /// This is intentionally not part of [`LoadBalancingPolicy`]. Callers must
    /// opt in by downcasting a cache-aware policy and must provide an exact
    /// URL-plus-generation-plus-revision allowlist produced by the admission layer. Acquiring a
    /// lease is read-only with respect to the prefix trees, replication
    /// cooldown, worker load, and fallback work reservations.
    pub(crate) fn begin_distribution_lease(
        &self,
        model_id: &str,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo<'_>,
        partition: &str,
        allowed_targets: &[(String, u64, u64)],
    ) -> Option<(usize, CacheDistributionLease)> {
        let _selection_guard = self.distribution_selection_lock.lock();
        if !self.config.engine_load
            || self.config.max_cached_owners_per_prefix == 0
            || self.config.cache_owner_spill_cooldown_secs == 0
            || partition.is_empty()
            || info.tokens.is_some()
        {
            return None;
        }
        let text = info.request_text?;
        if let Some(value) = info
            .headers
            .and_then(|headers| headers.get(ADMISSION_PARTITION_HEADER))
        {
            if value.to_str().ok()?.trim() != partition {
                return None;
            }
        }
        // Ready-but-circuit-open workers still carry authoritative prefix and
        // pressure evidence. Keep them in the observed set, while exact target
        // filtering below independently requires `can_execute`.
        let observed_indices = Self::distribution_observed_indices(workers);
        let first_idx = *observed_indices.first()?;
        let tree_model_id = normalize_model_key(workers[first_idx].model_id());
        if normalize_model_key(model_id) != tree_model_id
            || observed_indices
                .iter()
                .any(|&idx| normalize_model_key(workers[idx].model_id()) != tree_model_id)
        {
            return None;
        }
        if !self.distribution_index_within_bound(tree_model_id) {
            Metrics::record_cache_aware_replication_decision(
                tree_model_id,
                "distribution_index_bound_exceeded",
                0,
            );
            return None;
        }

        let exact_candidates =
            Self::exact_distribution_candidates(workers, info, &observed_indices, allowed_targets)?;
        if exact_candidates.is_empty() {
            return None;
        }
        let pressure_plan = self.engine_pressure_plan(workers, &observed_indices)?;
        let ownership = self.prefix_ownership(workers, info, &observed_indices, tree_model_id);
        let known_owners = self.owners_with_provisional(&ownership, workers, &observed_indices);
        if known_owners.is_empty() {
            return None;
        }

        let routeable_owners = Self::routeable_distribution_owners(workers, &known_owners);
        let suitable_owners =
            Self::suitable_cached_owners(workers, &routeable_owners, Some(&pressure_plan));
        let existing_candidates: Vec<usize> = exact_candidates
            .iter()
            .copied()
            .filter(|idx| suitable_owners.contains(idx))
            .collect();
        let (target_idx, creates_owner) = if let Some(idx) =
            Self::best_distribution_candidate(workers, &existing_candidates, &pressure_plan)
        {
            (idx, false)
        } else {
            if !suitable_owners.is_empty()
                || known_owners.len() >= self.config.max_cached_owners_per_prefix
            {
                return None;
            }
            let new_owner_candidates: Vec<usize> = exact_candidates
                .iter()
                .copied()
                .filter(|idx| {
                    !known_owners.contains(idx)
                        && pressure_plan.allowed_indices.contains(idx)
                        && pressure_plan.pressure_by_index[idx].pressure
                            <= ENGINE_PRESSURE_HIGH_WATERMARK
                })
                .collect();
            (
                Self::best_distribution_candidate(workers, &new_owner_candidates, &pressure_plan)?,
                true,
            )
        };

        let target_generation_id = workers[target_idx].generation_id();
        let target_revision = workers[target_idx].revision();
        if !allowed_targets
            .iter()
            .any(|(url, generation_id, revision)| {
                url == workers[target_idx].url()
                    && *generation_id == target_generation_id
                    && *revision == target_revision
            })
        {
            return None;
        }
        let lease = self.acquire_distribution_lease(
            partition,
            ownership.key,
            tree_model_id,
            workers[target_idx].as_ref(),
            text,
            creates_owner,
        )?;
        Some((target_idx, lease))
    }

    /// Record fallback work against an exact target selected by a valid
    /// distribution lease, without rerunning worker selection.
    pub(crate) fn reserve_distribution_target_work(
        &self,
        target: &dyn Worker,
        info: &SelectWorkerInfo<'_>,
    ) -> u64 {
        let cost = self.fallback.reserve_exact_worker(target.url(), info);
        target.increment_processed();
        cost
    }

    /// Revalidate an exact lease immediately before dispatch.
    ///
    /// The current policy instance, pending lease record, request, target
    /// object, target revision, routing constraints, prefix condition, and
    /// engine pressure must all still match. This method does not mutate any
    /// cache-policy or worker state.
    pub(crate) fn validate_distribution_lease(
        &self,
        model_id: &str,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo<'_>,
        expected_worker: &Arc<dyn Worker>,
        lease: &CacheDistributionLease,
    ) -> bool {
        if !Arc::ptr_eq(&self.distribution_state, &lease.state)
            || self.distribution_state.policy_instance_id != lease.policy_instance_id
            || normalize_model_key(model_id) != lease.model_id.as_ref()
            || info.tokens.is_some()
            || !info
                .request_text
                .is_some_and(|text| lease.request_matches(text))
            || !lease.pending_matches()
        {
            return false;
        }
        let Some(target_idx) = Self::exact_lease_target_index(workers, expected_worker, lease)
        else {
            return false;
        };
        let observed_indices = Self::distribution_observed_indices(workers);
        if !observed_indices.contains(&target_idx)
            || normalize_model_key(workers[target_idx].model_id()) != lease.model_id.as_ref()
            || SizeAwarePowerOfTwoPolicy::eligible_candidates(workers, info, &[target_idx])
                != [target_idx]
        {
            return false;
        }
        let Some(pressure_plan) = self.engine_pressure_plan(workers, &observed_indices) else {
            return false;
        };
        let ownership = self.prefix_ownership(workers, info, &observed_indices, &lease.model_id);
        if ownership.key != lease.prefix_key {
            return false;
        }
        let known_owners = self.owners_with_provisional(&ownership, workers, &observed_indices);
        if known_owners.is_empty() {
            return false;
        }
        let routeable_owners = Self::routeable_distribution_owners(workers, &known_owners);
        let suitable_owners =
            Self::suitable_cached_owners(workers, &routeable_owners, Some(&pressure_plan));

        if known_owners.contains(&target_idx) {
            return suitable_owners.contains(&target_idx);
        }
        lease.creates_owner
            && suitable_owners.is_empty()
            && known_owners.len() < self.config.max_cached_owners_per_prefix
            && pressure_plan.allowed_indices.contains(&target_idx)
            && pressure_plan.pressure_by_index[&target_idx].pressure
                <= ENGINE_PRESSURE_HIGH_WATERMARK
    }

    /// Commit durable HTTP prefix ownership after a successful backend call.
    ///
    /// Commit intentionally rechecks only immutable binding and current target
    /// identity. The owner-pressure condition was checked immediately before
    /// dispatch, while a successful response is itself evidence that this exact
    /// target now owns the routed prefix.
    pub(crate) fn commit_distribution_success(
        &self,
        model_id: &str,
        workers: &[Arc<dyn Worker>],
        request_text: &str,
        expected_worker: &Arc<dyn Worker>,
        lease: &mut CacheDistributionLease,
    ) -> bool {
        let _selection_guard = self.distribution_selection_lock.lock();
        if !Arc::ptr_eq(&self.distribution_state, &lease.state)
            || self.distribution_state.policy_instance_id != lease.policy_instance_id
            || normalize_model_key(model_id) != lease.model_id.as_ref()
            || !lease.request_matches(request_text)
            || !lease.pending_matches()
            || Self::exact_lease_target_index(workers, expected_worker, lease).is_none()
        {
            return false;
        }

        let Some(tree) = self
            .string_trees
            .get(lease.model_id.as_ref())
            .map(|entry| entry.value().clone())
        else {
            return false;
        };
        let result = tree.match_and_insert_with(request_text, |_| Some(lease.target_url.as_ref()));
        if self.should_populate_hash_index() {
            let matched_prefix: String = request_text
                .chars()
                .take(result.matched_char_count)
                .collect();
            self.hash_index
                .entry(lease.model_id.to_string())
                .or_default()
                .string_tree
                .insert(kv_index::hash_node_path(request_text), matched_prefix);
        }
        if lease.creates_owner {
            self.replication_state.insert(
                lease.prefix_key,
                PrefixReplicationState {
                    last_spill: Instant::now(),
                    provisional_owner: lease.target_url.to_string(),
                },
            );
        }
        lease.finish()
    }

    fn acquire_distribution_lease(
        &self,
        partition: &str,
        prefix_key: PrefixBudgetKey,
        model_id: &str,
        target: &dyn Worker,
        request_text: &str,
        creates_owner: bool,
    ) -> Option<CacheDistributionLease> {
        let lease_id = self.distribution_state.next_lease_id()?;
        let model_id: Arc<str> = Arc::from(model_id);
        let target_url: Arc<str> = Arc::from(target.url());
        let target_generation_id = target.generation_id();
        let target_revision = target.revision();
        let partition: Arc<str> = Arc::from(partition);
        let request_digest = blake3::hash(request_text.as_bytes());
        let request_len = request_text.len();
        let pending = PendingDistributionSeed {
            lease_id,
            prefix_key,
            model_id: Arc::clone(&model_id),
            target_url: Arc::clone(&target_url),
            target_generation_id,
            target_revision,
            request_digest,
            request_len,
            creates_owner,
        };
        match self
            .distribution_state
            .pending_by_partition
            .entry(partition.to_string())
        {
            Entry::Occupied(_) => None,
            Entry::Vacant(entry) => {
                entry.insert(pending);
                Some(CacheDistributionLease {
                    state: Arc::clone(&self.distribution_state),
                    policy_instance_id: self.distribution_state.policy_instance_id,
                    lease_id,
                    partition,
                    prefix_key,
                    model_id,
                    target_url,
                    target_generation_id,
                    target_revision,
                    request_digest,
                    request_len,
                    creates_owner,
                    finished: false,
                })
            }
        }
    }

    fn distribution_observed_indices(workers: &[Arc<dyn Worker>]) -> Vec<usize> {
        workers
            .iter()
            .enumerate()
            .filter_map(|(idx, worker)| {
                let state = worker.routing_state();
                state.healthy.then_some(idx)
            })
            .collect()
    }

    fn routeable_distribution_owners(
        workers: &[Arc<dyn Worker>],
        owner_indices: &[usize],
    ) -> Vec<usize> {
        owner_indices
            .iter()
            .copied()
            .filter(|&idx| workers[idx].is_available())
            .collect()
    }

    fn exact_distribution_candidates(
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo<'_>,
        healthy_indices: &[usize],
        allowed_targets: &[(String, u64, u64)],
    ) -> Option<Vec<usize>> {
        let mut allowed_by_url = HashMap::with_capacity(allowed_targets.len());
        for (url, generation_id, revision) in allowed_targets {
            if url.is_empty() {
                return None;
            }
            if let Some(previous) = allowed_by_url.insert(url.as_str(), (*generation_id, *revision))
            {
                if previous != (*generation_id, *revision) {
                    return None;
                }
            }
        }

        let mut matched_by_url = HashMap::with_capacity(allowed_by_url.len());
        for &idx in healthy_indices {
            let worker = &workers[idx];
            if allowed_by_url.get(worker.url()).copied()
                != Some((worker.generation_id(), worker.revision()))
            {
                continue;
            }
            if matched_by_url.insert(worker.url(), idx).is_some() {
                // An exact URL and revision must identify one worker object.
                return None;
            }
        }
        let candidate_indices: Vec<usize> = matched_by_url.into_values().collect();
        Some(SizeAwarePowerOfTwoPolicy::eligible_candidates(
            workers,
            info,
            &candidate_indices,
        ))
    }

    fn best_distribution_candidate(
        workers: &[Arc<dyn Worker>],
        candidate_indices: &[usize],
        pressure_plan: &EnginePressurePlan,
    ) -> Option<usize> {
        candidate_indices.iter().copied().min_by(|&left, &right| {
            let left_pressure = pressure_plan.pressure_by_index[&left];
            let right_pressure = pressure_plan.pressure_by_index[&right];
            left_pressure
                .pressure
                .total_cmp(&right_pressure.pressure)
                .then_with(|| {
                    left_pressure
                        .waiting_requests
                        .cmp(&right_pressure.waiting_requests)
                })
                .then_with(|| workers[left].load().cmp(&workers[right].load()))
                .then_with(|| workers[left].url().cmp(workers[right].url()))
        })
    }

    fn exact_lease_target_index(
        workers: &[Arc<dyn Worker>],
        expected_worker: &Arc<dyn Worker>,
        lease: &CacheDistributionLease,
    ) -> Option<usize> {
        if expected_worker.url() != lease.target_url.as_ref()
            || expected_worker.generation_id() != lease.target_generation_id
            || expected_worker.revision() != lease.target_revision
            || !expected_worker.is_available()
        {
            return None;
        }
        let mut matches = workers.iter().enumerate().filter(|(_, worker)| {
            worker.url() == lease.target_url.as_ref()
                && worker.generation_id() == lease.target_generation_id
                && worker.revision() == lease.target_revision
                && worker.is_available()
        });
        let (idx, current) = matches.next()?;
        if matches.next().is_some() || !Arc::ptr_eq(current, expected_worker) {
            return None;
        }
        Some(idx)
    }

    /// True when the pool is imbalanced enough to abandon cache affinity.
    ///
    /// Four independent triggers, OR'd together. The two KV-based triggers
    /// require a backend `token_usage` snapshot and are disabled at their `1.0`
    /// default (utilization and spread are both `<= 1.0`, so `> 1.0` never
    /// fires):
    ///
    /// - **overload** (`overload_token_usage_threshold`): the hottest engine's
    ///   KV utilization exceeds the ceiling — a critically-saturated engine,
    ///   shed regardless of balance. Set high (e.g. 0.9) as a safety valve.
    /// - **KV spread** (`balance_token_usage_threshold`): the hottest engine is
    ///   materially more KV-saturated than the coldest, i.e. a cooler engine
    ///   exists to spill toward. This is the true balance signal for long-context
    ///   workloads, and — unlike request counts, which each gateway sees only
    ///   locally — it is invariant to the number of gateway replicas.
    /// - **reserved-work spread**: router-local prompt plus expected-output
    ///   reservations differ by more than one fallback output-work quantum and
    ///   exceed the relative threshold. This catches long prompts and bursts
    ///   synchronously, before backend polling or active-request guards update.
    /// - **count spread**: request-count dispersion (abs AND rel) over healthy
    ///   workers. Always evaluated, so high-count / low-KV imbalance is still
    ///   caught when KV looks even.
    /// Whether to abandon cache affinity for shortest-queue because the pool is
    /// imbalanced — by backend KV usage (overload ceiling or hot-vs-cool spread)
    /// or by request-count spread. `min_load`/`max_load` are the request-count
    /// bounds over the healthy workers, which `select_worker` gathers in its
    /// single worker pass (tests use the `imbalanced` helper to fold them).
    fn imbalance_reason(
        &self,
        workers: &[Arc<dyn Worker>],
        healthy_indices: &[usize],
        min_load: usize,
        max_load: usize,
    ) -> Option<ImbalanceReason> {
        // KV-based triggers — need a load snapshot; both default 1.0 = disabled.
        if let Some((min_usage, max_usage)) =
            self.backend_token_usage_bounds(workers, healthy_indices)
        {
            // Overload: a single engine is critically saturated.
            if max_usage > f64::from(self.config.overload_token_usage_threshold) {
                return Some(ImbalanceReason::BackendOverload);
            }
            // KV imbalance: a hot engine with a materially cooler home.
            if max_usage - min_usage > f64::from(self.config.balance_token_usage_threshold) {
                return Some(ImbalanceReason::BackendSpread);
            }
        }

        if self.fallback.is_reserved_work_imbalanced(
            workers,
            healthy_indices,
            self.config.balance_rel_threshold,
        ) {
            return Some(ImbalanceReason::ReservedWork);
        }

        // Count spread (abs AND rel) over healthy workers.
        if max_load.saturating_sub(min_load) > self.config.balance_abs_threshold
            && (max_load as f32) > (min_load as f32 * self.config.balance_rel_threshold)
        {
            Some(ImbalanceReason::RequestCount)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn is_imbalanced(
        &self,
        workers: &[Arc<dyn Worker>],
        healthy_indices: &[usize],
        min_load: usize,
        max_load: usize,
    ) -> bool {
        self.imbalance_reason(workers, healthy_indices, min_load, max_load)
            .is_some()
    }

    /// Min and max backend KV-cache utilization (0.0–1.0) across healthy workers
    /// that have a `WorkerMonitor` snapshot entry, as `(min, max)`. `None` when
    /// no receiver is wired or no healthy worker has a load entry (→ caller
    /// relies on the request-count spread).
    fn backend_token_usage_bounds(
        &self,
        workers: &[Arc<dyn Worker>],
        healthy_indices: &[usize],
    ) -> Option<(f64, f64)> {
        let guard = self.load_rx.read();
        let rx = guard.as_ref()?;
        let loads = rx.borrow();
        let mut bounds: Option<(f64, f64)> = None;
        for &idx in healthy_indices {
            if let Some(load) = loads.get(workers[idx].url()) {
                let usage = load.effective_token_usage();
                bounds = Some(match bounds {
                    Some((min, max)) => (min.min(usage), max.max(usage)),
                    None => (usage, usage),
                });
            }
        }
        bounds
    }

    /// Initialize the trees with worker URLs (used only during initial setup)
    /// Initializes both string trees (HTTP) and token trees (gRPC) for each model.
    pub fn init_workers(&self, workers: &[Arc<dyn Worker>]) {
        // Group workers by model
        let mut model_workers: HashMap<String, Vec<&Arc<dyn Worker>>> = HashMap::new();
        for worker in workers {
            let tree_key = normalize_model_key(worker.model_id());
            model_workers
                .entry(tree_key.to_string())
                .or_default()
                .push(worker);
        }

        // Initialize trees for each model (both string and token trees)
        for (tree_key, model_workers) in model_workers {
            // Initialize string tree (HTTP)
            let string_tree = self
                .string_trees
                .entry(tree_key.clone())
                .or_insert_with(|| Arc::new(Tree::new()));
            // Initialize token tree (gRPC)
            let token_tree = self
                .token_trees
                .entry(tree_key)
                .or_insert_with(|| Arc::new(TokenTree::new()));

            for worker in model_workers {
                string_tree.insert_text("", worker.url());
                token_tree.insert_tokens(&[], worker.url());
            }
        }
    }

    /// Add a single worker to the trees (incremental update)
    pub fn add_worker(&self, worker: &dyn Worker) {
        let tree_key = normalize_model_key(worker.model_id()).to_string();
        // Add to string tree (HTTP)
        let string_tree = self
            .string_trees
            .entry(tree_key.clone())
            .or_insert_with(|| Arc::new(Tree::new()));
        string_tree.insert_text("", worker.url());
        // Add to token tree (gRPC)
        let token_tree = self
            .token_trees
            .entry(tree_key)
            .or_insert_with(|| Arc::new(TokenTree::new()));
        token_tree.insert_tokens(&[], worker.url());
    }

    /// Add a worker by URL and model (for backward compatibility)
    pub fn add_worker_by_url(&self, url: &str, model_id: &str) {
        let model_id_string = model_id.to_string();
        // Add to string tree (HTTP)
        let string_tree = self
            .string_trees
            .entry(model_id_string.clone())
            .or_insert_with(|| Arc::new(Tree::new()));
        string_tree.insert_text("", url);
        // Add to token tree (gRPC)
        let token_tree = self
            .token_trees
            .entry(model_id_string)
            .or_insert_with(|| Arc::new(TokenTree::new()));
        token_tree.insert_tokens(&[], url);
    }

    /// Remove a worker from the model-scoped trees as soon as it drains.
    pub fn remove_worker(&self, worker: &dyn Worker) {
        self.remove_worker_from_model(worker.model_id(), worker.url());
    }

    pub fn remove_worker_from_model(&self, model_id: &str, url: &str) {
        let model_id = normalize_model_key(model_id);
        let tenant: Arc<str> = Arc::from(url);
        if let Some(tree) = self.string_trees.get(model_id) {
            tree.remove_tenant_all(&tenant);
        }
        if let Some(tree) = self.token_trees.get(model_id) {
            tree.evict_tenant(&tenant, 0);
        }
        self.engine_loads.write().remove(url);
        LoadBalancingPolicy::remove_worker(&self.fallback, url);
    }

    /// Remove a worker by URL from every model tree for PD and legacy callers.
    pub fn remove_worker_by_url(&self, url: &str) {
        let tenant: Arc<str> = Arc::from(url);
        for tree in self.string_trees.iter() {
            tree.remove_tenant_all(&tenant);
        }
        for tree in self.token_trees.iter() {
            tree.evict_tenant(&tenant, 0);
        }
        self.engine_loads.write().remove(url);
        LoadBalancingPolicy::remove_worker(&self.fallback, url);
    }

    /// Run cache eviction to prevent unbounded growth
    pub fn evict_cache(&self, max_size: usize) {
        // Evict string trees (HTTP)
        for tree_ref in self.string_trees.iter() {
            let model_id = tree_ref.key();
            let tree = tree_ref.value();
            tree.evict_tenant_by_size(max_size);
            debug!(
                "String tree eviction for model {}, max_size: {}",
                model_id, max_size
            );
        }
        // Evict token trees (gRPC)
        for tree_ref in self.token_trees.iter() {
            let model_id = tree_ref.key();
            let tree = tree_ref.value();
            tree.evict_tenant_by_size(max_size);
            debug!(
                "Token tree eviction for model {}, max_size: {}",
                model_id, max_size
            );
        }
        // Evict hash index per model per tree kind. `max_size` is a
        // per-tree bound; clearing one model's overflow must not wipe
        // other models' still-valid metadata.
        for entry in self.hash_index.iter() {
            let per_model = entry.value();
            if per_model.string_tree.len() > max_size {
                per_model.string_tree.clear();
                debug!(
                    model_id = entry.key(),
                    "String hash index cleared (exceeded max_size: {})", max_size
                );
            }
            if per_model.token_tree.len() > max_size {
                per_model.token_tree.clear();
                debug!(
                    model_id = entry.key(),
                    "Token hash index cleared (exceeded max_size: {})", max_size
                );
            }
        }
    }

    /// Use size-aware P2C when no suitable cached owner exists, then record the
    /// destination as an additional owner of the routed prefix.
    fn select_worker_fallback(
        &self,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo,
        candidate_indices: &[usize],
        model_id: &str,
    ) -> Option<usize> {
        // Log load balancing trigger (only compute worker loads if debug enabled)
        if tracing::enabled!(tracing::Level::DEBUG) {
            let worker_loads: Vec<(&str, usize)> =
                workers.iter().map(|w| (w.url(), w.load())).collect();
            debug!("Load balancing triggered | workers: {:?}", worker_loads);
        }

        let selected_idx =
            self.fallback
                .select_worker_from_candidates(workers, info, candidate_indices)?;
        let worker_url = workers[selected_idx].url();

        // Even in imbalanced mode, update the appropriate tree to maintain cache state
        // Prefer token tree for gRPC requests, fall back to string tree for HTTP
        if let Some(tokens) = info.tokens {
            // gRPC request: update token tree
            let tree = self
                .token_trees
                .get(model_id)
                .map(|entry| entry.value().clone());
            if let Some(tree) = tree {
                // We need the match result (the prior shared prefix) BEFORE the
                // insert so the hash_index stores only that bounded prefix, not
                // the full path that exists post-insert (32K tokens × 4 bytes ×
                // max_tree_size = multi-GB/model). `match_and_insert` resolves
                // the match against the pre-insert tree and inserts in the SAME
                // descent, so `result.matched_token_count` is the same prior
                // prefix length the standalone match returned. When we don't
                // populate the index, a plain insert (no match) suffices.
                if self.should_populate_hash_index() {
                    let result = tree.match_and_insert(tokens, worker_url);
                    let matched_prefix: Vec<u32> = tokens[..result.matched_token_count].to_vec();
                    self.hash_index
                        .entry(model_id.to_string())
                        .or_default()
                        .token_tree
                        .insert(kv_index::hash_token_path(tokens), matched_prefix);
                } else {
                    tree.insert_tokens(tokens, worker_url);
                }
            }
        } else if let Some(text) = info.request_text {
            // HTTP request: update string tree
            let tree = self
                .string_trees
                .get(model_id)
                .map(|entry| entry.value().clone());

            if let Some(tree) = tree {
                // Match BEFORE insert so the hash_index stores only the prior
                // shared prefix (~50-200 chars), not the full prompt (20KB+)
                // that exists post-insert. `match_and_insert` does both in a
                // single descent; `result.matched_char_count` is the same prior
                // prefix length the standalone match returned. When we don't
                // populate the index, a plain insert (no match) suffices.
                if self.should_populate_hash_index() {
                    let result = tree.match_and_insert(text, worker_url);
                    let matched_prefix: String =
                        text.chars().take(result.matched_char_count).collect();
                    let path_hash = kv_index::hash_node_path(text);
                    self.hash_index
                        .entry(model_id.to_string())
                        .or_default()
                        .string_tree
                        .insert(path_hash, matched_prefix);
                } else {
                    tree.insert_text(text, worker_url);
                }
            } else {
                debug!(
                    "Warning: No string tree found for model '{}', skipping cache update",
                    model_id
                );
            }
        }

        Some(selected_idx)
    }
}

/// Which of the two local trees a hash query targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TreeKind {
    String,
    Token,
}

/// Handle the policy exposes so mesh-adjacent consumers can apply
/// remote tenant inserts against the local tree without reaching
/// into private fields. Defined here (not in the adapter) to keep
/// the dependency direction `adapter → policy`.
pub trait TreeHandle: Send + Sync + std::fmt::Debug {
    /// If `node_hash` is known locally (resolvable to a stored
    /// matched-prefix), record `worker_url` as a tenant of the
    /// matched node and return `true`. Returns `false` if the
    /// hash isn't known — the caller is expected to request
    /// repair so the path can be reconstructed from a peer.
    ///
    /// This subsumes "is the hash known?" plus "apply the
    /// insert": the adapter doesn't need separate read+write
    /// trips, and we never expose the matched value across the
    /// trait boundary (it stays inside the policy where
    /// eviction owns its lifecycle).
    fn apply_known_remote_insert(
        &self,
        model_id: &str,
        tree_kind: TreeKind,
        node_hash: u64,
        worker_url: &str,
    ) -> bool;

    /// Open a stream of `RepairEntry` for one `(model_id,
    /// tree_kind)`, in the deterministic pre-order produced by
    /// the underlying tree's `iter_entries`. Returns `None` if
    /// no tree exists locally for that model. Paging is wire
    /// shape and lives in the adapter, not on this trait — the
    /// stream just yields entries one at a time.
    fn open_repair_stream(
        &self,
        model_id: &str,
        tree_kind: TreeKind,
    ) -> Option<Box<dyn Iterator<Item = RepairEntry> + Send>>;

    /// Apply every entry in `page` to the local `(model_id,
    /// tree_kind)` tree, creating the tree if it doesn't yet
    /// exist locally. Returns the number of entries successfully
    /// applied (entries whose variant doesn't match `tree_kind`
    /// are logged and skipped, not applied). Idempotent —
    /// reapplying the same page is a no-op on the tree state
    /// because the underlying radix tree's `insert_text` /
    /// `insert_tokens` are themselves idempotent for the same
    /// `(path, tenant)` pair.
    fn apply_repair_page(&self, page: &TreeRepairPage) -> usize;
}

impl TreeHandle for CacheAwarePolicy {
    fn apply_known_remote_insert(
        &self,
        model_id: &str,
        tree_kind: TreeKind,
        node_hash: u64,
        worker_url: &str,
    ) -> bool {
        // Normalize empty → UNKNOWN_MODEL_ID so lookups match the
        // key shape every populate site already uses.
        let model_id = normalize_model_key(model_id);
        let Some(model_entry) = self.hash_index.get(model_id) else {
            return false;
        };
        match tree_kind {
            TreeKind::String => {
                let Some(path) = model_entry.string_tree.get(&node_hash) else {
                    return false;
                };
                let Some(tree) = self.string_trees.get(model_id) else {
                    // Hash index entry without a corresponding
                    // tree means a populate site mutated
                    // `hash_index` without creating the tree
                    // (or eviction dropped the tree but left the
                    // index). Returning false here masks the
                    // invariant violation as a spurious repair
                    // request, so log loudly.
                    warn!(
                        model_id,
                        node_hash,
                        "string hash_index entry without matching string_trees entry; populate-site invariant violated",
                    );
                    return false;
                };
                tree.insert_text(path.value(), worker_url);
                true
            }
            TreeKind::Token => {
                let Some(tokens) = model_entry.token_tree.get(&node_hash) else {
                    return false;
                };
                let Some(tree) = self.token_trees.get(model_id) else {
                    warn!(
                        model_id,
                        node_hash,
                        "token hash_index entry without matching token_trees entry; populate-site invariant violated",
                    );
                    return false;
                };
                tree.insert_tokens(tokens.value(), worker_url);
                true
            }
        }
    }

    fn open_repair_stream(
        &self,
        model_id: &str,
        tree_kind: TreeKind,
    ) -> Option<Box<dyn Iterator<Item = RepairEntry> + Send>> {
        let model_id = normalize_model_key(model_id);
        match tree_kind {
            TreeKind::String => {
                let tree = self.string_trees.get(model_id)?.value().clone();
                Some(Box::new(tree.iter_entries().map(|(path, tenants)| {
                    RepairEntry::String { path, tenants }
                })))
            }
            TreeKind::Token => {
                let tree = self.token_trees.get(model_id)?.value().clone();
                Some(Box::new(tree.iter_entries().map(|(tokens, tenants)| {
                    RepairEntry::Token { tokens, tenants }
                })))
            }
        }
    }

    fn apply_repair_page(&self, page: &TreeRepairPage) -> usize {
        let model_id = normalize_model_key(&page.model_id);
        let mut applied: usize = 0;
        match page.tree_kind {
            TreeKind::String => {
                // Create the tree on first repair page if it
                // doesn't exist yet locally — repair is the
                // primary cold-start path for a fresh peer.
                let tree = self
                    .string_trees
                    .entry(model_id.to_string())
                    .or_insert_with(|| Arc::new(Tree::new()))
                    .clone();
                for entry in &page.entries {
                    match entry {
                        RepairEntry::String { path, tenants } => {
                            for (tenant, _epoch) in tenants {
                                tree.insert_text(path, tenant);
                            }
                            self.hash_index
                                .entry(model_id.to_string())
                                .or_default()
                                .string_tree
                                .insert(kv_index::hash_node_path(path), path.clone());
                            applied += 1;
                        }
                        RepairEntry::Token { .. } => {
                            warn!(
                                model_id,
                                session_id = %page.session_id,
                                page_index = page.page_index,
                                "RepairEntry variant mismatch: page kind=String but entry kind=Token; skipping",
                            );
                        }
                    }
                }
            }
            TreeKind::Token => {
                let tree = self
                    .token_trees
                    .entry(model_id.to_string())
                    .or_insert_with(|| Arc::new(TokenTree::new()))
                    .clone();
                for entry in &page.entries {
                    match entry {
                        RepairEntry::Token { tokens, tenants } => {
                            for (tenant, _epoch) in tenants {
                                tree.insert_tokens(tokens, tenant);
                            }
                            self.hash_index
                                .entry(model_id.to_string())
                                .or_default()
                                .token_tree
                                .insert(kv_index::hash_token_path(tokens), tokens.clone());
                            applied += 1;
                        }
                        RepairEntry::String { .. } => {
                            warn!(
                                model_id,
                                session_id = %page.session_id,
                                page_index = page.page_index,
                                "RepairEntry variant mismatch: page kind=Token but entry kind=String; skipping",
                            );
                        }
                    }
                }
            }
        }
        applied
    }
}

impl LoadBalancingPolicy for CacheAwarePolicy {
    fn select_worker(&self, workers: &[Arc<dyn Worker>], info: &SelectWorkerInfo) -> Option<usize> {
        let _distribution_selection_guard = info
            .forbid_unleased_cache_owner_expansion
            .then(|| self.distribution_selection_lock.lock());
        let request_text = info.request_text;
        let request_tokens = info.tokens;

        // Single O(workers) gather: read each worker once via routing_state()
        // (status + load + processed under one ArcSwap guard), replacing the
        // former separate passes whose per-worker guard traffic dominated routing
        // CPU at scale. Collects healthy indices and load min/max; cache-owner
        // lookup is a hash-free scan over healthy_indices.
        let mut healthy_indices: Vec<usize> = Vec::with_capacity(workers.len());
        let mut min_load = usize::MAX;
        let mut max_load = 0usize;
        for (idx, worker) in workers.iter().enumerate() {
            let state = worker.routing_state();
            if state.healthy && state.can_execute {
                healthy_indices.push(idx);
                min_load = min_load.min(state.load);
                max_load = max_load.max(state.load);
            }
        }

        if healthy_indices.is_empty() {
            return None;
        }
        let min_load = if min_load == usize::MAX { 0 } else { min_load };

        // The router pre-filters workers by model, so any healthy worker gives
        // us the model key before engine pressure narrows the candidate set.
        let model_id = normalize_model_key(workers[healthy_indices[0]].model_id());

        if info.forbid_unleased_cache_owner_expansion
            && !self.distribution_index_within_bound(model_id)
        {
            Metrics::record_cache_aware_replication_decision(
                model_id,
                "distribution_index_bound_exceeded",
                0,
            );
            return None;
        }

        // Distribution-enabled ordinary traffic may consume only ownership
        // that was already committed before this selection. Include healthy
        // circuit-open workers in the ownership lookup so an unavailable owner
        // cannot be mistaken for a cold prefix and silently replicated. This
        // early return is the tri-state "blocked" boundary: `None` must not
        // fall through to either pressure-spill or fleet-wide fallback.
        let observed_indices = info
            .forbid_unleased_cache_owner_expansion
            .then(|| Self::distribution_observed_indices(workers));
        let ownership_indices = observed_indices.as_deref().unwrap_or(&healthy_indices);
        let guarded_ownership = self.prefix_ownership(workers, info, ownership_indices, model_id);
        if info.forbid_unleased_cache_owner_expansion && guarded_ownership.has_known_owner {
            let committed_owners = SizeAwarePowerOfTwoPolicy::eligible_candidates(
                workers,
                info,
                &guarded_ownership.owners,
            );
            let result = if committed_owners.is_empty() {
                "unleased_owner_expansion_blocked"
            } else {
                "committed_owner_only"
            };
            Metrics::record_cache_aware_replication_decision(
                model_id,
                result,
                guarded_ownership.owners.len(),
            );
            return self.select_from_cached_owners(workers, info, &committed_owners, model_id);
        }

        // A distribution seed is the only request allowed to reach its exact
        // clean-peer target until that request proves successful ownership.
        // Keep ordinary traffic on committed owners while the seed is pending.
        // Returning here, including `None` when no committed owner can execute,
        // also prevents the ordinary pressure-spill fallback from bypassing the
        // one-in-flight distribution permit.
        let pending_ownership = guarded_ownership;
        if self.has_pending_distribution_seed(pending_ownership.key, info, model_id) {
            let committed_owners = SizeAwarePowerOfTwoPolicy::eligible_candidates(
                workers,
                info,
                &pending_ownership.owners,
            );
            Metrics::record_cache_aware_replication_decision(
                model_id,
                "distribution_pending_owner_hold",
                pending_ownership.owners.len(),
            );
            return self.select_from_cached_owners(workers, info, &committed_owners, model_id);
        }
        let pressure_plan = self.engine_pressure_plan(workers, &healthy_indices);

        // Prefix ownership is evaluated before fleet-wide imbalance. This is
        // the critical locality invariant: a hot unrelated worker cannot make
        // a usable cached owner disappear. Only pressure on every matching
        // owner may create one controlled additional owner.
        if let Some(selected_idx) = self.select_cached_owner_or_pressure_spill(
            workers,
            info,
            &healthy_indices,
            model_id,
            pressure_plan.as_ref(),
        ) {
            let result = if pressure_plan.is_some() {
                "cached_owner_scoped"
            } else {
                "telemetry_fallback"
            };
            if self.config.engine_load {
                Metrics::record_cache_aware_engine_decision(result);
            }
            return Some(selected_idx);
        }

        let selection_indices = match pressure_plan.as_ref() {
            Some(plan) => &plan.allowed_indices,
            _ => &healthy_indices,
        };

        // Engine pressure is the outer safety filter. When it narrows the
        // candidate set, recompute upstream's imbalance bounds inside that set
        // so fallback routing cannot bypass the guard.
        // With engine pressure disabled or unavailable, reuse the original
        // single-pass values and preserve upstream behavior exactly.
        let (selection_min_load, selection_max_load) = if pressure_plan.is_some() {
            let mut filtered_min_load = usize::MAX;
            let mut filtered_max_load = 0usize;
            for &idx in selection_indices {
                let state = workers[idx].routing_state();
                filtered_min_load = filtered_min_load.min(state.load);
                filtered_max_load = filtered_max_load.max(state.load);
            }
            (
                if filtered_min_load == usize::MAX {
                    0
                } else {
                    filtered_min_load
                },
                filtered_max_load,
            )
        } else {
            (min_load, max_load)
        };

        // Apply upstream's imbalance and cache-affinity behavior only within
        // the candidates admitted by the engine-pressure guard.
        let imbalance_reason = self.imbalance_reason(
            workers,
            selection_indices,
            selection_min_load,
            selection_max_load,
        );
        let selected_idx = if let Some(reason) = imbalance_reason {
            self.select_worker_imbalanced_with_budget(
                workers,
                info,
                &healthy_indices,
                selection_indices,
                model_id,
                reason,
            )
        } else if let Some(tokens) = request_tokens {
            if self.has_event_indexer(model_id) {
                self.select_worker_event_driven(workers, tokens, selection_indices, info, model_id)
            } else {
                self.select_worker_with_tokens(workers, tokens, selection_indices, info, model_id)
            }
        } else {
            self.select_worker_with_text(
                workers,
                request_text.unwrap_or(""),
                selection_indices,
                info,
                model_id,
            )
        }?;

        if let Some(plan) = pressure_plan.as_ref() {
            let result = if plan.allowed_indices.len() == healthy_indices.len() {
                "all_candidates_allowed"
            } else {
                "candidate_set_restricted"
            };
            Metrics::record_cache_aware_engine_decision(result);
            debug!(
                selected_worker = workers[selected_idx].url(),
                selected_pressure = plan.pressure_by_index[&selected_idx].pressure,
                least_pressure_worker = workers[plan.best_index].url(),
                least_pressure = plan.pressure_by_index[&plan.best_index].pressure,
                result,
                "Cache-aware engine-pressure decision"
            );
        } else if self.config.engine_load {
            Metrics::record_cache_aware_engine_decision("telemetry_fallback");
        }

        Some(selected_idx)
    }

    fn update_loads(&self, loads: &HashMap<String, WorkerLoadResponse>) {
        let observed_at = Instant::now();
        let mut cached = self.engine_loads.write();
        for (url, response) in loads {
            cached.insert(
                url.clone(),
                TimedWorkerLoad {
                    response: response.clone(),
                    observed_at,
                },
            );
        }
    }

    fn update_loads_for_workers(
        &self,
        loads: &HashMap<String, WorkerLoadResponse>,
        worker_urls: &[String],
    ) {
        self.update_loads(loads);
        self.engine_loads
            .write()
            .retain(|url, _| !worker_urls.contains(url) || loads.contains_key(url));
    }

    fn needs_load_updates(&self) -> bool {
        self.config.engine_load
    }

    fn on_request_complete(&self, worker_url: &str, success: bool) {
        // Could track success rates per worker for more intelligent routing
        if !success {
            // Optionally reduce affinity for failed requests
            tracing::debug!(
                "Request to {} completed with success={}",
                worker_url,
                success
            );
        }
    }

    fn reservation_cost(&self, info: &SelectWorkerInfo<'_>) -> Option<u64> {
        self.fallback.reservation_cost(info)
    }

    fn release_reservation(&self, worker_url: &str, cost: u64) {
        self.fallback.release_reservation(worker_url, cost);
    }

    fn remove_worker(&self, url: &str) {
        self.remove_worker_by_url(url);
    }

    fn name(&self) -> &'static str {
        "cache_aware"
    }

    fn needs_request_text(&self) -> bool {
        true // Cache-aware policy needs request text for cache affinity
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// Private helper methods for select_worker
impl CacheAwarePolicy {
    /// Check if an event-driven indexer exists with data for this model.
    /// Returns false when the indexer is empty (startup, reconnect) so
    /// routing falls through to the approximate token tree instead of
    /// taking the event-driven path with no data and landing on min-load.
    fn has_event_indexer(&self, model_id: &str) -> bool {
        let guard = self.kv_monitor.read();
        guard
            .as_ref()
            .and_then(|m| m.get_indexer(model_id))
            .is_some_and(|indexer| indexer.current_size() > 0)
    }

    fn token_tree_ownership(
        &self,
        workers: &[Arc<dyn Worker>],
        tokens: &[u32],
        healthy_indices: &[usize],
        model_id: &str,
    ) -> PrefixOwnership {
        let mut owners = Vec::new();
        let mut matched_tokens = 0usize;
        let mut has_known_owner = false;
        if let Some(tree) = self
            .token_trees
            .get(model_id)
            .map(|entry| entry.value().clone())
        {
            let result = tree.match_prefix_with_counts(tokens);
            let match_rate = if result.input_token_count == 0 {
                0.0
            } else {
                result.matched_token_count as f32 / result.input_token_count as f32
            };
            if match_rate > self.config.cache_threshold {
                matched_tokens = result.matched_token_count;
                has_known_owner = !result.tenants.is_empty();
                owners = healthy_indices
                    .iter()
                    .copied()
                    .filter(|&idx| {
                        result
                            .tenants
                            .iter()
                            .any(|tenant| tenant.as_ref() == workers[idx].url())
                    })
                    .collect();
            }
        }

        let prefix_tokens = if matched_tokens > 0 {
            &tokens[..matched_tokens]
        } else {
            tokens
        };
        PrefixOwnership {
            key: PrefixBudgetKey {
                model_hash: kv_index::hash_node_path(model_id),
                prefix_hash: kv_index::hash_token_path(prefix_tokens),
                kind: PrefixKind::Token,
            },
            owners,
            has_known_owner,
        }
    }

    fn prefix_ownership(
        &self,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo,
        healthy_indices: &[usize],
        model_id: &str,
    ) -> PrefixOwnership {
        if let Some(tokens) = info.tokens {
            let guard = self.kv_monitor.read();
            if let Some(monitor) = guard.as_ref() {
                if let Some(indexer) = monitor.get_indexer(model_id) {
                    if indexer.current_size() > 0 {
                        let block_size = monitor
                            .block_size(model_id)
                            .unwrap_or(self.config.block_size);
                        let (owners, matched_blocks) = Self::score_overlap_with_depth(
                            workers,
                            tokens,
                            healthy_indices,
                            &indexer,
                            block_size,
                        );
                        if !owners.is_empty() {
                            let matched_tokens =
                                matched_blocks.saturating_mul(block_size).min(tokens.len());
                            return PrefixOwnership {
                                key: PrefixBudgetKey {
                                    model_hash: kv_index::hash_node_path(model_id),
                                    prefix_hash: kv_index::hash_token_path(
                                        &tokens[..matched_tokens],
                                    ),
                                    kind: PrefixKind::Token,
                                },
                                owners,
                                has_known_owner: true,
                            };
                        }
                    }
                }
            }
            drop(guard);

            // The event stream is authoritative once it catches up, but the
            // approximate tree supplies provisional ownership during the short
            // store-event delay after a fallback destination was selected.
            return self.token_tree_ownership(workers, tokens, healthy_indices, model_id);
        }

        let text = info.request_text.unwrap_or("");
        let mut owners = Vec::new();
        let mut matched_chars = 0usize;
        let mut has_known_owner = false;
        if let Some(tree) = self
            .string_trees
            .get(model_id)
            .map(|entry| entry.value().clone())
        {
            let result = tree.match_prefix_with_counts(text);
            let match_rate = if result.input_char_count == 0 {
                0.0
            } else {
                result.matched_char_count as f32 / result.input_char_count as f32
            };
            if match_rate > self.config.cache_threshold {
                matched_chars = result.matched_char_count;
                has_known_owner = !result.tenants.is_empty();
                owners = healthy_indices
                    .iter()
                    .copied()
                    .filter(|&idx| {
                        result
                            .tenants
                            .iter()
                            .any(|tenant| tenant.as_ref() == workers[idx].url())
                    })
                    .collect();
            }
        }
        let prefix = if matched_chars > 0 {
            text.chars().take(matched_chars).collect::<String>()
        } else {
            text.to_string()
        };
        PrefixOwnership {
            key: PrefixBudgetKey {
                model_hash: kv_index::hash_node_path(model_id),
                prefix_hash: kv_index::hash_node_path(&prefix),
                kind: PrefixKind::String,
            },
            owners,
            has_known_owner,
        }
    }

    fn owners_with_provisional(
        &self,
        ownership: &PrefixOwnership,
        workers: &[Arc<dyn Worker>],
        healthy_indices: &[usize],
    ) -> Vec<usize> {
        let mut owners = ownership.owners.clone();
        if let Some(idx) = self.recent_provisional_owner(ownership.key, workers, healthy_indices) {
            if !owners.contains(&idx) {
                owners.push(idx);
            }
        }
        owners
    }

    fn select_from_cached_owners(
        &self,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo,
        owner_indices: &[usize],
        model_id: &str,
    ) -> Option<usize> {
        if let Some(tokens) = info.tokens {
            if self.has_event_indexer(model_id) {
                self.select_worker_event_driven(workers, tokens, owner_indices, info, model_id)
            } else {
                self.select_worker_with_tokens(workers, tokens, owner_indices, info, model_id)
            }
        } else {
            self.select_worker_with_text(
                workers,
                info.request_text.unwrap_or(""),
                owner_indices,
                info,
                model_id,
            )
        }
    }

    /// Prefer the longest-prefix owners before applying fleet-wide imbalance.
    ///
    /// Existing owners are balanced by owner-local pressure and atomic reserved
    /// work. A new owner is created only when every eligible owner is above the
    /// pressure high-water mark and the configured replication ceiling has not
    /// been reached. Concurrent spill requests coalesce on one provisional
    /// destination until backend cache events catch up.
    fn select_cached_owner_or_pressure_spill(
        &self,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo,
        healthy_indices: &[usize],
        model_id: &str,
        pressure_plan: Option<&EnginePressurePlan>,
    ) -> Option<usize> {
        if self.config.max_cached_owners_per_prefix == 0 {
            return None;
        }

        let ownership = self.prefix_ownership(workers, info, healthy_indices, model_id);
        let known_owners = self.owners_with_provisional(&ownership, workers, healthy_indices);
        let owner_count = known_owners.len();
        if owner_count == 0 {
            return None;
        }

        let eligible_owners =
            SizeAwarePowerOfTwoPolicy::eligible_candidates(workers, info, &known_owners);
        if eligible_owners.is_empty() {
            return None;
        }

        let suitable_owners =
            Self::suitable_cached_owners(workers, &eligible_owners, pressure_plan);
        if !suitable_owners.is_empty() {
            Metrics::record_cache_aware_replication_decision(
                model_id,
                "cached_owner_hold",
                owner_count,
            );
            return self.select_from_cached_owners(workers, info, &suitable_owners, model_id);
        }

        let Some(plan) = pressure_plan else {
            // `suitable_cached_owners` only returns empty with a complete,
            // fresh pressure plan, but keep this fail-open guard explicit.
            return self.select_from_cached_owners(workers, info, &eligible_owners, model_id);
        };

        if owner_count < self.config.max_cached_owners_per_prefix {
            let spill_candidates: Vec<usize> = plan
                .allowed_indices
                .iter()
                .copied()
                .filter(|idx| {
                    plan.pressure_by_index[idx].pressure <= ENGINE_PRESSURE_HIGH_WATERMARK
                        && !known_owners.contains(idx)
                })
                .collect();
            let spill_candidates =
                SizeAwarePowerOfTwoPolicy::eligible_candidates(workers, info, &spill_candidates);
            if !spill_candidates.is_empty() {
                let (selected, claimed_new_owner) = self.select_or_join_replication_spill(
                    ownership.key,
                    workers,
                    info,
                    &spill_candidates,
                    model_id,
                )?;
                let result = if claimed_new_owner {
                    "owner_pressure_spill"
                } else {
                    "spill_cooldown_hold"
                };
                Metrics::record_cache_aware_replication_decision(model_id, result, owner_count);
                return Some(selected);
            }
        }

        // At the owner ceiling, or when the whole fleet is above the pressure
        // high-water mark, preserve affinity on the least-pressured owner. The
        // admission controller remains responsible for stopping new dispatch
        // when there is no safe capacity anywhere in the model pool.
        let best_pressure = eligible_owners
            .iter()
            .map(|idx| plan.pressure_by_index[idx].pressure)
            .min_by(f64::total_cmp)?;
        let least_pressured: Vec<usize> = eligible_owners
            .iter()
            .copied()
            .filter(|idx| {
                plan.pressure_by_index[idx].pressure <= best_pressure + f64::from(f32::EPSILON)
            })
            .collect();
        let result = if owner_count >= self.config.max_cached_owners_per_prefix {
            "owner_ceiling_hold"
        } else {
            "no_safe_spill_hold"
        };
        Metrics::record_cache_aware_replication_decision(model_id, result, owner_count);
        self.select_from_cached_owners(workers, info, &least_pressured, model_id)
    }

    fn recent_provisional_owner(
        &self,
        key: PrefixBudgetKey,
        workers: &[Arc<dyn Worker>],
        candidate_indices: &[usize],
    ) -> Option<usize> {
        let cooldown = Duration::from_secs(self.config.cache_owner_spill_cooldown_secs);
        if cooldown.is_zero() {
            return None;
        }
        let state = self.replication_state.get(&key)?;
        if state.last_spill.elapsed() >= cooldown {
            return None;
        }
        candidate_indices
            .iter()
            .copied()
            .find(|&idx| workers[idx].url() == state.provisional_owner)
    }

    /// Return whether this request matches an in-flight exact distribution
    /// seed. The trusted admission partition header selects the one pending
    /// record; standalone traffic falls back to the model key.
    ///
    /// Ordinary routing must never follow a pending seed. Doing so would turn
    /// one bounded distribution permit into an unbounded fan-out before the
    /// target has proved that it owns the prefix. Callers instead hold traffic
    /// on already committed owners, or fail closed when none is executable.
    fn has_pending_distribution_seed(
        &self,
        key: PrefixBudgetKey,
        info: &SelectWorkerInfo<'_>,
        model_id: &str,
    ) -> bool {
        if self.distribution_state.pending_by_partition.is_empty() {
            return false;
        }
        let partition = info
            .headers
            .and_then(|headers| headers.get(ADMISSION_PARTITION_HEADER))
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(model_id);
        self.distribution_state
            .pending_by_partition
            .get(partition)
            .is_some_and(|pending| {
                pending.prefix_key == key && pending.model_id.as_ref() == model_id
            })
    }

    /// Atomically claim the next replication slot for a prefix. The first
    /// caller after a cooldown selects a size-aware P2C destination and stores
    /// it as the provisional owner while holding the map shard. Concurrent
    /// callers then join that same destination instead of opening several new
    /// replicas before the approximate tree or backend event stream catches up.
    ///
    /// Returns `(selected_worker, claimed_spill_slot)`. The caller compares the
    /// destination with the authoritative owners to determine whether the claim
    /// actually created a new owner.
    fn select_or_join_replication_spill(
        &self,
        key: PrefixBudgetKey,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo,
        candidate_indices: &[usize],
        model_id: &str,
    ) -> Option<(usize, bool)> {
        let cooldown = Duration::from_secs(self.config.cache_owner_spill_cooldown_secs);
        if cooldown.is_zero() {
            let selected =
                self.select_worker_fallback(workers, info, candidate_indices, model_id)?;
            return Some((selected, true));
        }

        match self.replication_state.entry(key) {
            Entry::Occupied(mut entry) => {
                let active = entry.get().last_spill.elapsed() < cooldown;
                if active {
                    let provisional_owner = entry.get().provisional_owner.as_str();
                    if let Some(idx) = candidate_indices
                        .iter()
                        .copied()
                        .find(|&idx| workers[idx].url() == provisional_owner)
                    {
                        drop(entry);
                        let selected = self.fallback.select_least_loaded_from_candidates(
                            workers,
                            info,
                            &[idx],
                        )?;
                        return Some((selected, false));
                    }
                }

                // An expired claim, or an active provisional owner excluded by
                // pressure, may be replaced by a new suitable P2C destination.
                let selected =
                    self.select_worker_fallback(workers, info, candidate_indices, model_id)?;
                entry.insert(PrefixReplicationState {
                    last_spill: Instant::now(),
                    provisional_owner: workers[selected].url().to_string(),
                });
                Some((selected, true))
            }
            Entry::Vacant(entry) => {
                let selected =
                    self.select_worker_fallback(workers, info, candidate_indices, model_id)?;
                entry.insert(PrefixReplicationState {
                    last_spill: Instant::now(),
                    provisional_owner: workers[selected].url().to_string(),
                });
                Some((selected, true))
            }
        }
    }

    fn select_worker_imbalanced_with_budget(
        &self,
        workers: &[Arc<dyn Worker>],
        info: &SelectWorkerInfo,
        healthy_indices: &[usize],
        candidate_indices: &[usize],
        model_id: &str,
        _reason: ImbalanceReason,
    ) -> Option<usize> {
        if self.config.max_cached_owners_per_prefix == 0 {
            return self.select_worker_fallback(workers, info, candidate_indices, model_id);
        }

        let eligible_indices =
            SizeAwarePowerOfTwoPolicy::eligible_candidates(workers, info, candidate_indices);
        if eligible_indices.is_empty() {
            return None;
        }
        let ownership = self.prefix_ownership(workers, info, healthy_indices, model_id);
        let mut owner_candidates: Vec<usize> = ownership
            .owners
            .iter()
            .copied()
            .filter(|idx| eligible_indices.contains(idx))
            .collect();
        let provisional_owner =
            self.recent_provisional_owner(ownership.key, workers, healthy_indices);
        if let Some(idx) = provisional_owner.filter(|idx| eligible_indices.contains(idx)) {
            if !owner_candidates.contains(&idx) {
                owner_candidates.push(idx);
            }
        }

        let mut healthy_owners = ownership.owners.clone();
        if let Some(idx) = provisional_owner {
            if !healthy_owners.contains(&idx) {
                healthy_owners.push(idx);
            }
        }
        let owner_count = healthy_owners.len();
        if !owner_candidates.is_empty() {
            Metrics::record_cache_aware_replication_decision(
                model_id,
                "cached_owner_hold",
                owner_count,
            );
            return self.fallback.select_least_loaded_from_candidates(
                workers,
                info,
                &owner_candidates,
            );
        }
        let (selected, claimed_new_owner) = self.select_or_join_replication_spill(
            ownership.key,
            workers,
            info,
            &eligible_indices,
            model_id,
        )?;
        let creates_owner = !ownership.owners.contains(&selected) && claimed_new_owner;
        let result = if !claimed_new_owner {
            "spill_cooldown_hold"
        } else if creates_owner {
            "no_suitable_owner_spill"
        } else {
            "fallback_existing_owner"
        };
        Metrics::record_cache_aware_replication_decision(model_id, result, owner_count);
        Some(selected)
    }

    /// Event-driven routing: PositionalIndexer overlap scoring (Type 1).
    ///
    /// When exact overlap is found, selects the worker with the best cache
    /// match. When the event index has no overlap, consults the approximate
    /// token tree before size-aware P2C. This repairs affinity after a late
    /// event subscription: SGLang's live-only stream can begin with a block
    /// whose parent predates the subscription, while routed traffic still
    /// gives us bounded provisional ownership that later exact events replace.
    fn select_worker_event_driven(
        &self,
        workers: &[Arc<dyn Worker>],
        tokens: &[u32],
        healthy_indices: &[usize],
        info: &SelectWorkerInfo,
        model_id: &str,
    ) -> Option<usize> {
        let guard = self.kv_monitor.read();
        let monitor = guard.as_ref()?;
        let indexer = monitor.get_indexer(model_id)?;

        // Per-model block_size: learned from events > config default
        let block_size = monitor
            .block_size(model_id)
            .unwrap_or(self.config.block_size);

        let owners = Self::score_overlap(workers, tokens, healthy_indices, &indexer, block_size);
        if !owners.is_empty() {
            let idx = self
                .fallback
                .select_least_loaded_from_candidates(workers, info, &owners)?;
            debug!(
                worker = workers[idx].url(),
                owner_count = owners.len(),
                model_id,
                "Event-driven routing: overlap match"
            );
            return Some(idx);
        }

        // No exact overlap: recover from the provisional token tree populated
        // by earlier routed requests. A miss there uses size-aware P2C and
        // records that destination as a provisional owner.
        let idx =
            self.select_worker_with_tokens(workers, tokens, healthy_indices, info, model_id)?;
        debug!(
            worker = workers[idx].url(),
            model_id, "Event-driven routing: no exact overlap, provisional-tree fallback"
        );
        Some(idx)
    }

    /// Return every healthy worker tied for the highest PositionalIndexer overlap.
    ///
    /// Returns an empty vector if the request is too short for a full block or
    /// no workers have matching data.
    fn score_overlap(
        workers: &[Arc<dyn Worker>],
        tokens: &[u32],
        healthy_indices: &[usize],
        indexer: &PositionalIndexer,
        block_size: usize,
    ) -> Vec<usize> {
        Self::score_overlap_with_depth(workers, tokens, healthy_indices, indexer, block_size).0
    }

    /// Return the best owners together with the number of matching full blocks.
    /// The depth lets the replication budget identify the exact shared prefix
    /// without storing request text or token vectors in its cooldown map.
    fn score_overlap_with_depth(
        workers: &[Arc<dyn Worker>],
        tokens: &[u32],
        healthy_indices: &[usize],
        indexer: &PositionalIndexer,
        block_size: usize,
    ) -> (Vec<usize>, usize) {
        let content_hashes = compute_request_content_hashes(tokens, block_size);
        if content_hashes.is_empty() {
            return (Vec::new(), 0);
        }

        let overlap = indexer.find_matches(&content_hashes, false);
        if overlap.scores.is_empty() {
            return (Vec::new(), 0);
        }

        let best_score = healthy_indices
            .iter()
            .copied()
            .filter_map(|idx| {
                indexer
                    .worker_id(workers[idx].url())
                    .and_then(|id| overlap.scores.get(&id))
                    .copied()
                    .filter(|score| *score > 0)
            })
            .max()
            .unwrap_or(0);
        if best_score == 0 {
            return (Vec::new(), 0);
        }

        let owners = healthy_indices
            .iter()
            .copied()
            .filter(|&idx| {
                indexer
                    .worker_id(workers[idx].url())
                    .and_then(|id| overlap.scores.get(&id))
                    .copied()
                    == Some(best_score)
            })
            .collect();
        (owners, best_score as usize)
    }

    /// Select worker using token-based tree (gRPC path)
    fn select_worker_with_tokens(
        &self,
        workers: &[Arc<dyn Worker>],
        tokens: &[u32],
        healthy_indices: &[usize],
        info: &SelectWorkerInfo,
        model_id: &str,
    ) -> Option<usize> {
        let tree = self
            .token_trees
            .get(model_id)
            .map(|entry| entry.value().clone());

        if let Some(tree) = tree {
            // Single tree descent: match, pick the worker from the match
            // result, then insert for it — replacing the former
            // match_prefix_with_counts + insert_tokens pair (two full descents
            // over the same prefix). The selection closure runs once, after the
            // match:
            //   * cache hit: choose the least-loaded healthy owner and insert for it;
            //   * cache miss: use size-aware P2C and insert for the destination;
            //   * no suitable owner: defer insertion until the fallback selects.
            let mut selected_idx: Option<usize> = None;
            let result = tree.match_and_insert_with(tokens, |result| {
                let match_rate = if result.input_token_count == 0 {
                    0.0
                } else {
                    result.matched_token_count as f32 / result.input_token_count as f32
                };

                selected_idx = if match_rate > self.config.cache_threshold {
                    let owner_indices: Vec<_> = healthy_indices
                        .iter()
                        .copied()
                        .filter(|&idx| {
                            result
                                .tenants
                                .iter()
                                .any(|tenant| tenant.as_ref() == workers[idx].url())
                        })
                        .collect();
                    self.fallback
                        .select_least_loaded_from_candidates(workers, info, &owner_indices)
                } else {
                    self.fallback
                        .select_worker_from_candidates(workers, info, healthy_indices)
                };

                // Insert for the selected worker (None => no insert, exactly
                // like the old `if let Some(idx)` guard around insert_tokens).
                selected_idx.map(|idx| workers[idx].url())
            });

            if let Some(idx) = selected_idx {
                // Record hash(full_tokens)→matched_prefix tokens.
                // The hash key matches what sync_tree_operation
                // sends on the wire (hash of full sequence). The
                // VALUE is only the matched prefix — not the full
                // sequence (32K tokens × 4 bytes = 128 KB worst
                // case). v1 never populated a token hash index;
                // v2's `TreeHandle` impl consults this map per
                // incoming token delta, so maintain it alongside
                // the tree. Mirrors the string side at the
                // analogous block; reuses the match `result`
                // returned by match_and_insert_with.
                if self.should_populate_hash_index() {
                    let matched_prefix: Vec<u32> = tokens[..result.matched_token_count].to_vec();
                    self.hash_index
                        .entry(model_id.to_string())
                        .or_default()
                        .token_tree
                        .insert(kv_index::hash_token_path(tokens), matched_prefix);
                }
                return Some(idx);
            }

            self.select_worker_fallback(workers, info, healthy_indices, model_id)
        } else {
            debug!(
                "Warning: No token tree found for model '{}', using size-aware fallback",
                model_id
            );
            self.select_worker_fallback(workers, info, healthy_indices, model_id)
        }
    }

    /// Select worker using string-based tree (HTTP path)
    fn select_worker_with_text(
        &self,
        workers: &[Arc<dyn Worker>],
        text: &str,
        healthy_indices: &[usize],
        info: &SelectWorkerInfo,
        model_id: &str,
    ) -> Option<usize> {
        let tree = self
            .string_trees
            .get(model_id)
            .map(|entry| entry.value().clone());

        if let Some(tree) = tree {
            // Single tree descent: match, pick the worker from the match result,
            // then insert for it — replacing the former match_prefix_with_counts
            // + insert_text pair. See the token path for the branch rationale.
            let mut selected_idx: Option<usize> = None;
            let result = tree.match_and_insert_with(text, |result| {
                let match_rate = if result.input_char_count == 0 {
                    0.0
                } else {
                    result.matched_char_count as f32 / result.input_char_count as f32
                };

                selected_idx = if match_rate > self.config.cache_threshold {
                    let owner_indices: Vec<_> = healthy_indices
                        .iter()
                        .copied()
                        .filter(|&idx| {
                            result
                                .tenants
                                .iter()
                                .any(|tenant| tenant.as_ref() == workers[idx].url())
                        })
                        .collect();
                    self.fallback
                        .select_least_loaded_from_candidates(workers, info, &owner_indices)
                } else {
                    self.fallback
                        .select_worker_from_candidates(workers, info, healthy_indices)
                };

                // Insert for the selected worker (None => no insert, exactly
                // like the old `if let Some(idx)` guard around insert_text).
                selected_idx.map(|idx| workers[idx].url())
            });

            if let Some(idx) = selected_idx {
                // Record hash(full_text)→matched_prefix for mesh tenant delta
                // resolution. The hash key matches what sync_tree_operation sends
                // on the wire (hash of full text). The VALUE is only the matched
                // prefix (~50-200 chars), not the full prompt (20KB+). When a
                // remote delta arrives, we look up the hash and call
                // insert_text(matched_prefix, worker) which routes to the same
                // tree node. This keeps the index memory-bounded.
                if self.should_populate_hash_index() {
                    let matched_prefix: String =
                        text.chars().take(result.matched_char_count).collect();
                    let path_hash = kv_index::hash_node_path(text);
                    self.hash_index
                        .entry(model_id.to_string())
                        .or_default()
                        .string_tree
                        .insert(path_hash, matched_prefix);
                }

                return Some(idx);
            }

            self.select_worker_fallback(workers, info, healthy_indices, model_id)
        } else {
            debug!(
                "Warning: No string tree found for model '{}', using size-aware fallback",
                model_id
            );
            self.select_worker_fallback(workers, info, healthy_indices, model_id)
        }
    }
}

impl Default for CacheAwarePolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use kv_index::{compute_content_hash, SequenceHash, StoredBlock, WorkerBlockMap};
    use openai_protocol::worker::{HealthCheckConfig, SchedulerLoadSnapshot, WorkerStatus};

    use super::*;
    use crate::worker::{BasicWorkerBuilder, WorkerType};

    fn no_health_check() -> HealthCheckConfig {
        HealthCheckConfig {
            disable_health_check: true,
            ..Default::default()
        }
    }

    fn engine_load(token_usage: f64, utilization: f64, waiting: i32) -> WorkerLoadResponse {
        WorkerLoadResponse {
            loads: vec![SchedulerLoadSnapshot {
                token_usage,
                utilization,
                num_waiting_reqs: waiting,
                ..Default::default()
            }],
            dp_rank_count: 1,
            ..Default::default()
        }
    }

    fn engine_load_with_scheduler(
        token_usage: f64,
        utilization: f64,
        running: i32,
        waiting: i32,
        max_running: i32,
    ) -> WorkerLoadResponse {
        WorkerLoadResponse {
            loads: vec![SchedulerLoadSnapshot {
                token_usage,
                utilization,
                num_running_reqs: running,
                num_waiting_reqs: waiting,
                max_running_requests: max_running,
                ..Default::default()
            }],
            dp_rank_count: 1,
            ..Default::default()
        }
    }

    fn engine_load_with_scheduler_ranks(ranks: &[(i32, i32, i32)]) -> WorkerLoadResponse {
        WorkerLoadResponse {
            loads: ranks
                .iter()
                .enumerate()
                .map(
                    |(dp_rank, &(running, waiting, max_running))| SchedulerLoadSnapshot {
                        dp_rank: i32::try_from(dp_rank).unwrap(),
                        token_usage: 0.10,
                        utilization: 0.10,
                        num_running_reqs: running,
                        num_waiting_reqs: waiting,
                        max_running_requests: max_running,
                        ..Default::default()
                    },
                )
                .collect(),
            dp_rank_count: i32::try_from(ranks.len()).unwrap(),
            ..Default::default()
        }
    }

    fn engine_load_with_standard_scheduler_missing_cap(
        token_usage: f64,
        utilization: f64,
        running: i32,
        waiting: i32,
    ) -> WorkerLoadResponse {
        WorkerLoadResponse {
            loads: vec![SchedulerLoadSnapshot {
                token_usage,
                utilization,
                num_running_reqs: running,
                num_waiting_reqs: waiting,
                num_total_reqs: running.saturating_add(waiting),
                max_running_requests: 0,
                ..Default::default()
            }],
            dp_rank_count: 1,
            ..Default::default()
        }
    }

    fn two_workers() -> Vec<Arc<dyn Worker>> {
        vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ]
    }

    fn two_workers_with_max_running(max_running: u16) -> Vec<Arc<dyn Worker>> {
        ["http://w1:8000", "http://w2:8000"]
            .into_iter()
            .map(|url| {
                Arc::new(
                    BasicWorkerBuilder::new(url)
                        .worker_type(WorkerType::Regular)
                        .labels(HashMap::from([(
                            "max_running_requests".to_string(),
                            max_running.to_string(),
                        )]))
                        .health_config(no_health_check())
                        .build(),
                ) as Arc<dyn Worker>
            })
            .collect()
    }

    fn engine_aware_policy() -> CacheAwarePolicy {
        CacheAwarePolicy::with_config(CacheAwareConfig {
            engine_load: true,
            eviction_interval_secs: 0,
            ..Default::default()
        })
    }

    fn prime_worker_one_affinity(
        policy: &CacheAwarePolicy,
        workers: &[Arc<dyn Worker>],
        text: &str,
    ) {
        policy.init_workers(workers);
        let model_id = normalize_model_key(workers[0].model_id());
        policy
            .string_trees
            .get(model_id)
            .unwrap()
            .insert_text(text, workers[0].url());
    }

    fn scheduler_pressure_policy() -> CacheAwarePolicy {
        CacheAwarePolicy::with_config(CacheAwareConfig {
            engine_load: true,
            eviction_interval_secs: 0,
            max_cached_owners_per_prefix: 8,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        })
    }

    fn select_text(policy: &CacheAwarePolicy, workers: &[Arc<dyn Worker>], text: &str) -> usize {
        policy
            .select_worker(
                workers,
                &SelectWorkerInfo {
                    request_text: Some(text),
                    ..Default::default()
                },
            )
            .unwrap()
    }

    fn partition_headers(partition: &str) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            ADMISSION_PARTITION_HEADER,
            partition.parse().expect("valid partition header"),
        );
        headers
    }

    fn owner_pressure_loads(policy: &CacheAwarePolicy) {
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 34, 0, 37),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));
    }

    fn exact_target(worker: &Arc<dyn Worker>) -> Vec<(String, u64, u64)> {
        vec![(
            worker.url().to_string(),
            worker.generation_id(),
            worker.revision(),
        )]
    }

    #[test]
    fn distribution_lease_is_default_off() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            eviction_interval_secs: 0,
            ..Default::default()
        });
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                workers[0].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 34, 0, 37),
            ),
            (
                workers[1].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));
        let headers = partition_headers("kimi-k3");
        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            headers: Some(&headers),
            ..Default::default()
        };
        let model_id = normalize_model_key(workers[0].model_id());

        assert!(policy
            .begin_distribution_lease(
                model_id,
                &workers,
                &info,
                "kimi-k3",
                &exact_target(&workers[1]),
            )
            .is_none());
        assert!(policy.distribution_state.pending_by_partition.is_empty());
    }

    #[test]
    fn distribution_seed_is_isolated_until_success_is_committed() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        owner_pressure_loads(&policy);
        for _ in 0..5 {
            workers[0].record_outcome(503);
        }
        assert!(workers[0].is_healthy());
        assert!(!workers[0].is_available());
        policy.update_loads(&HashMap::from([
            (
                workers[0].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 1, 0, 37),
            ),
            (
                workers[1].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));
        let headers = partition_headers("kimi-k3");
        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            headers: Some(&headers),
            forbid_unleased_cache_owner_expansion: true,
            ..Default::default()
        };
        let model_id = normalize_model_key(workers[0].model_id());
        let allowed = exact_target(&workers[1]);

        assert!(policy
            .begin_distribution_lease(
                model_id,
                &workers,
                &info,
                "kimi-k3",
                &[(
                    workers[1].url().to_string(),
                    workers[1].generation_id(),
                    workers[1].revision() + 1,
                )],
            )
            .is_none());

        let (target_idx, mut lease) = policy
            .begin_distribution_lease(model_id, &workers, &info, "kimi-k3", &allowed)
            .expect("idle exact target should receive one seed");
        assert_eq!(target_idx, 1);
        assert_eq!(lease.target_url(), workers[1].url());
        assert_eq!(lease.target_generation_id(), workers[1].generation_id());
        assert_eq!(lease.target_revision(), workers[1].revision());
        assert!(lease.creates_owner);
        assert_eq!(
            policy.distribution_state.pending_by_partition.len(),
            1,
            "the seed must occupy the partition before dispatch"
        );

        let before = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("shared prefix");
        assert_eq!(before.tenants.len(), 1);
        assert!(before
            .tenants
            .iter()
            .any(|tenant| tenant.as_ref() == workers[0].url()));
        assert!(policy.replication_state.is_empty());

        assert!(policy
            .begin_distribution_lease(model_id, &workers, &info, "kimi-k3", &allowed)
            .is_none());
        for _ in 0..8 {
            assert_eq!(
                policy.select_worker(&workers, &info),
                None,
                "ordinary traffic must not fan out to an uncommitted seed target"
            );
            assert_eq!(
                policy.distribution_state.pending_by_partition.len(),
                1,
                "ordinary routing must not consume the exact seed lease"
            );
            assert!(
                policy.replication_state.is_empty(),
                "ordinary routing must not create a provisional spill around the seed permit"
            );
        }
        let while_pending = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("shared prefix");
        assert_eq!(
            while_pending.tenants.len(),
            1,
            "pending routing must not publish tree ownership"
        );

        assert!(policy.validate_distribution_lease(model_id, &workers, &info, &workers[1], &lease,));
        assert!(policy.commit_distribution_success(
            model_id,
            &workers,
            "shared prefix",
            &workers[1],
            &mut lease,
        ));
        assert!(policy.distribution_state.pending_by_partition.is_empty());
        assert_eq!(policy.replication_state.len(), 1);
        let committed = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("shared prefix");
        assert_eq!(committed.tenants.len(), 2);
        assert!(committed
            .tenants
            .iter()
            .any(|tenant| tenant.as_ref() == workers[1].url()));
        assert_eq!(
            policy.select_worker(&workers, &info),
            Some(1),
            "the exact target becomes routeable only after successful ownership commit"
        );
    }

    #[test]
    fn distribution_existing_owner_uses_pending_slot_without_new_cooldown() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                workers[0].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 1, 0, 37),
            ),
            (
                workers[1].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));
        let headers = partition_headers("kimi-k3");
        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            headers: Some(&headers),
            ..Default::default()
        };
        let model_id = normalize_model_key(workers[0].model_id());
        let allowed = exact_target(&workers[0]);

        let (target_idx, mut lease) = policy
            .begin_distribution_lease(model_id, &workers, &info, "kimi-k3", &allowed)
            .expect("an exact suitable owner may consume the one-shot route");
        assert_eq!(target_idx, 0);
        assert!(!lease.creates_owner);
        assert!(policy
            .begin_distribution_lease(model_id, &workers, &info, "kimi-k3", &allowed)
            .is_none());
        assert_eq!(policy.select_worker(&workers, &info), Some(0));
        assert!(policy.validate_distribution_lease(model_id, &workers, &info, &workers[0], &lease,));
        assert!(policy.commit_distribution_success(
            model_id,
            &workers,
            "shared prefix",
            &workers[0],
            &mut lease,
        ));
        assert!(policy.replication_state.is_empty());
    }

    #[test]
    fn distribution_lease_fails_closed_on_target_or_policy_change() {
        let policy = scheduler_pressure_policy();
        let other_policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        owner_pressure_loads(&policy);
        let headers = partition_headers("kimi-k3");
        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            headers: Some(&headers),
            ..Default::default()
        };
        let changed_info = SelectWorkerInfo {
            request_text: Some("different request"),
            headers: Some(&headers),
            ..Default::default()
        };
        let model_id = normalize_model_key(workers[0].model_id());
        let (_, lease) = policy
            .begin_distribution_lease(
                model_id,
                &workers,
                &info,
                "kimi-k3",
                &exact_target(&workers[1]),
            )
            .unwrap();

        assert!(!policy.validate_distribution_lease(
            model_id,
            &workers,
            &changed_info,
            &workers[1],
            &lease,
        ));
        assert!(!other_policy.validate_distribution_lease(
            model_id,
            &workers,
            &info,
            &workers[1],
            &lease,
        ));
        workers[1].set_status(WorkerStatus::NotReady);
        assert!(!policy.validate_distribution_lease(
            model_id,
            &workers,
            &info,
            &workers[1],
            &lease,
        ));
        drop(lease);
        assert!(policy.distribution_state.pending_by_partition.is_empty());
    }

    #[test]
    fn stale_distribution_lease_cannot_remove_newer_partition_state() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        owner_pressure_loads(&policy);
        let headers = partition_headers("kimi-k3");
        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            headers: Some(&headers),
            ..Default::default()
        };
        let model_id = normalize_model_key(workers[0].model_id());
        let allowed = exact_target(&workers[1]);
        let (_, stale) = policy
            .begin_distribution_lease(model_id, &workers, &info, "kimi-k3", &allowed)
            .unwrap();
        assert!(policy
            .distribution_state
            .release_if_current("kimi-k3", stale.lease_id));

        let (_, current) = policy
            .begin_distribution_lease(model_id, &workers, &info, "kimi-k3", &allowed)
            .unwrap();
        let current_id = current.lease_id;
        assert_ne!(stale.lease_id, current_id);
        drop(stale);
        assert_eq!(
            policy
                .distribution_state
                .pending_by_partition
                .get("kimi-k3")
                .unwrap()
                .lease_id,
            current_id
        );
        drop(current);
        assert!(policy.distribution_state.pending_by_partition.is_empty());
    }

    #[test]
    fn distribution_lease_rejects_same_url_new_worker_generation() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        owner_pressure_loads(&policy);
        let headers = partition_headers("kimi-k3");
        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            headers: Some(&headers),
            ..Default::default()
        };
        let model_id = normalize_model_key(workers[0].model_id());
        let (_, lease) = policy
            .begin_distribution_lease(
                model_id,
                &workers,
                &info,
                "kimi-k3",
                &exact_target(&workers[1]),
            )
            .unwrap();

        let replacement: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new(workers[1].url())
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        );
        assert_eq!(replacement.revision(), lease.target_revision());
        assert_ne!(replacement.generation_id(), lease.target_generation_id());
        let replaced_workers = vec![Arc::clone(&workers[0]), Arc::clone(&replacement)];
        assert!(!policy.validate_distribution_lease(
            model_id,
            &replaced_workers,
            &info,
            &replacement,
            &lease,
        ));
    }

    #[test]
    fn test_engine_load_keeps_cache_affinity_when_pressure_is_close() {
        let policy = engine_aware_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");

        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.50, 0.40, 0)),
            ("http://w2:8000".to_string(), engine_load(0.45, 0.40, 0)),
        ]));

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("shared prefix"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(selected, 0);
    }

    #[test]
    fn test_engine_load_overrides_affinity_for_hot_worker() {
        let policy = engine_aware_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");

        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.85, 0.70, 0)),
            ("http://w2:8000".to_string(), engine_load(0.40, 0.50, 0)),
        ]));

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("shared prefix"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(selected, 1);
    }

    #[test]
    fn scheduler_pressure_keeps_affinity_below_watermark() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 32, 1, 37),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));

        let selected = select_text(&policy, &workers, "shared prefix");

        assert_eq!(selected, 0);
        assert!(policy.replication_state.is_empty());
    }

    #[test]
    fn scheduler_pressure_spills_above_watermark() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 33, 1, 37),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));

        let selected = select_text(&policy, &workers, "shared prefix");

        assert_eq!(selected, 1);
        assert_eq!(policy.replication_state.len(), 1);
    }

    #[test]
    fn scheduler_pressure_sums_complete_rank_caps() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler_ranks(&[(17, 0, 37), (17, 0, 37)]),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler_ranks(&[(0, 0, 37), (0, 0, 37)]),
            ),
        ]));

        let selected = select_text(&policy, &workers, "shared prefix");

        assert_eq!(selected, 0);
        assert!(policy.replication_state.is_empty());
    }

    #[test]
    fn scheduler_pressure_uses_registered_cap_for_single_standard_rank() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers_with_max_running(37);
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_standard_scheduler_missing_cap(0.10, 0.10, 33, 1),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_standard_scheduler_missing_cap(0.10, 0.10, 0, 0),
            ),
        ]));

        let selected = select_text(&policy, &workers, "shared prefix");

        assert_eq!(selected, 1);
        assert_eq!(policy.replication_state.len(), 1);
    }

    #[test]
    fn distribution_seed_uses_registered_cap_for_single_standard_rank() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers_with_max_running(38);
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_standard_scheduler_missing_cap(0.10, 0.10, 38, 1),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_standard_scheduler_missing_cap(0.10, 0.10, 0, 0),
            ),
        ]));
        let headers = partition_headers("kimi-k3");
        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            headers: Some(&headers),
            ..Default::default()
        };
        let model_id = normalize_model_key(workers[0].model_id());

        let (target_idx, _lease) = policy
            .begin_distribution_lease(
                model_id,
                &workers,
                &info,
                "kimi-k3",
                &exact_target(&workers[1]),
            )
            .expect("the clean peer must be targetable from canonical single-rank telemetry");

        assert_eq!(target_idx, 1);
    }

    #[test]
    fn scheduler_pressure_ignores_metadata_cap_when_rank_caps_are_missing() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers_with_max_running(37);
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler_ranks(&[(17, 0, 0), (17, 0, 0)]),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler_ranks(&[(0, 0, 0), (0, 0, 0)]),
            ),
        ]));

        let selected = select_text(&policy, &workers, "shared prefix");

        assert_eq!(selected, 0);
        assert!(policy.replication_state.is_empty());
    }

    #[test]
    fn scheduler_pressure_ignores_partial_rank_caps() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler_ranks(&[(17, 0, 37), (17, 0, 0)]),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler_ranks(&[(0, 0, 37), (0, 0, 37)]),
            ),
        ]));

        let selected = select_text(&policy, &workers, "shared prefix");

        assert_eq!(selected, 0);
        assert!(policy.replication_state.is_empty());
    }

    #[test]
    fn scheduler_pressure_ignores_tp_duplicated_counts_without_snapshot_cap() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers_with_max_running(37);
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 176, 32, 0),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 0),
            ),
        ]));

        let selected = select_text(&policy, &workers, "shared prefix");

        assert_eq!(selected, 0);
        assert!(policy.replication_state.is_empty());
    }

    #[test]
    fn scheduler_pressure_uses_an_existing_suitable_owner_before_spilling() {
        let policy = scheduler_pressure_policy();
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        policy.init_workers(&workers);
        let model_id = normalize_model_key(workers[0].model_id());
        let tree = policy.string_trees.get(model_id).unwrap().value().clone();
        tree.insert_text("hot prefix", workers[0].url());
        tree.insert_text("hot prefix", workers[1].url());
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler(0.20, 0.20, 37, 116, 37),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler(0.20, 0.20, 20, 0, 37),
            ),
            (
                "http://w3:8000".to_string(),
                engine_load_with_scheduler(0.20, 0.20, 0, 0, 37),
            ),
        ]));

        let selected = select_text(&policy, &workers, "hot prefix");

        assert_eq!(selected, 1);
        assert!(policy.replication_state.is_empty());
        assert_eq!(tree.match_prefix_with_counts("hot prefix").tenants.len(), 2);
    }

    #[test]
    fn scheduler_pressure_skips_a_saturated_spill_candidate() {
        let policy = scheduler_pressure_policy();
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        prime_worker_one_affinity(&policy, &workers, "hot prefix");
        policy.update_loads(&HashMap::from([
            (
                "http://w1:8000".to_string(),
                engine_load_with_scheduler(0.20, 0.20, 37, 116, 37),
            ),
            (
                "http://w2:8000".to_string(),
                engine_load_with_scheduler(0.20, 0.20, 37, 0, 37),
            ),
            (
                "http://w3:8000".to_string(),
                engine_load_with_scheduler(0.20, 0.20, 0, 0, 37),
            ),
        ]));

        let selected = select_text(&policy, &workers, "hot prefix");

        assert_eq!(selected, 2);
    }

    #[test]
    fn test_engine_load_falls_back_when_telemetry_is_partial() {
        let policy = engine_aware_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.95, 0.95, 10)),
            ("http://w2:8000".to_string(), engine_load(0.10, 0.10, 0)),
        ]));
        let partial = HashMap::from([("http://w1:8000".to_string(), engine_load(0.95, 0.95, 10))]);
        policy.update_loads_for_workers(
            &partial,
            &["http://w1:8000".to_string(), "http://w2:8000".to_string()],
        );

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("shared prefix"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(selected, 0);
    }

    #[test]
    fn test_engine_load_falls_back_when_telemetry_is_stale() {
        let policy = engine_aware_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "shared prefix");
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.95, 0.95, 10)),
            ("http://w2:8000".to_string(), engine_load(0.10, 0.10, 0)),
        ]));
        for load in policy.engine_loads.write().values_mut() {
            load.observed_at = Instant::now() - ENGINE_LOAD_MAX_AGE - Duration::from_secs(1);
        }

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("shared prefix"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(selected, 0);
    }

    #[test]
    fn test_cache_aware_with_balanced_load() {
        // Create policy without eviction thread for testing
        let config = CacheAwareConfig {
            eviction_interval_secs: 0, // Disable eviction thread
            ..Default::default()
        };
        let policy = CacheAwarePolicy::with_config(config);
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .api_key("test_api_key")
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .api_key("test_api_key")
                    .health_config(no_health_check())
                    .build(),
            ),
        ];

        // Initialize the policy with workers
        policy.init_workers(&workers);

        // First request should be distributed
        let idx1 = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hello world"),
                    ..Default::default()
                },
            )
            .unwrap();

        // Same request should go to same worker (cache hit)
        let idx2 = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hello world"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx1, idx2);

        // Similar request should also go to same worker
        let idx3 = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hello"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx1, idx3);
    }

    #[test]
    fn test_http_prefix_balances_across_all_cached_owners() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            eviction_interval_secs: 0,
            ..Default::default()
        });
        let workers = two_workers();
        policy.init_workers(&workers);

        let model_id = normalize_model_key(workers[0].model_id());
        let tree = policy.string_trees.get(model_id).unwrap().value().clone();
        tree.insert_text("shared prefix", workers[0].url());
        tree.insert_text("shared prefix", workers[1].url());

        let info = SelectWorkerInfo {
            request_text: Some("shared prefix"),
            max_output_tokens: Some(512),
            reserve_work: true,
            ..Default::default()
        };
        let first = policy.select_worker(&workers, &info).unwrap();
        let second = policy.select_worker(&workers, &info).unwrap();

        assert_ne!(
            first, second,
            "atomic reservations should spread a hot prefix across its owners"
        );
        let matched = tree.match_prefix_with_counts("shared prefix");
        assert_eq!(
            matched
                .tenants
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
            vec![workers[0].url(), workers[1].url()]
        );
    }

    #[test]
    fn test_http_fallback_adds_owner_and_worker_removal_prunes_it() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            eviction_interval_secs: 0,
            ..Default::default()
        });
        let workers = two_workers();
        policy.init_workers(&workers);

        let model_id = normalize_model_key(workers[0].model_id());
        let tree = policy.string_trees.get(model_id).unwrap().value().clone();
        tree.insert_text("hot prefix", workers[0].url());

        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-smg-excluded-worker-urls",
            workers[0].url().parse().unwrap(),
        );
        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hot prefix"),
                    headers: Some(&headers),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(selected, 1);

        let matched = tree.match_prefix_with_counts("hot prefix");
        assert_eq!(matched.tenants.len(), 2, "fallback must add another owner");

        policy.remove_worker_from_model(model_id, workers[0].url());
        let matched = tree.match_prefix_with_counts("hot prefix");
        assert_eq!(
            matched
                .tenants
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
            vec![workers[1].url()]
        );
    }

    fn replication_budget_policy(max_owners: usize, cooldown_secs: u64) -> CacheAwarePolicy {
        CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            balance_abs_threshold: 0,
            balance_rel_threshold: 1.0,
            eviction_interval_secs: 0,
            max_cached_owners_per_prefix: max_owners,
            cache_owner_spill_cooldown_secs: cooldown_secs,
            ..Default::default()
        })
    }

    #[test]
    fn replication_budget_holds_on_least_loaded_owner_at_target() {
        let policy = replication_budget_policy(1, 5);
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "hot prefix");
        for _ in 0..20 {
            workers[0].increment_load();
        }

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hot prefix"),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(selected, 0, "the soft owner target must stop replication");
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("hot prefix");
        assert_eq!(matched.tenants.len(), 1);
    }

    #[test]
    fn replication_budget_never_prunes_authoritative_owners() {
        let policy = replication_budget_policy(1, 5);
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        policy.init_workers(&workers);
        let model_id = normalize_model_key(workers[0].model_id());
        let tree = policy.string_trees.get(model_id).unwrap().value().clone();
        for worker in &workers {
            tree.insert_text("hot prefix", worker.url());
        }
        for _ in 0..20 {
            workers[0].increment_load();
        }
        for _ in 0..10 {
            workers[1].increment_load();
        }

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hot prefix"),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(selected, 2, "all known owners remain routing candidates");
        let matched = tree.match_prefix_with_counts("hot prefix");
        assert_eq!(
            matched.tenants.len(),
            3,
            "the target is not a hard catalog cap"
        );
    }

    #[test]
    fn owner_pressure_spill_allows_only_one_new_owner() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            engine_load: true,
            eviction_interval_secs: 0,
            max_cached_owners_per_prefix: 8,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        });
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        prime_worker_one_affinity(&policy, &workers, "hot prefix");
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.95, 0.95, 4)),
            ("http://w2:8000".to_string(), engine_load(0.20, 0.20, 0)),
            ("http://w3:8000".to_string(), engine_load(0.20, 0.20, 0)),
        ]));
        let info = SelectWorkerInfo {
            request_text: Some("hot prefix"),
            ..Default::default()
        };

        let first = policy.select_worker(&workers, &info).unwrap();
        let second = policy.select_worker(&workers, &info).unwrap();

        assert_ne!(first, 0, "the first imbalance may create one owner");
        assert_eq!(second, first, "the next request must join that spill");
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("hot prefix");
        assert_eq!(matched.tenants.len(), 2);
    }

    #[test]
    fn distribution_guard_keeps_ordinary_pressure_on_committed_owner() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            engine_load: true,
            eviction_interval_secs: 0,
            max_cached_owners_per_prefix: 8,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        });
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        prime_worker_one_affinity(&policy, &workers, "hot prefix");
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.95, 0.95, 4)),
            ("http://w2:8000".to_string(), engine_load(0.20, 0.20, 0)),
            ("http://w3:8000".to_string(), engine_load(0.20, 0.20, 0)),
        ]));
        let info = SelectWorkerInfo {
            request_text: Some("hot prefix"),
            forbid_unleased_cache_owner_expansion: true,
            ..Default::default()
        };

        for _ in 0..8 {
            assert_eq!(
                policy.select_worker(&workers, &info),
                Some(0),
                "ordinary traffic must remain on a committed owner"
            );
        }
        assert!(policy.replication_state.is_empty());
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("hot prefix");
        assert_eq!(matched.tenants.len(), 1);
    }

    #[test]
    fn distribution_guard_fails_closed_when_recorded_owner_cannot_execute() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "hot prefix");
        owner_pressure_loads(&policy);
        for _ in 0..5 {
            workers[0].record_outcome(503);
        }
        assert!(workers[0].is_healthy());
        assert!(!workers[0].is_available());
        let info = SelectWorkerInfo {
            request_text: Some("hot prefix"),
            forbid_unleased_cache_owner_expansion: true,
            ..Default::default()
        };

        assert_eq!(policy.select_worker(&workers, &info), None);
        assert!(policy.replication_state.is_empty());
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("hot prefix");
        assert_eq!(matched.tenants.len(), 1);
        assert!(matched
            .tenants
            .iter()
            .all(|tenant| tenant.as_ref() != workers[1].url()));
    }

    #[test]
    fn distribution_guard_allows_one_cold_bootstrap_owner() {
        let policy = scheduler_pressure_policy();
        let workers = two_workers();
        policy.init_workers(&workers);
        policy.update_loads(&HashMap::from([
            (
                workers[0].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
            (
                workers[1].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));
        let info = SelectWorkerInfo {
            request_text: Some("new cold prefix"),
            forbid_unleased_cache_owner_expansion: true,
            ..Default::default()
        };

        let first = policy.select_worker(&workers, &info).unwrap();
        let second = policy.select_worker(&workers, &info).unwrap();
        assert_eq!(second, first);
        assert!(policy.replication_state.is_empty());
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("new cold prefix");
        assert_eq!(matched.tenants.len(), 1);
    }

    #[test]
    fn distribution_guard_serializes_concurrent_cold_bootstrap() {
        let policy = Arc::new(scheduler_pressure_policy());
        let workers = two_workers();
        policy.init_workers(&workers);
        policy.update_loads(&HashMap::from([
            (
                workers[0].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
            (
                workers[1].url().to_string(),
                engine_load_with_scheduler(0.10, 0.10, 0, 0, 37),
            ),
        ]));
        let barrier = Arc::new(std::sync::Barrier::new(16));

        let selected = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let policy = Arc::clone(&policy);
                    let workers = &workers;
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        let info = SelectWorkerInfo {
                            request_text: Some("concurrent cold prefix"),
                            forbid_unleased_cache_owner_expansion: true,
                            ..Default::default()
                        };
                        barrier.wait();
                        policy.select_worker(workers, &info).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert!(selected.iter().all(|idx| *idx == selected[0]));
        assert!(policy.replication_state.is_empty());
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("concurrent cold prefix");
        assert_eq!(matched.tenants.len(), 1);
    }

    #[test]
    fn distribution_guard_fails_closed_after_tree_bound_is_crossed() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            engine_load: true,
            eviction_interval_secs: 0,
            max_tree_size: 4,
            max_cached_owners_per_prefix: 2,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        });
        let workers = two_workers();
        policy.init_workers(&workers);
        let model_id = normalize_model_key(workers[0].model_id());
        let tree = policy.string_trees.get(model_id).unwrap().value().clone();
        tree.insert_text("oversized prefix", workers[0].url());
        assert!(tree
            .tenant_char_count
            .iter()
            .any(|entry| *entry.value() > policy.config.max_tree_size));
        assert!(!policy.distribution_index_within_bound(model_id));

        let info = SelectWorkerInfo {
            request_text: Some("oversized prefix continues"),
            forbid_unleased_cache_owner_expansion: true,
            ..Default::default()
        };
        assert_eq!(policy.select_worker(&workers, &info), None);
    }

    #[test]
    fn replication_ceiling_holds_even_when_cached_owner_is_hot() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            engine_load: true,
            balance_abs_threshold: usize::MAX,
            balance_token_usage_threshold: 1.0,
            overload_token_usage_threshold: 0.9,
            eviction_interval_secs: 0,
            max_cached_owners_per_prefix: 1,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        });
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "hot prefix");
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.95, 0.95, 4)),
            ("http://w2:8000".to_string(), engine_load(0.20, 0.20, 0)),
        ]));

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hot prefix"),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(selected, 0);
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("hot prefix");
        assert_eq!(matched.tenants.len(), 1);
    }

    #[test]
    fn sequential_prefix_extensions_stay_on_one_owner() {
        let policy = replication_budget_policy(8, 5);
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        prime_worker_one_affinity(&policy, &workers, "shared turn one");
        for _ in 0..20 {
            workers[0].increment_load();
        }

        for text in [
            "shared turn one turn two",
            "shared turn one turn two turn three",
        ] {
            let selected = policy
                .select_worker(
                    &workers,
                    &SelectWorkerInfo {
                        request_text: Some(text),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(selected, 0);
        }

        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("shared turn one turn two turn three");
        assert_eq!(matched.tenants.len(), 1);
    }

    #[test]
    fn replication_budget_excluded_owner_falls_back_and_replication_continues() {
        let policy = replication_budget_policy(1, 60);
        let workers = two_workers();
        prime_worker_one_affinity(&policy, &workers, "hot prefix");
        for _ in 0..20 {
            workers[0].increment_load();
        }
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-smg-excluded-worker-urls",
            workers[0].url().parse().unwrap(),
        );

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("hot prefix"),
                    headers: Some(&headers),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(selected, 1, "an unsuitable owner must not trap the request");
        let model_id = normalize_model_key(workers[0].model_id());
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("hot prefix");
        assert_eq!(matched.tenants.len(), 2);
    }

    #[test]
    fn replication_spill_claim_is_atomic_under_concurrency() {
        let policy = replication_budget_policy(8, 60);
        let workers = make_workers(&[
            "http://w1:8000",
            "http://w2:8000",
            "http://w3:8000",
            "http://w4:8000",
        ]);
        policy.init_workers(&workers);
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let selected = std::sync::Mutex::new(Vec::new());
        let key = PrefixBudgetKey {
            model_hash: 1,
            prefix_hash: 2,
            kind: PrefixKind::String,
        };
        let model_id = normalize_model_key(workers[0].model_id());

        std::thread::scope(|scope| {
            for _ in 0..16 {
                let barrier = Arc::clone(&barrier);
                let selected = &selected;
                let policy = &policy;
                let workers = &workers;
                scope.spawn(move || {
                    barrier.wait();
                    let info = SelectWorkerInfo {
                        request_text: Some("cold hot-prefix request"),
                        reserve_work: true,
                        ..Default::default()
                    };
                    let (idx, _) = policy
                        .select_or_join_replication_spill(
                            key,
                            workers,
                            &info,
                            &(0..workers.len()).collect::<Vec<_>>(),
                            model_id,
                        )
                        .unwrap();
                    selected.lock().unwrap().push(idx);
                });
            }
        });

        let selected = selected.into_inner().unwrap();
        assert!(selected.iter().all(|idx| *idx == selected[0]));
        assert_eq!(policy.replication_state.len(), 1);
        let matched = policy
            .string_trees
            .get(model_id)
            .unwrap()
            .match_prefix_with_counts("cold hot-prefix request");
        assert_eq!(matched.tenants.len(), 1);
    }

    #[test]
    fn test_cache_aware_with_imbalanced_load() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.5,
            balance_abs_threshold: 5,
            balance_rel_threshold: 2.0,
            eviction_interval_secs: 0, // Disable eviction thread
            max_tree_size: 10000,
            fallback_output_token_estimate: 4096,
            block_size: 16,
            engine_load: false,
            balance_token_usage_threshold: 1.0,
            overload_token_usage_threshold: 1.0,
            ..Default::default()
        });

        let worker1 = BasicWorkerBuilder::new("http://w1:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        let worker2 = BasicWorkerBuilder::new("http://w2:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();

        // Create significant load imbalance
        for _ in 0..20 {
            worker1.increment_load();
        }
        // worker2 has load 0

        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(worker1), Arc::new(worker2)];
        policy.init_workers(&workers);

        // Should select worker2 (lower load) despite cache affinity
        let info = SelectWorkerInfo {
            request_text: Some("test"),
            ..Default::default()
        };
        for _ in 0..5 {
            let idx = policy.select_worker(&workers, &info).unwrap();
            assert_eq!(idx, 1); // Should always pick worker2
        }
    }

    // ---- is_imbalanced: 3-term trigger (overload ∨ KV-spread ∨ count) ----

    /// Single-DP load snapshot reporting the given KV utilization (0.0–1.0).
    fn kv_load(token_usage: f64) -> WorkerLoadResponse {
        WorkerLoadResponse {
            loads: vec![SchedulerLoadSnapshot {
                token_usage,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Healthy workers (health checks disabled) for the given URLs.
    fn make_workers(urls: &[&str]) -> Vec<Arc<dyn Worker>> {
        urls.iter()
            .map(|u| {
                Arc::new(
                    BasicWorkerBuilder::new(*u)
                        .worker_type(WorkerType::Regular)
                        .health_config(no_health_check())
                        .build(),
                ) as Arc<dyn Worker>
            })
            .collect()
    }

    /// Inject a backend KV snapshot (utilization per worker, by index). Returns
    /// the sender; bind it (`let _tx = ...`) to keep the watch channel open.
    fn inject_kv(
        policy: &CacheAwarePolicy,
        workers: &[Arc<dyn Worker>],
        usages: &[f64],
    ) -> watch::Sender<HashMap<String, WorkerLoadResponse>> {
        let map: HashMap<String, WorkerLoadResponse> = workers
            .iter()
            .zip(usages)
            .map(|(w, &u)| (w.url().to_string(), kv_load(u)))
            .collect();
        let (tx, rx) = watch::channel(map);
        policy.set_load_receiver(Some(rx));
        tx
    }

    #[test]
    fn engine_pressure_filters_before_upstream_imbalance_routing() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            engine_load: true,
            balance_abs_threshold: 5,
            balance_rel_threshold: 2.0,
            balance_token_usage_threshold: 0.3,
            overload_token_usage_threshold: 0.9,
            eviction_interval_secs: 0,
            ..Default::default()
        });
        let workers = two_workers();
        policy.init_workers(&workers);

        // Without the outer engine filter, both upstream imbalance signals
        // fire and shortest-queue would choose hot worker 0.
        for _ in 0..20 {
            workers[1].increment_load();
        }
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.95, 0.95, 0)),
            ("http://w2:8000".to_string(), engine_load(0.10, 0.10, 0)),
        ]));
        let _tx = inject_kv(&policy, &workers, &[0.95, 0.10]);

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("shared prefix"),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(
            selected, 1,
            "upstream imbalance routing must not re-admit a worker excluded by engine pressure"
        );
    }

    /// Config isolating the KV triggers (count effectively disabled): `balance`
    /// is the spread threshold, `overload` the ceiling.
    fn kv_only_config(balance_spread: f32, overload_ceiling: f32) -> CacheAwareConfig {
        CacheAwareConfig {
            balance_abs_threshold: usize::MAX,
            eviction_interval_secs: 0,
            balance_token_usage_threshold: balance_spread,
            overload_token_usage_threshold: overload_ceiling,
            ..Default::default()
        }
    }

    fn all_healthy(workers: &[Arc<dyn Worker>]) -> Vec<usize> {
        (0..workers.len()).collect()
    }

    /// Run the imbalance check the way `select_worker` does: fold the request-count
    /// bounds over the healthy workers (production gathers them in one pass), then
    /// call `is_imbalanced`.
    fn imbalanced(policy: &CacheAwarePolicy, workers: &[Arc<dyn Worker>]) -> bool {
        let healthy = all_healthy(workers);
        let (min_load, max_load) = healthy
            .iter()
            .fold((usize::MAX, 0usize), |(min, max), &idx| {
                let load = workers[idx].load();
                (min.min(load), max.max(load))
            });
        let min_load = if min_load == usize::MAX { 0 } else { min_load };
        policy.is_imbalanced(workers, &healthy, min_load, max_load)
    }

    #[test]
    fn is_imbalanced_uniform_high_kv_does_not_fire() {
        // All engines equally saturated: high utilization, zero spread.
        let policy = CacheAwarePolicy::with_config(kv_only_config(0.3, 0.95));
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        let _tx = inject_kv(&policy, &workers, &[0.9, 0.9, 0.9]);
        // max 0.9 < 0.95 ceiling, spread 0.0 < 0.3 → keep cache affinity.
        assert!(
            !imbalanced(&policy, &workers),
            "uniform-high KV (no cooler home) must not abandon cache affinity"
        );
    }

    #[test]
    fn is_imbalanced_one_hot_rest_idle_fires_via_spread() {
        // Same hottest engine (0.9) as the uniform case, but neighbors are idle.
        let policy = CacheAwarePolicy::with_config(kv_only_config(0.3, 0.95));
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        let _tx = inject_kv(&policy, &workers, &[0.9, 0.15, 0.15]);
        // spread 0.75 > 0.3 → spill toward a cooler engine.
        assert!(
            imbalanced(&policy, &workers),
            "a hot engine with idle neighbors (large KV spread) must rebalance"
        );
    }

    #[test]
    fn is_imbalanced_overload_ceiling_fires_below_spread() {
        // Critically hot engine, but the spread is under the balance threshold.
        let policy = CacheAwarePolicy::with_config(kv_only_config(0.3, 0.95));
        let workers = make_workers(&["http://w1:8000", "http://w2:8000"]);
        let _tx = inject_kv(&policy, &workers, &[0.97, 0.80]);
        // spread 0.17 < 0.3 (balance quiet) but 0.97 > 0.95 ceiling → shed.
        assert!(
            imbalanced(&policy, &workers),
            "a critically-saturated engine must shed even below the spread threshold"
        );
    }

    #[test]
    fn is_imbalanced_high_count_low_kv_caught_by_count() {
        // KV is even, so both KV triggers stay quiet — count must still catch it.
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            balance_abs_threshold: 5,
            balance_rel_threshold: 2.0,
            eviction_interval_secs: 0,
            balance_token_usage_threshold: 0.3,
            overload_token_usage_threshold: 0.95,
            ..Default::default()
        });
        let workers = make_workers(&["http://w1:8000", "http://w2:8000"]);
        let _tx = inject_kv(&policy, &workers, &[0.3, 0.3]);
        for _ in 0..20 {
            workers[0].increment_load();
        }
        // KV spread 0.0, max 0.3 → KV quiet; count 20 vs 0 → fire.
        assert!(
            imbalanced(&policy, &workers),
            "count spread must still trigger when KV utilization looks even"
        );
    }

    #[test]
    fn is_imbalanced_kv_disabled_by_default_ignores_snapshot() {
        // Default config: both KV thresholds 1.0 (disabled).
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            eviction_interval_secs: 0,
            ..Default::default()
        });
        let workers = make_workers(&["http://w1:8000", "http://w2:8000"]);
        // A massive KV spread that WOULD fire if KV balancing were enabled...
        let _tx = inject_kv(&policy, &workers, &[0.95, 0.05]);
        // ...is ignored at the 1.0 default; counts balanced → no rebalance.
        assert!(
            !imbalanced(&policy, &workers),
            "default thresholds (1.0) must ignore KV usage entirely"
        );
    }

    #[test]
    fn test_cache_aware_worker_removal() {
        let config = CacheAwareConfig {
            eviction_interval_secs: 0, // Disable eviction thread
            ..Default::default()
        };
        let policy = CacheAwarePolicy::with_config(config);
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];

        policy.init_workers(&workers);

        // Route some requests
        policy.select_worker(
            &workers,
            &SelectWorkerInfo {
                request_text: Some("test1"),
                ..Default::default()
            },
        );
        policy.select_worker(
            &workers,
            &SelectWorkerInfo {
                request_text: Some("test2"),
                ..Default::default()
            },
        );

        // Remove a worker
        policy.remove_worker_by_url("http://w1:8000");
        workers[0].set_status(WorkerStatus::NotReady);

        // All requests should now go to worker2
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("test1"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_apply_known_remote_insert_round_trip() {
        // Seed both kinds via `apply_repair_page` (the v2 cold-start
        // path that populates hash_index), then verify
        // `apply_known_remote_insert` resolves the hash and returns
        // true. Unknown hashes return false. Wrong-kind lookups
        // against the same hash return false (model + kind scope
        // the index).
        let config = CacheAwareConfig {
            eviction_interval_secs: 0,
            ..Default::default()
        };
        let policy = CacheAwarePolicy::with_config(config);

        let text = "remote_text";
        let tokens = vec![1u32, 2, 3, 4];
        let string_page = TreeRepairPage {
            session_id: uuid::Uuid::now_v7(),
            model_id: "model1".to_string(),
            tree_kind: TreeKind::String,
            page_index: 0,
            entries: vec![RepairEntry::String {
                path: text.to_string(),
                tenants: vec![(Arc::from("http://w1"), 1)],
            }],
            next_cursor: None,
            is_last: true,
        };
        assert_eq!(policy.apply_repair_page(&string_page), 1);

        let token_page = TreeRepairPage {
            session_id: uuid::Uuid::now_v7(),
            model_id: "model1".to_string(),
            tree_kind: TreeKind::Token,
            page_index: 0,
            entries: vec![RepairEntry::Token {
                tokens: tokens.clone(),
                tenants: vec![(Arc::from("http://w1"), 1)],
            }],
            next_cursor: None,
            is_last: true,
        };
        assert_eq!(policy.apply_repair_page(&token_page), 1);

        let text_hash = kv_index::hash_node_path(text);
        let token_hash = kv_index::hash_token_path(&tokens);

        // Known hashes apply for the matching kind.
        assert!(policy.apply_known_remote_insert(
            "model1",
            TreeKind::String,
            text_hash,
            "http://w2",
        ));
        assert!(policy.apply_known_remote_insert(
            "model1",
            TreeKind::Token,
            token_hash,
            "http://w2",
        ));

        // Same hash but wrong kind doesn't alias.
        assert!(!policy.apply_known_remote_insert(
            "model1",
            TreeKind::Token,
            text_hash,
            "http://w2",
        ));

        // Unknown hash, unknown model → false.
        assert!(!policy.apply_known_remote_insert(
            "model1",
            TreeKind::String,
            0xDEAD_BEEF,
            "http://w2",
        ));
        assert!(!policy.apply_known_remote_insert(
            "unknown_model",
            TreeKind::String,
            text_hash,
            "http://w2",
        ));
    }

    #[test]
    fn test_apply_repair_page_seeds_hash_index() {
        let config = CacheAwareConfig {
            eviction_interval_secs: 0,
            ..Default::default()
        };
        let policy = CacheAwarePolicy::with_config(config);
        let text = "repaired text";
        let tokens = vec![1u32; 16];

        let string_page = TreeRepairPage {
            session_id: uuid::Uuid::now_v7(),
            model_id: "model1".to_string(),
            tree_kind: TreeKind::String,
            page_index: 0,
            entries: vec![RepairEntry::String {
                path: text.to_string(),
                tenants: vec![(Arc::from("http://w1"), 1)],
            }],
            next_cursor: None,
            is_last: true,
        };
        assert_eq!(policy.apply_repair_page(&string_page), 1);
        assert!(policy.apply_known_remote_insert(
            "model1",
            TreeKind::String,
            kv_index::hash_node_path(text),
            "http://w2",
        ));

        let token_page = TreeRepairPage {
            session_id: uuid::Uuid::now_v7(),
            model_id: "model1".to_string(),
            tree_kind: TreeKind::Token,
            page_index: 0,
            entries: vec![RepairEntry::Token {
                tokens: tokens.clone(),
                tenants: vec![(Arc::from("http://w1"), 1)],
            }],
            next_cursor: None,
            is_last: true,
        };
        assert_eq!(policy.apply_repair_page(&token_page), 1);
        assert!(policy.apply_known_remote_insert(
            "model1",
            TreeKind::Token,
            kv_index::hash_token_path(&tokens),
            "http://w2",
        ));
    }

    #[test]
    fn test_apply_known_remote_insert_from_request_hot_path() {
        // Companion to `test_apply_known_remote_insert_round_trip`.
        // That test seeds via `apply_repair_page`, which stores
        // full text/tokens. The local request hot path
        // (`select_worker_with_text` / `_with_tokens` plus the
        // imbalanced fallback) stores the *matched prefix* shape
        // instead. A regression on the matched-prefix apply path
        // would still pass the full-path test, so seed via
        // `select_worker` here and assert apply succeeds.
        //
        // Opt into request-hot-path hash_index population — without
        // this the populate sites are no-ops and the apply call
        // below would have nothing to resolve. In production this
        // flag is flipped by the mesh wiring code; here we set it
        // directly because the test mimics the mesh consumer.
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            eviction_interval_secs: 0,
            ..Default::default()
        });
        policy.set_populate_hash_index(true);
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        // Drive a string request through select_worker — populates
        // the string-side hash_index with a matched-prefix value.
        let text = "the quick brown fox jumps over the lazy dog";
        policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some(text),
                    ..Default::default()
                },
            )
            .unwrap();
        let text_hash = kv_index::hash_node_path(text);

        // Drive a token request — populates the token-side
        // hash_index. select_worker uses the model_id from the
        // first worker's `model_id()`, which the builder leaves
        // empty → UNKNOWN_MODEL_ID after normalization.
        let tokens: Vec<u32> = (0..32).collect();
        policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&tokens),
                    ..Default::default()
                },
            )
            .unwrap();
        let token_hash = kv_index::hash_token_path(&tokens);

        // Both populate sites use UNKNOWN_MODEL_ID for these
        // workers (no model_id set on the builder), and the
        // resolver normalizes empty → UNKNOWN_MODEL_ID, so an
        // empty model_id resolves the same entries the populate
        // sites wrote.
        assert!(policy.apply_known_remote_insert("", TreeKind::String, text_hash, "http://remote",));
        assert!(policy.apply_known_remote_insert("", TreeKind::Token, token_hash, "http://remote",));
    }

    #[test]
    fn test_cache_aware_without_mesh() {
        let config = CacheAwareConfig {
            eviction_interval_secs: 0,
            ..Default::default()
        };
        let policy = CacheAwarePolicy::with_config(config);

        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(
            BasicWorkerBuilder::new("http://w1:8000")
                .worker_type(WorkerType::Regular)
                .api_key("test_api_key")
                .health_config(no_health_check())
                .build(),
        )];

        policy.init_workers(&workers);

        // Should work without mesh
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("test request"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, 0);
    }

    // -----------------------------------------------------------------------
    // Event-driven routing tests (Type 1: PositionalIndexer overlap scoring)
    // -----------------------------------------------------------------------

    /// Helper: create a PositionalIndexer and store blocks for a worker.
    /// `token_chunks` is a list of token-id slices — each becomes one block.
    fn setup_indexer_with_blocks(
        worker_url: &str,
        token_chunks: &[&[u32]],
        jump_size: usize,
    ) -> Arc<PositionalIndexer> {
        let indexer = Arc::new(PositionalIndexer::new(jump_size));
        let worker_id = indexer.intern_worker(worker_url).unwrap();
        let mut wb = WorkerBlockMap::default();
        let blocks: Vec<StoredBlock> = token_chunks
            .iter()
            .enumerate()
            .map(|(i, tokens)| StoredBlock {
                seq_hash: SequenceHash(i as u64 + 1),
                content_hash: compute_content_hash(tokens),
            })
            .collect();
        indexer
            .apply_stored(worker_id, &blocks, None, &mut wb)
            .unwrap();
        indexer
    }

    fn test_config() -> CacheAwareConfig {
        CacheAwareConfig {
            eviction_interval_secs: 0,
            block_size: 4, // small block size for easy test setup
            ..Default::default()
        }
    }

    // -- score_overlap unit tests (scoring helper) --

    #[test]
    fn test_score_overlap_selects_best_match() {
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        // Store 4 blocks for w1: tokens [1..16] in blocks of 4
        let indexer = setup_indexer_with_blocks(
            "http://w1:8000",
            &[
                &[1, 2, 3, 4],
                &[5, 6, 7, 8],
                &[9, 10, 11, 12],
                &[13, 14, 15, 16],
            ],
            4,
        );

        // Query with matching tokens — should select w1
        let result = CacheAwarePolicy::score_overlap(
            &workers,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            &[0, 1],
            &indexer,
            4,
        );
        assert_eq!(result, vec![0]); // w1
    }

    #[test]
    fn test_score_overlap_no_match_returns_none() {
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(
            BasicWorkerBuilder::new("http://w1:8000")
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        )];
        policy.init_workers(&workers);

        let indexer =
            setup_indexer_with_blocks("http://w1:8000", &[&[1, 2, 3, 4], &[5, 6, 7, 8]], 4);

        // Completely different tokens — no overlap → None
        let result = CacheAwarePolicy::score_overlap(
            &workers,
            &[100, 200, 300, 400, 500, 600, 700, 800],
            &[0],
            &indexer,
            4,
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_score_overlap_load_tiebreak() {
        let policy = CacheAwarePolicy::with_config(test_config());

        let w1 = BasicWorkerBuilder::new("http://w1:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        let w2 = BasicWorkerBuilder::new("http://w2:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();

        // Give w1 higher load
        for _ in 0..10 {
            w1.increment_load();
        }

        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(w1), Arc::new(w2)];
        policy.init_workers(&workers);

        // Store same blocks for both workers (equal overlap)
        let indexer = Arc::new(PositionalIndexer::new(4));
        let w1_id = indexer.intern_worker("http://w1:8000").unwrap();
        let w2_id = indexer.intern_worker("http://w2:8000").unwrap();
        let mut wb1 = WorkerBlockMap::default();
        let mut wb2 = WorkerBlockMap::default();
        let blocks = vec![StoredBlock {
            seq_hash: SequenceHash(1),
            content_hash: compute_content_hash(&[1, 2, 3, 4]),
        }];
        indexer
            .apply_stored(w1_id, &blocks, None, &mut wb1)
            .unwrap();
        let blocks2 = vec![StoredBlock {
            seq_hash: SequenceHash(1),
            content_hash: compute_content_hash(&[1, 2, 3, 4]),
        }];
        indexer
            .apply_stored(w2_id, &blocks2, None, &mut wb2)
            .unwrap();

        // Equal overlap returns both owners; routing applies the atomic load tie-break.
        let result = CacheAwarePolicy::score_overlap(&workers, &[1, 2, 3, 4], &[0, 1], &indexer, 4);
        assert_eq!(result, vec![0, 1]);
    }

    #[test]
    fn test_score_overlap_tree_size_tiebreak() {
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        let indexer = Arc::new(PositionalIndexer::new(4));
        let w1_id = indexer.intern_worker("http://w1:8000").unwrap();
        let w2_id = indexer.intern_worker("http://w2:8000").unwrap();
        let mut wb1 = WorkerBlockMap::default();
        let mut wb2 = WorkerBlockMap::default();

        // Both workers have block [1,2,3,4] (equal overlap, equal load)
        let block = vec![StoredBlock {
            seq_hash: SequenceHash(1),
            content_hash: compute_content_hash(&[1, 2, 3, 4]),
        }];
        indexer.apply_stored(w1_id, &block, None, &mut wb1).unwrap();

        // w2 has the same block plus extra blocks → larger tree
        let block2 = vec![StoredBlock {
            seq_hash: SequenceHash(1),
            content_hash: compute_content_hash(&[1, 2, 3, 4]),
        }];
        indexer
            .apply_stored(w2_id, &block2, None, &mut wb2)
            .unwrap();
        let extra = vec![StoredBlock {
            seq_hash: SequenceHash(2),
            content_hash: compute_content_hash(&[5, 6, 7, 8]),
        }];
        indexer
            .apply_stored(w2_id, &extra, Some(SequenceHash(1)), &mut wb2)
            .unwrap();

        // Equal overlap returns both owners; tree size no longer hides a valid owner.
        let result = CacheAwarePolicy::score_overlap(&workers, &[1, 2, 3, 4], &[0, 1], &indexer, 4);
        assert_eq!(result, vec![0, 1]);
    }

    #[test]
    fn test_score_overlap_short_request_returns_none() {
        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(
            BasicWorkerBuilder::new("http://w1:8000")
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        )];

        let indexer = setup_indexer_with_blocks("http://w1:8000", &[&[1, 2, 3, 4]], 4);

        // Request shorter than block_size → no full blocks → None
        let result = CacheAwarePolicy::score_overlap(&workers, &[1, 2, 3], &[0], &indexer, 4);
        assert!(result.is_empty());
    }

    #[test]
    fn test_score_overlap_partial_match() {
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        let indexer = Arc::new(PositionalIndexer::new(4));
        let w1_id = indexer.intern_worker("http://w1:8000").unwrap();
        let w2_id = indexer.intern_worker("http://w2:8000").unwrap();
        let mut wb1 = WorkerBlockMap::default();
        let mut wb2 = WorkerBlockMap::default();

        // w1 has 4 blocks cached
        let blocks_w1: Vec<StoredBlock> = (0..4)
            .map(|i| StoredBlock {
                seq_hash: SequenceHash(i as u64 + 1),
                content_hash: compute_content_hash(&[
                    (i * 4 + 1) as u32,
                    (i * 4 + 2) as u32,
                    (i * 4 + 3) as u32,
                    (i * 4 + 4) as u32,
                ]),
            })
            .collect();
        indexer
            .apply_stored(w1_id, &blocks_w1, None, &mut wb1)
            .unwrap();

        // w2 has only the first 2 blocks (partial overlap with same request)
        let blocks_w2: Vec<StoredBlock> = (0..2)
            .map(|i| StoredBlock {
                seq_hash: SequenceHash(i as u64 + 1),
                content_hash: compute_content_hash(&[
                    (i * 4 + 1) as u32,
                    (i * 4 + 2) as u32,
                    (i * 4 + 3) as u32,
                    (i * 4 + 4) as u32,
                ]),
            })
            .collect();
        indexer
            .apply_stored(w2_id, &blocks_w2, None, &mut wb2)
            .unwrap();

        // Query with all 4 blocks worth of tokens → w1 wins (higher overlap: 4 vs 2)
        let result = CacheAwarePolicy::score_overlap(
            &workers,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            &[0, 1],
            &indexer,
            4,
        );
        assert_eq!(result, vec![0]); // w1 (higher overlap)
    }

    // -- select_worker_event_driven integration tests --

    #[test]
    fn test_event_driven_overlap_selects_cached_worker() {
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        // Set up monitor with indexer data for "unknown" model
        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        let indexer =
            setup_indexer_with_blocks("http://w1:8000", &[&[1, 2, 3, 4], &[5, 6, 7, 8]], 4);
        monitor.indexers.insert("unknown".to_string(), indexer);
        policy.set_kv_event_monitor(Some(monitor));

        // Full dispatch: should use event-driven and select w1
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[1, 2, 3, 4, 5, 6, 7, 8]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, 0); // w1 (has cached blocks)
    }

    #[test]
    fn test_event_driven_replication_budget_uses_least_loaded_cached_owner() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            balance_abs_threshold: 0,
            balance_rel_threshold: 1.0,
            eviction_interval_secs: 0,
            block_size: 4,
            max_cached_owners_per_prefix: 2,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        });
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        for _ in 0..20 {
            workers[0].increment_load();
        }
        for _ in 0..10 {
            workers[1].increment_load();
        }
        policy.init_workers(&workers);

        let indexer = Arc::new(PositionalIndexer::new(4));
        let block = vec![StoredBlock {
            seq_hash: SequenceHash(1),
            content_hash: compute_content_hash(&[1, 2, 3, 4]),
        }];
        let w1_id = indexer.intern_worker(workers[0].url()).unwrap();
        let w2_id = indexer.intern_worker(workers[1].url()).unwrap();
        indexer
            .apply_stored(w1_id, &block, None, &mut WorkerBlockMap::default())
            .unwrap();
        indexer
            .apply_stored(w2_id, &block, None, &mut WorkerBlockMap::default())
            .unwrap();
        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        monitor.indexers.insert("unknown".to_string(), indexer);
        policy.set_kv_event_monitor(Some(monitor));

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[1, 2, 3, 4]),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(
            selected, 1,
            "the event index must expose both owners and avoid uncached worker 3"
        );
    }

    #[test]
    fn event_driven_unrelated_hot_worker_does_not_break_affinity() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            engine_load: true,
            balance_token_usage_threshold: 0.30,
            overload_token_usage_threshold: 0.90,
            eviction_interval_secs: 0,
            block_size: 4,
            max_cached_owners_per_prefix: 8,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        });
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        policy.init_workers(&workers);
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.84, 0.84, 1)),
            ("http://w2:8000".to_string(), engine_load(0.34, 0.34, 0)),
            ("http://w3:8000".to_string(), engine_load(0.95, 0.95, 8)),
        ]));
        let _tx = inject_kv(&policy, &workers, &[0.84, 0.34, 0.95]);

        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        monitor.indexers.insert(
            "unknown".to_string(),
            setup_indexer_with_blocks(workers[0].url(), &[&[1, 2, 3, 4]], 4),
        );
        policy.set_kv_event_monitor(Some(monitor));

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[1, 2, 3, 4]),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(selected, 0);
    }

    #[test]
    fn event_driven_owner_pressure_spill_reuses_provisional_owner() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            engine_load: true,
            eviction_interval_secs: 0,
            block_size: 4,
            max_cached_owners_per_prefix: 8,
            cache_owner_spill_cooldown_secs: 60,
            ..Default::default()
        });
        let workers = make_workers(&["http://w1:8000", "http://w2:8000", "http://w3:8000"]);
        policy.init_workers(&workers);
        policy.update_loads(&HashMap::from([
            ("http://w1:8000".to_string(), engine_load(0.95, 0.95, 8)),
            ("http://w2:8000".to_string(), engine_load(0.20, 0.20, 0)),
            ("http://w3:8000".to_string(), engine_load(0.20, 0.20, 0)),
        ]));

        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        monitor.indexers.insert(
            "unknown".to_string(),
            setup_indexer_with_blocks(workers[0].url(), &[&[1, 2, 3, 4]], 4),
        );
        policy.set_kv_event_monitor(Some(monitor));
        let info = SelectWorkerInfo {
            tokens: Some(&[1, 2, 3, 4]),
            ..Default::default()
        };

        let first = policy.select_worker(&workers, &info).unwrap();
        let second = policy.select_worker(&workers, &info).unwrap();

        assert_ne!(first, 0);
        assert_eq!(second, first);
        assert_eq!(policy.replication_state.len(), 1);
    }

    #[test]
    fn test_event_driven_no_overlap_uses_min_load() {
        let policy = CacheAwarePolicy::with_config(test_config());

        let w1 = BasicWorkerBuilder::new("http://w1:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        let w2 = BasicWorkerBuilder::new("http://w2:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        // Give w1 higher load so min-load picks w2
        for _ in 0..3 {
            w1.increment_load();
        }

        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(w1), Arc::new(w2)];
        policy.init_workers(&workers);

        // Monitor has indexer with data, but tokens don't match
        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        let indexer = setup_indexer_with_blocks("http://w1:8000", &[&[1, 2, 3, 4]], 4);
        monitor.indexers.insert("unknown".to_string(), indexer);
        policy.set_kv_event_monitor(Some(monitor));

        // No overlap → event-driven falls back to min-load (not token tree)
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[100, 200, 300, 400]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, 1); // w2 (min load), NOT token tree result
    }

    #[test]
    fn test_event_driven_short_request_uses_min_load() {
        let policy = CacheAwarePolicy::with_config(test_config()); // block_size=4

        let w1 = BasicWorkerBuilder::new("http://w1:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        let w2 = BasicWorkerBuilder::new("http://w2:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        for _ in 0..3 {
            w1.increment_load();
        }

        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(w1), Arc::new(w2)];
        policy.init_workers(&workers);

        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        let indexer = setup_indexer_with_blocks("http://w1:8000", &[&[1, 2, 3, 4]], 4);
        monitor.indexers.insert("unknown".to_string(), indexer);
        policy.set_kv_event_monitor(Some(monitor));

        // Request shorter than block_size → no full blocks → min-load fallback
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[1, 2, 3]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, 1); // w2 (min load)
    }

    #[test]
    fn test_no_monitor_uses_token_tree() {
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        // No kv_monitor → has_event_indexer returns false → uses token tree
        assert!(!policy.has_event_indexer("unknown"));

        // Should still route (via token tree, not event-driven)
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[1, 2, 3, 4]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(idx < 2); // valid worker selected
    }

    #[test]
    fn test_set_kv_event_monitor() {
        let policy = CacheAwarePolicy::with_config(test_config());

        // Initially no monitor
        assert!(policy.kv_monitor.read().is_none());

        // Set monitor (works via &self thanks to interior mutability)
        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        policy.set_kv_event_monitor(Some(Arc::clone(&monitor)));
        assert!(policy.kv_monitor.read().is_some());

        // get_indexer returns None for unknown model
        assert!(monitor.get_indexer("nonexistent").is_none());

        // Clear monitor
        policy.set_kv_event_monitor(None);
        assert!(policy.kv_monitor.read().is_none());
    }

    #[test]
    fn test_event_driven_uses_monitor_block_size() {
        // Test that event-driven routing uses monitor's learned block_size
        // instead of config default when available.
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            block_size: 4, // config default
            eviction_interval_secs: 0,
            ..Default::default()
        });

        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        let monitor = Arc::new(KvEventMonitor::new(Some(4)));

        // Store blocks using block_size=8 (tokens chunked in groups of 8)
        let indexer = Arc::new(PositionalIndexer::new(4));
        let w1_id = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();
        let block = vec![StoredBlock {
            seq_hash: SequenceHash(1),
            content_hash: compute_content_hash(&[1, 2, 3, 4, 5, 6, 7, 8]),
        }];
        indexer.apply_stored(w1_id, &block, None, &mut wb).unwrap();
        monitor
            .indexers
            .insert("unknown".to_string(), indexer.clone());

        // Set block_size=8 in monitor (simulating learned from events)
        monitor.set_block_size("unknown", 8);

        policy.set_kv_event_monitor(Some(monitor));

        // Query with 8 tokens — with block_size=8, this is one full block
        // With config block_size=4, this would be two blocks and wouldn't match
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[1, 2, 3, 4, 5, 6, 7, 8]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, 0); // w1 has the cached block
    }

    #[test]
    fn test_imbalanced_skips_event_driven() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            balance_abs_threshold: 5,
            balance_rel_threshold: 2.0,
            eviction_interval_secs: 0,
            block_size: 4,
            balance_token_usage_threshold: 1.0,
            overload_token_usage_threshold: 1.0,
            ..Default::default()
        });

        let w1 = BasicWorkerBuilder::new("http://w1:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        let w2 = BasicWorkerBuilder::new("http://w2:8000")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();

        // Create heavy imbalance: w1 has 20 load, w2 has 0
        for _ in 0..20 {
            w1.increment_load();
        }

        let workers: Vec<Arc<dyn Worker>> = vec![Arc::new(w1), Arc::new(w2)];
        policy.init_workers(&workers);

        // Even though we set up event monitor, imbalance check fires first
        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        policy.set_kv_event_monitor(Some(monitor));

        // With imbalance, select_worker should pick min-load (w2), not event-driven
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&[1, 2, 3, 4]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, 1); // w2 (min load), regardless of event data
    }

    #[test]
    fn test_empty_indexer_falls_through_to_token_tree() {
        // When the monitor has an indexer for a model but the indexer is empty
        // (startup, reconnect), routing should fall through to the token tree
        // instead of taking the event-driven path and landing on min-load.
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers: Vec<Arc<dyn Worker>> = vec![
            Arc::new(
                BasicWorkerBuilder::new("http://w1:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
            Arc::new(
                BasicWorkerBuilder::new("http://w2:8000")
                    .worker_type(WorkerType::Regular)
                    .health_config(no_health_check())
                    .build(),
            ),
        ];
        policy.init_workers(&workers);

        // Set up monitor with an empty indexer
        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        let empty_indexer = Arc::new(PositionalIndexer::new(4));
        monitor
            .indexers
            .insert("unknown".to_string(), empty_indexer);
        policy.set_kv_event_monitor(Some(monitor));

        // Empty indexer → has_event_indexer returns false → falls through to token tree
        assert!(!policy.has_event_indexer("unknown"));

        // Tokens must be >= PAGE_SIZE (16) to populate the tree; shorter
        // sequences are uncacheable and fall through to min-load.
        let tokens: Vec<u32> = (1..=16).collect();

        // First request populates the token tree for the selected worker.
        let idx = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&tokens),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(idx < 2); // valid worker via token tree

        // Same tokens again — token-tree cache hit routes to the same worker.
        let idx2 = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&tokens),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(idx, idx2); // token tree cache affinity preserved
    }

    #[test]
    fn test_nonempty_event_index_miss_uses_provisional_token_owner() {
        // A late SGLang subscription can yield a nonempty event index whose
        // first stored block references an unseen parent. Exact lookup then
        // misses even though earlier routed traffic established an owner in
        // the provisional token tree.
        let policy = CacheAwarePolicy::with_config(test_config());
        let workers = two_workers();
        policy.init_workers(&workers);

        let tokens: Vec<u32> = (1..=16).collect();
        let model_id = normalize_model_key(workers[0].model_id());
        policy
            .token_trees
            .get(model_id)
            .unwrap()
            .insert_tokens(&tokens, workers[0].url());

        // Make a plain P2C miss prefer w2, proving the result below comes from
        // provisional cache affinity rather than load balancing coincidence.
        workers[0].increment_load();

        let monitor = Arc::new(KvEventMonitor::new(Some(4)));
        let indexer = Arc::new(PositionalIndexer::new(4));
        let worker_id = indexer.intern_worker(workers[1].url()).unwrap();
        let mut worker_blocks = WorkerBlockMap::default();
        indexer
            .apply_stored(
                worker_id,
                &[StoredBlock {
                    seq_hash: SequenceHash(999),
                    content_hash: compute_content_hash(&[100, 101, 102, 103]),
                }],
                None,
                &mut worker_blocks,
            )
            .unwrap();
        monitor.indexers.insert(model_id.to_string(), indexer);
        monitor.set_block_size(model_id, 4);
        policy.set_kv_event_monitor(Some(monitor));
        assert!(policy.has_event_indexer(model_id));

        let selected = policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    tokens: Some(&tokens),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(selected, 0);
    }

    #[test]
    fn test_reserved_long_prompt_spills_without_request_count_imbalance() {
        let policy = CacheAwarePolicy::with_config(CacheAwareConfig {
            cache_threshold: 0.0,
            balance_abs_threshold: usize::MAX,
            balance_rel_threshold: 1.5,
            eviction_interval_secs: 0,
            fallback_output_token_estimate: 4096,
            ..Default::default()
        });
        let workers = two_workers();
        policy.init_workers(&workers);

        let tokens: Vec<u32> = (1..=16).collect();
        let model_id = normalize_model_key(workers[0].model_id());
        policy
            .token_trees
            .get(model_id)
            .unwrap()
            .insert_tokens(&tokens, workers[0].url());

        let info = SelectWorkerInfo {
            tokens: Some(&tokens),
            reserve_work: true,
            ..Default::default()
        };
        let first = policy.select_worker(&workers, &info).unwrap();
        assert_eq!(
            first, 0,
            "the existing cached owner should receive the first request"
        );

        // Active-request counts are still equal because no execution guard was
        // created. The synchronous 4,112-token reservation alone must make the
        // second admission use P2C and choose the unreserved worker.
        let second = policy.select_worker(&workers, &info).unwrap();
        assert_eq!(second, 1);

        let cost = policy.reservation_cost(&info).unwrap();
        policy.release_reservation(workers[first].url(), cost);
        policy.release_reservation(workers[second].url(), cost);
    }
}
