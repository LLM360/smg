//! Per-worker KV cache event subscription manager.
//!
//! `KvEventMonitor` spawns a background tokio task per gRPC worker that subscribes
//! to KV cache events and feeds them into a shared `PositionalIndexer` (one per model).
//! This enables event-driven cache-aware routing as an alternative to the approximate
//! radix tree approach.
//!
//! Lifecycle:
//! - `on_worker_added` — spawns streaming task, creates indexer if needed
//! - `on_worker_removed` — signals graceful shutdown, task cleans up indexer
//! - `stop` — signals shutdown to all tasks, clears state

use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::{DashMap, DashSet};
use futures::FutureExt as _;
use kv_index::{
    compute_content_hash, ApplyError, PositionalIndexer, SequenceHash, StoredBlock, WorkerBlockMap,
};
use smg_grpc_client::common_proto::{
    kv_cache_event, KvBlock, KvBlocksRemoved, KvBlocksStored, KvCacheEvent, KvEventBatch,
};
use tokio::{
    sync::{oneshot, Mutex},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

use crate::{
    observability::metrics::Metrics,
    worker::{ConnectionMode, Worker, UNKNOWN_MODEL_ID},
};

/// Default jump size for new `PositionalIndexer` instances.
const DEFAULT_JUMP_SIZE: usize = 64;

/// Initial reconnection delay after stream failure.
const INITIAL_RECONNECT_DELAY_MS: u64 = 100;

/// Maximum reconnection delay (caps exponential backoff).
const MAX_RECONNECT_DELAY_MS: u64 = 30_000;

/// Aggregation window for per-worker unknown-parent skip summaries (log side only).
///
/// Matches the companion dedup window in comet issue #3716: the generic
/// tracing-layer dedup then sees at most one line per window from this monitor.
const ORPHAN_SKIP_WINDOW: Duration = Duration::from_secs(60);

/// Manages per-worker KV cache event subscriptions.
///
/// Each gRPC worker gets a dedicated tokio task that subscribes to the backend's
/// KV cache event stream and feeds events into a shared `PositionalIndexer`
/// (one per `model_id`). Workers serving the same model share the same indexer.
pub struct KvEventMonitor {
    /// Per-model positional indexers: model_id → shared indexer.
    pub(crate) indexers: DashMap<String, Arc<PositionalIndexer>>,
    /// Per-model block sizes learned from KV events or set via WorkerSpec.
    /// Used by CacheAwarePolicy to chunk request tokens at query time.
    /// Arc-wrapped so subscription tasks can update it from events.
    block_sizes: Arc<DashMap<String, usize>>,
    /// Workers whose current stream has applied at least one contiguous batch.
    event_ready_workers: Arc<DashSet<String>>,
    /// Per-worker seqlock generation. Odd means the index is being mutated;
    /// certified readers accept a claim only across the same even generation.
    event_generations: Arc<DashMap<String, u64>>,
    /// Last successfully applied batch, bounding trust on heartbeatless streams.
    event_last_batch: Arc<DashMap<String, Instant>>,
    /// Per-worker subscription handles: worker_url → subscription info.
    /// Mutex matches LoadMonitor pattern for atomic abort + remove.
    worker_handles: Mutex<HashMap<String, WorkerSubscription>>,
    /// Jump size for new PositionalIndexer instances.
    jump_size: usize,
}

/// Tracks a single worker's subscription state.
struct WorkerSubscription {
    handle: JoinHandle<()>,
    model_id: String,
    /// Signals the subscription task to shut down gracefully.
    /// The task owns its `WorkerBlockMap` and cleans up the indexer on exit.
    shutdown_tx: oneshot::Sender<()>,
}

/// Result of processing a stream connection to completion.
enum StreamResult {
    /// Stream closed normally (server-side).
    Ended,
    /// Stream produced an error.
    Error(String),
}

#[derive(Debug, PartialEq, Eq)]
enum BatchResult {
    Applied,
    EpochReset { previous: u64, received: u64 },
    GapRecovered { expected: u64, received: u64 },
}

/// Cause of the per-worker clear that started the current stream epoch.
///
/// A stream epoch spans per-worker clears; every store event whose parent
/// predates the clear is an orphan skip on a handled path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanSkipCause {
    /// First stream epoch of the worker; no batch has been applied yet.
    ColdStart,
    /// The stream ended or errored and the subscription reconnected.
    Reconnect,
    /// A non-replayable sequence gap forced a stale-state clear.
    SequenceGap,
    /// A non-increasing publisher sequence reset the engine epoch.
    EpochReset,
    /// The engine sent an explicit `Cleared` event.
    ClearedEvent,
}

impl OrphanSkipCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::ColdStart => "cold_start",
            Self::Reconnect => "reconnect",
            Self::SequenceGap => "sequence_gap",
            Self::EpochReset => "epoch_reset",
            Self::ClearedEvent => "cleared_event",
        }
    }
}

/// Stream-epoch classification of unknown-parent skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanSkipPhase {
    /// No successful store applied since the last clear; expected recovery
    /// volume on a handled path (aggregates at INFO).
    Rebuild,
    /// At least one store applied since the last clear; the sustained-drift
    /// signal (aggregates at WARN).
    Steady,
}

impl OrphanSkipPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rebuild => "rebuild",
            Self::Steady => "steady",
        }
    }
}

/// Metric label value for the apply error behind an unknown-parent skip.
fn orphan_skip_error_kind(error: &ApplyError) -> &'static str {
    match error {
        ApplyError::WorkerNotTracked => "worker_not_tracked",
        ApplyError::ParentBlockNotFound => "parent_not_found",
    }
}

/// Per-window summary of unknown-parent skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrphanSkipAggregate {
    phase: OrphanSkipPhase,
    count: u64,
    worker_not_tracked: u64,
    parent_not_found: u64,
}

/// Log decisions for one skipped unknown-parent store event.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct OrphanSkipNotices {
    /// Emit the bounded first-instance WARN for this stream epoch.
    first_instance: bool,
    /// One full 60s window closed on this event; emit its aggregate.
    window_aggregate: Option<OrphanSkipAggregate>,
}

/// Per-worker tracker of unknown-parent store skips for one stream epoch.
///
/// Owned by the worker's subscription task; no shared state. Keeps the log
/// surface bounded on a handled path: the counter carries per-event
/// attribution, DEBUG carries per-event detail, one first-instance WARN per
/// epoch announces each clear epoch, and one aggregate per
/// [`ORPHAN_SKIP_WINDOW`] carries continuing volume.
struct OrphanSkipTracker {
    cause: OrphanSkipCause,
    phase: OrphanSkipPhase,
    first_notice_sent: bool,
    pending: u64,
    pending_worker_not_tracked: u64,
    pending_parent_not_found: u64,
    window_start: Option<Instant>,
}

impl Default for OrphanSkipTracker {
    fn default() -> Self {
        Self {
            cause: OrphanSkipCause::ColdStart,
            phase: OrphanSkipPhase::Rebuild,
            first_notice_sent: false,
            pending: 0,
            pending_worker_not_tracked: 0,
            pending_parent_not_found: 0,
            window_start: None,
        }
    }
}

impl OrphanSkipTracker {
    /// Start a fresh stream epoch after the worker's index state was cleared.
    fn record_clear(&mut self, cause: OrphanSkipCause) {
        *self = Self {
            cause,
            ..Self::default()
        };
    }

    /// Note one successful `apply_stored`.
    ///
    /// The first success after a clear ends rebuild volume: the phase moves
    /// to steady and pending rebuild aggregation resets. The first-instance
    /// notice stays consumed for the rest of the epoch.
    fn record_apply_success(&mut self) {
        if self.phase == OrphanSkipPhase::Rebuild {
            self.phase = OrphanSkipPhase::Steady;
            self.pending = 0;
            self.pending_worker_not_tracked = 0;
            self.pending_parent_not_found = 0;
            self.window_start = None;
        }
    }

    /// Note one skipped unknown-parent store event at `now` (injectable for
    /// deterministic tests) and return the log decisions for it.
    fn record_skip(&mut self, error: &ApplyError, now: Instant) -> OrphanSkipNotices {
        let notices = OrphanSkipNotices {
            first_instance: !self.first_notice_sent,
            window_aggregate: self.advance_window(now),
        };
        self.first_notice_sent = true;
        self.pending += 1;
        match error {
            ApplyError::WorkerNotTracked => self.pending_worker_not_tracked += 1,
            ApplyError::ParentBlockNotFound => self.pending_parent_not_found += 1,
        }
        if self.window_start.is_none() {
            self.window_start = Some(now);
        }
        notices
    }

    /// Close a full window and return its aggregate, resetting its counters.
    fn advance_window(&mut self, now: Instant) -> Option<OrphanSkipAggregate> {
        let start = self.window_start?;
        if now.saturating_duration_since(start) < ORPHAN_SKIP_WINDOW {
            return None;
        }
        let aggregate = OrphanSkipAggregate {
            phase: self.phase,
            count: self.pending,
            worker_not_tracked: self.pending_worker_not_tracked,
            parent_not_found: self.pending_parent_not_found,
        };
        self.pending = 0;
        self.pending_worker_not_tracked = 0;
        self.pending_parent_not_found = 0;
        self.window_start = None;
        Some(aggregate)
    }
}

impl KvEventMonitor {
    /// Create a new `KvEventMonitor`.
    ///
    /// `jump_size` controls the `PositionalIndexer` jump search stride.
    /// Pass `None` for the default (64).
    pub fn new(jump_size: Option<usize>) -> Self {
        let jump_size = jump_size.unwrap_or(DEFAULT_JUMP_SIZE).max(1);
        Self {
            indexers: DashMap::new(),
            block_sizes: Arc::new(DashMap::new()),
            event_ready_workers: Arc::new(DashSet::new()),
            event_generations: Arc::new(DashMap::new()),
            event_last_batch: Arc::new(DashMap::new()),
            worker_handles: Mutex::new(HashMap::new()),
            jump_size,
        }
    }

    /// Start a KV event subscription for a worker.
    ///
    /// Spawns a background tokio task that subscribes to KV cache events via
    /// server-streaming gRPC and applies them to the model's `PositionalIndexer`.
    /// Duplicate calls for the same worker URL are no-ops.
    pub async fn on_worker_added(&self, worker: &Arc<dyn Worker>) {
        let url = worker.url().to_string();
        // Normalize model_id to match routing's normalize_model_key — empty → "unknown".
        let model_id = Self::normalize_model_id(worker.model_id());

        if *worker.connection_mode() == ConnectionMode::Http {
            debug!(worker_url = %url, "HTTP worker, skipping KV event subscription");
            return;
        }

        let mut handles = self.worker_handles.lock().await;
        if handles.contains_key(&url) {
            debug!(worker_url = %url, "KV event subscription already active, skipping");
            return;
        }

        let indexer = self
            .indexers
            .entry(model_id.clone())
            .or_insert_with(|| Arc::new(PositionalIndexer::new(self.jump_size)))
            .clone();

        // Seed block_size provisionally from WorkerSpec. The event stream will
        // overwrite this with the backend's actual page size once received.
        if let Some(bs) = worker.metadata().spec.kv_block_size {
            if bs > 0 {
                self.block_sizes.entry(model_id.clone()).or_insert(bs);
            } else {
                warn!(worker_url = %url, "Worker reports kv_block_size=0, ignoring");
            }
        }

        let worker = Arc::clone(worker);
        let worker_url = url.clone();
        let block_sizes = Arc::clone(&self.block_sizes);
        let event_ready_workers = Arc::clone(&self.event_ready_workers);
        let event_generations = Arc::clone(&self.event_generations);
        let event_last_batch = Arc::clone(&self.event_last_batch);
        Self::begin_generation_mutation(&event_generations, &url);
        event_ready_workers.remove(&url);
        event_last_batch.remove(&url);

        info!(
            worker_url = %url,
            model_id = %model_id,
            "Starting KV event subscription"
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let loop_model_id = model_id.clone();
        let task_url = url.clone();

        #[expect(
            clippy::disallowed_methods,
            reason = "KV event monitor: runs for the lifetime of the worker, \
                      handle is stored and graceful shutdown is sent on removal"
        )]
        let handle = tokio::spawn(async move {
            let worker_id = match indexer.intern_worker(&task_url) {
                Ok(id) => id,
                Err(e) => {
                    error!(
                        worker_url = %task_url,
                        error = %e,
                        "Failed to intern worker; KV events disabled for this worker"
                    );
                    Metrics::record_kv_event_subscription_failure(&task_url, "intern_failed");
                    return;
                }
            };
            // Keep reverse membership outside the caught future so panic
            // cleanup can remove every exact membership from the shared index.
            let mut worker_blocks = WorkerBlockMap::default();
            // Catch panics here so they surface when they happen — a bare
            // JoinError would only be observed at worker removal, leaving the
            // index silently frozen for this worker until then.
            let result = std::panic::AssertUnwindSafe(Self::subscription_loop(
                worker,
                worker_url,
                Arc::clone(&indexer),
                worker_id,
                &mut worker_blocks,
                block_sizes,
                Arc::clone(&event_ready_workers),
                Arc::clone(&event_generations),
                Arc::clone(&event_last_batch),
                loop_model_id,
                shutdown_rx,
            ))
            .catch_unwind()
            .await;
            Self::begin_generation_mutation(&event_generations, &task_url);
            event_ready_workers.remove(&task_url);
            event_last_batch.remove(&task_url);
            indexer.remove_worker(worker_id, worker_blocks);
            if let Err(payload) = result {
                let msg = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .map(String::from)
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "(non-string panic)".into());
                error!(
                    worker_url = %task_url,
                    panic.message = %msg,
                    "KV event subscription task panicked; KV events from this \
                     worker no longer feed cache-aware routing"
                );
                Metrics::record_kv_event_subscription_failure(&task_url, "panic");
            }
        });

        handles.insert(
            url,
            WorkerSubscription {
                handle,
                model_id,
                shutdown_tx,
            },
        );
    }

    /// Stop the KV event subscription for a worker.
    ///
    /// Sends a graceful shutdown signal — the task cleans up its own
    /// `WorkerBlockMap` in the indexer before exiting.
    pub async fn on_worker_removed(&self, worker_url: &str) {
        // Serialize same-URL removal and re-add. The old task must finish
        // clearing its worker ID before a replacement begins publishing.
        let mut handles = self.worker_handles.lock().await;
        let Some(sub) = handles.remove(worker_url) else {
            return;
        };

        info!(worker_url = %worker_url, "Stopping KV event subscription");
        // Signal graceful shutdown — task cleans up its worker_blocks in the indexer.
        let _ = sub.shutdown_tx.send(());
        // Panics are caught inside the task; a JoinError here (abort or a
        // panic that escaped the guard) must still be surfaced, not discarded.
        if let Err(e) = sub.handle.await {
            error!(
                worker_url = %worker_url,
                error = %e,
                "KV event subscription task failed"
            );
            Metrics::record_kv_event_subscription_failure(worker_url, "join_error");
        }

        // Re-check under lock whether this was the last worker for the model.
        // Must re-acquire lock after shutdown to avoid TOCTOU with concurrent
        // on_worker_added that may have added a new worker for the same model
        // between our first lock release and this point.
        let should_remove_indexer = !handles.values().any(|other| other.model_id == sub.model_id);

        if should_remove_indexer {
            self.indexers.remove(&sub.model_id);
            self.block_sizes.remove(&sub.model_id);
        }
        Self::begin_generation_mutation(&self.event_generations, worker_url);
        self.event_ready_workers.remove(worker_url);
        self.event_last_batch.remove(worker_url);
    }

    /// Stop all subscriptions and clean up.
    pub async fn stop(&self) {
        let subscriptions: HashMap<String, WorkerSubscription> = {
            let mut handles = self.worker_handles.lock().await;
            std::mem::take(&mut *handles)
        };

        if !subscriptions.is_empty() {
            info!(
                count = subscriptions.len(),
                "Stopping all KV event subscriptions"
            );
            for (url, sub) in subscriptions {
                debug!(worker_url = %url, "Stopping KV event subscription");
                let _ = sub.shutdown_tx.send(());
                if let Err(e) = sub.handle.await {
                    error!(
                        worker_url = %url,
                        error = %e,
                        "KV event subscription task failed"
                    );
                    Metrics::record_kv_event_subscription_failure(&url, "join_error");
                }
            }
        }

        self.indexers.clear();
        self.block_sizes.clear();
        self.event_ready_workers.clear();
        self.event_generations.clear();
        self.event_last_batch.clear();
    }

    /// Get the indexer for a model (used by `CacheAwarePolicy` for queries).
    pub fn get_indexer(&self, model_id: &str) -> Option<Arc<PositionalIndexer>> {
        self.indexers.get(model_id).map(|r| Arc::clone(&r))
    }

    /// Get the block size for a model (learned from events or set via `set_block_size`).
    pub fn block_size(&self, model_id: &str) -> Option<usize> {
        self.block_sizes.get(model_id).map(|v| *v)
    }

    /// Set the block size for a model (e.g. from WorkerSpec during registration).
    /// Does not overwrite a value already learned from events.
    pub fn set_block_size(&self, model_id: &str, block_size: usize) {
        self.block_sizes
            .entry(model_id.to_string())
            .or_insert(block_size);
    }

    /// Capture the even stream/index generation for a certified query.
    pub fn certified_generation(&self, worker_url: &str, max_age: Duration) -> Option<u64> {
        let generation = self.event_generations.get(worker_url).map(|entry| *entry)?;
        let observed_at = self.event_last_batch.get(worker_url).map(|entry| *entry)?;
        (generation.is_multiple_of(2)
            && self.event_ready_workers.contains(worker_url)
            && Instant::now().saturating_duration_since(observed_at) <= max_age)
            .then_some(generation)
    }

    /// Verify a claim at the selection linearization point.
    pub fn certified_generation_unchanged(
        &self,
        worker_url: &str,
        generation: u64,
        max_age: Duration,
    ) -> bool {
        generation.is_multiple_of(2)
            && self.event_ready_workers.contains(worker_url)
            && self
                .event_last_batch
                .get(worker_url)
                .is_some_and(|observed| {
                    Instant::now().saturating_duration_since(*observed) <= max_age
                })
            && self
                .event_generations
                .get(worker_url)
                .is_some_and(|current| *current == generation)
    }

    #[cfg(test)]
    pub(crate) fn mark_event_stream_ready_for_test(&self, worker_url: &str) {
        Self::begin_generation_mutation(&self.event_generations, worker_url);
        self.event_last_batch
            .insert(worker_url.to_string(), Instant::now());
        self.event_ready_workers.insert(worker_url.to_string());
        Self::finish_generation_mutation(&self.event_generations, worker_url);
    }

    #[cfg(test)]
    pub(crate) fn mark_event_stream_not_ready_for_test(&self, worker_url: &str) {
        Self::begin_generation_mutation(&self.event_generations, worker_url);
        self.event_ready_workers.remove(worker_url);
        self.event_last_batch.remove(worker_url);
    }

    #[cfg(test)]
    pub(crate) fn age_event_stream_for_test(&self, worker_url: &str, age: Duration) {
        self.event_last_batch.insert(
            worker_url.to_string(),
            Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
        );
    }

    /// Check if any subscription is running.
    pub async fn is_running(&self) -> bool {
        !self.worker_handles.lock().await.is_empty()
    }

    /// Normalize model_id to match routing's `normalize_model_key`.
    /// Empty model IDs map to UNKNOWN_MODEL_ID for consistent keying.
    fn normalize_model_id(model_id: &str) -> String {
        if model_id.is_empty() {
            UNKNOWN_MODEL_ID.to_string()
        } else {
            model_id.to_string()
        }
    }

    fn begin_generation_mutation(generations: &DashMap<String, u64>, worker_url: &str) {
        generations
            .entry(worker_url.to_string())
            .and_modify(|generation| {
                if generation.is_multiple_of(2) {
                    *generation = generation.wrapping_add(1);
                }
            })
            .or_insert(1);
    }

    fn finish_generation_mutation(generations: &DashMap<String, u64>, worker_url: &str) {
        generations
            .entry(worker_url.to_string())
            .and_modify(|generation| {
                if !generation.is_multiple_of(2) {
                    *generation = generation.wrapping_add(1);
                }
            })
            .or_insert(2);
    }

    // -----------------------------------------------------------------------
    // Subscription loop
    // -----------------------------------------------------------------------

    /// Learn `block_size` from the first `KvBlock` in a stored event.
    ///
    /// Called once per model when the first stored event arrives, providing
    /// ground truth from the backend. `CacheAwarePolicy` uses this to chunk
    /// request tokens into blocks for overlap scoring.
    ///
    /// Overwrites any provisional value seeded from `WorkerSpec` since the
    /// event stream reflects the backend's actual page size.
    fn learn_block_size(
        block_sizes: &DashMap<String, usize>,
        model_id: &str,
        learned: &mut bool,
        batch: &KvEventBatch,
    ) {
        if *learned {
            return;
        }
        for event in &batch.events {
            if let Some(kv_cache_event::Data::Stored(stored)) = &event.data {
                if let Some(block) = stored.blocks.first() {
                    if block.block_size > 0 {
                        let bs = block.block_size as usize;
                        block_sizes.insert(model_id.to_string(), bs);
                        info!(
                            model_id = %model_id,
                            block_size = bs,
                            "Learned block_size from KV event"
                        );
                        *learned = true;
                        return;
                    }
                }
            }
        }
    }

    /// Main subscription loop for a single worker.
    ///
    /// Owns the `WorkerBlockMap` for this worker and cleans it up on exit.
    /// Exits when `shutdown_rx` fires or the backend returns `Unimplemented`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the live stream task owns explicit per-worker certification state"
    )]
    async fn subscription_loop(
        worker: Arc<dyn Worker>,
        worker_url: String,
        indexer: Arc<PositionalIndexer>,
        worker_id: u32,
        worker_blocks: &mut WorkerBlockMap,
        block_sizes: Arc<DashMap<String, usize>>,
        event_ready_workers: Arc<DashSet<String>>,
        event_generations: Arc<DashMap<String, u64>>,
        event_last_batch: Arc<DashMap<String, Instant>>,
        model_id: String,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) {
        let mut last_seq: u64 = 0;
        let mut seen_batch = false;
        let mut reconnect_delay_ms = INITIAL_RECONNECT_DELAY_MS;
        let mut block_size_learned = false;
        // Per-worker unknown-parent skip tracker, owned by this task. Starts
        // in the cold-start rebuild epoch of the worker's first stream.
        let mut orphan_skips = OrphanSkipTracker::default();

        /// Sleep with shutdown check. Returns `true` if shutdown was signaled.
        macro_rules! sleep_or_shutdown {
            ($delay:expr, $rx:expr) => {
                tokio::select! {
                    _ = tokio::time::sleep($delay) => false,
                    _ = &mut *$rx => true,
                }
            };
        }

        macro_rules! invalidate_and_clear {
            () => {{
                Self::begin_generation_mutation(&event_generations, &worker_url);
                event_ready_workers.remove(&worker_url);
                event_last_batch.remove(&worker_url);
                indexer.apply_cleared(worker_id, worker_blocks);
                return;
            }};
        }

        loop {
            let grpc_client = match worker.get_grpc_client().await {
                Ok(Some(client)) => client,
                Ok(None) => {
                    // HTTP workers are filtered in on_worker_added, so this should
                    // be unreachable. Retry defensively rather than exiting and
                    // leaving a stale entry in worker_handles.
                    warn!(
                        worker_url = %worker_url,
                        delay_ms = reconnect_delay_ms,
                        "Worker has no gRPC client yet, retrying"
                    );
                    if sleep_or_shutdown!(
                        Duration::from_millis(reconnect_delay_ms),
                        &mut shutdown_rx
                    ) {
                        invalidate_and_clear!();
                    }
                    reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
                    continue;
                }
                Err(e) => {
                    warn!(
                        worker_url = %worker_url,
                        error = %e,
                        delay_ms = reconnect_delay_ms,
                        "Failed to get gRPC client, retrying"
                    );
                    if sleep_or_shutdown!(
                        Duration::from_millis(reconnect_delay_ms),
                        &mut shutdown_rx
                    ) {
                        invalidate_and_clear!();
                    }
                    reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
                    continue;
                }
            };

            let stream = match grpc_client.subscribe_kv_events(last_seq).await {
                Ok(stream) => {
                    info!(
                        worker_url = %worker_url,
                        start_seq = last_seq,
                        "KV event stream connected"
                    );
                    reconnect_delay_ms = INITIAL_RECONNECT_DELAY_MS;
                    stream
                }
                Err(e) => {
                    // If the backend doesn't implement SubscribeKvEvents (e.g. vLLM),
                    // stop retrying — this RPC will never succeed.
                    if e.code() == tonic::Code::Unimplemented {
                        warn!(
                            worker_url = %worker_url,
                            "Backend does not implement SubscribeKvEvents, \
                             disabling KV event subscription for this worker"
                        );
                        invalidate_and_clear!();
                    }
                    warn!(
                        worker_url = %worker_url,
                        error = %e,
                        delay_ms = reconnect_delay_ms,
                        "Failed to subscribe to KV events, retrying"
                    );
                    if sleep_or_shutdown!(
                        Duration::from_millis(reconnect_delay_ms),
                        &mut shutdown_rx
                    ) {
                        invalidate_and_clear!();
                    }
                    reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
                    continue;
                }
            };

            let on_batch = |batch: &KvEventBatch| {
                Self::learn_block_size(&block_sizes, &model_id, &mut block_size_learned, batch);
            };
            let stream_result = tokio::select! {
                result = Self::process_stream(
                    stream, &worker_url, worker_id, &indexer,
                    worker_blocks, &mut last_seq, &mut seen_batch,
                    &event_ready_workers, &event_generations, &event_last_batch,
                    &mut orphan_skips, on_batch,
                ) => result,
                _ = &mut shutdown_rx => {
                    invalidate_and_clear!();
                }
            };
            // Production KV subscriptions are live-only. A reconnect cannot
            // prove that the engine process or publisher epoch stayed the
            // same, so discard every old positive before subscribing again.
            Self::begin_generation_mutation(&event_generations, &worker_url);
            event_ready_workers.remove(&worker_url);
            event_last_batch.remove(&worker_url);
            Self::reset_stream_epoch(
                &indexer,
                worker_id,
                worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
            );

            match stream_result {
                StreamResult::Ended => {
                    info!(
                        worker_url = %worker_url,
                        last_seq = last_seq,
                        delay_ms = reconnect_delay_ms,
                        "KV event stream ended, reconnecting"
                    );
                    // Backoff to avoid tight reconnect loop if server keeps
                    // closing the stream cleanly (e.g., rolling connections).
                    if sleep_or_shutdown!(
                        Duration::from_millis(reconnect_delay_ms),
                        &mut shutdown_rx
                    ) {
                        invalidate_and_clear!();
                    }
                    reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
                }
                StreamResult::Error(e) => {
                    warn!(
                        worker_url = %worker_url,
                        error = %e,
                        last_seq = last_seq,
                        delay_ms = reconnect_delay_ms,
                        "KV event stream error, reconnecting"
                    );
                    if sleep_or_shutdown!(
                        Duration::from_millis(reconnect_delay_ms),
                        &mut shutdown_rx
                    ) {
                        invalidate_and_clear!();
                    }
                    reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Stream processing + proto conversion
    // -----------------------------------------------------------------------

    /// Process batches from a single stream connection.
    #[expect(
        clippy::too_many_arguments,
        reason = "the stream processor updates one explicit worker epoch atomically"
    )]
    async fn process_stream(
        mut stream: tonic::Streaming<KvEventBatch>,
        worker_url: &str,
        worker_id: u32,
        indexer: &PositionalIndexer,
        worker_blocks: &mut WorkerBlockMap,
        last_seq: &mut u64,
        seen_batch: &mut bool,
        event_ready_workers: &DashSet<String>,
        event_generations: &DashMap<String, u64>,
        event_last_batch: &DashMap<String, Instant>,
        orphan_skips: &mut OrphanSkipTracker,
        mut on_batch: impl FnMut(&KvEventBatch),
    ) -> StreamResult {
        use tokio_stream::StreamExt;

        while let Some(result) = stream.next().await {
            let batch = match result {
                Ok(batch) => batch,
                Err(e) => return StreamResult::Error(e.to_string()),
            };

            Self::begin_generation_mutation(event_generations, worker_url);
            Self::process_batch(
                &batch,
                worker_url,
                worker_id,
                indexer,
                worker_blocks,
                last_seq,
                seen_batch,
                orphan_skips,
                &mut on_batch,
            );
            event_last_batch.insert(worker_url.to_string(), Instant::now());
            event_ready_workers.insert(worker_url.to_string());
            Self::finish_generation_mutation(event_generations, worker_url);
        }

        StreamResult::Ended
    }

    /// Apply one KV event batch, recovering live-only streams after a sequence gap.
    #[expect(
        clippy::too_many_arguments,
        reason = "batch recovery mutates one explicit worker epoch and its test callback"
    )]
    fn process_batch(
        batch: &KvEventBatch,
        worker_url: &str,
        worker_id: u32,
        indexer: &PositionalIndexer,
        worker_blocks: &mut WorkerBlockMap,
        last_seq: &mut u64,
        seen_batch: &mut bool,
        orphan_skips: &mut OrphanSkipTracker,
        on_batch: &mut impl FnMut(&KvEventBatch),
    ) -> BatchResult {
        // The production stream is live-only. A non-increasing sequence on an
        // otherwise-connected bridge is an engine publisher epoch reset, not
        // replay. Clear first so equal/lower restart collisions cannot retain
        // stale positives.
        if *seen_batch && batch.sequence_number <= *last_seq {
            let previous = *last_seq;
            warn!(
                worker_url = %worker_url,
                last_seq = *last_seq,
                received = batch.sequence_number,
                "KV publisher sequence reset; cleared stale cache state"
            );
            indexer.apply_cleared(worker_id, worker_blocks);
            orphan_skips.record_clear(OrphanSkipCause::EpochReset);
            for event in &batch.events {
                Self::apply_event(
                    event,
                    worker_url,
                    worker_id,
                    indexer,
                    worker_blocks,
                    orphan_skips,
                );
            }
            on_batch(batch);
            *last_seq = batch.sequence_number;
            *seen_batch = true;
            return BatchResult::EpochReset {
                previous,
                received: batch.sequence_number,
            };
        }

        // Some engine bridges expose a live-only event stream even though the
        // gRPC request carries a replay cursor. Reconnecting such a stream after
        // one dropped event starts at the current publisher position, so retrying
        // the old cursor can never fill the gap. Clear this worker's stale view
        // and resume from the first live batch instead. The approximate token tree
        // remains available while new event-driven ownership is learned.
        let expected = last_seq.saturating_add(1);
        let gap = (*seen_batch && batch.sequence_number > expected)
            .then_some((expected, batch.sequence_number));
        if let Some((expected, received)) = gap {
            warn!(
                worker_url = %worker_url,
                expected = expected,
                received = received,
                "Sequence gap detected; cleared stale cache state and resumed live stream"
            );
            Metrics::record_kv_event_sequence_gap_recovery(worker_url);
            indexer.apply_cleared(worker_id, worker_blocks);
            orphan_skips.record_clear(OrphanSkipCause::SequenceGap);
        }

        for event in &batch.events {
            Self::apply_event(
                event,
                worker_url,
                worker_id,
                indexer,
                worker_blocks,
                orphan_skips,
            );
        }
        on_batch(batch);
        *last_seq = batch.sequence_number;
        *seen_batch = true;

        match gap {
            Some((expected, received)) => BatchResult::GapRecovered { expected, received },
            None => BatchResult::Applied,
        }
    }

    fn reset_stream_epoch(
        indexer: &PositionalIndexer,
        worker_id: u32,
        worker_blocks: &mut WorkerBlockMap,
        last_seq: &mut u64,
        seen_batch: &mut bool,
        orphan_skips: &mut OrphanSkipTracker,
    ) {
        indexer.apply_cleared(worker_id, worker_blocks);
        orphan_skips.record_clear(OrphanSkipCause::Reconnect);
        *last_seq = 0;
        *seen_batch = false;
    }

    /// Apply a single KV cache event to the indexer.
    fn apply_event(
        event: &KvCacheEvent,
        worker_url: &str,
        worker_id: u32,
        indexer: &PositionalIndexer,
        worker_blocks: &mut WorkerBlockMap,
        orphan_skips: &mut OrphanSkipTracker,
    ) {
        let Some(ref data) = event.data else {
            return;
        };

        match data {
            kv_cache_event::Data::Stored(stored) => {
                Self::apply_stored(
                    stored,
                    worker_url,
                    worker_id,
                    indexer,
                    worker_blocks,
                    orphan_skips,
                );
            }
            kv_cache_event::Data::Removed(removed) => {
                Self::apply_removed(removed, worker_id, indexer, worker_blocks);
            }
            kv_cache_event::Data::Cleared(_) => {
                indexer.apply_cleared(worker_id, worker_blocks);
                orphan_skips.record_clear(OrphanSkipCause::ClearedEvent);
            }
        }
    }

    /// Convert proto `KvBlocksStored` and apply to the indexer.
    fn apply_stored(
        stored: &KvBlocksStored,
        worker_url: &str,
        worker_id: u32,
        indexer: &PositionalIndexer,
        worker_blocks: &mut WorkerBlockMap,
        orphan_skips: &mut OrphanSkipTracker,
    ) {
        let blocks: Vec<StoredBlock> = stored.blocks.iter().map(convert_kv_block).collect();

        let parent_seq_hash = stored.parent_block_hash.map(SequenceHash::from);

        match indexer.apply_stored(worker_id, &blocks, parent_seq_hash, worker_blocks) {
            Ok(()) => orphan_skips.record_apply_success(),
            Err(error @ (ApplyError::WorkerNotTracked | ApplyError::ParentBlockNotFound)) => {
                // Skipping is the designed safe behavior on a handled path:
                // re-rooting an orphan from a live-only cold start would put a
                // child at position zero and manufacture a false prefix hit.
                // The counter carries per-event attribution and DEBUG carries
                // per-event detail; the log surface stays bounded to one
                // first-instance WARN per stream epoch plus one aggregate per
                // 60s window per worker (INFO while the epoch rebuilds, WARN
                // on steady drift — the only sustained WARNING from this path).
                Metrics::record_kv_event_unknown_parent_skip(
                    worker_url,
                    orphan_skips.phase.as_str(),
                    orphan_skip_error_kind(&error),
                );
                let notices = orphan_skips.record_skip(&error, Instant::now());
                debug!(
                    worker_url = %worker_url,
                    worker_id = worker_id,
                    parent_block_hash = ?stored.parent_block_hash,
                    error = orphan_skip_error_kind(&error),
                    phase = orphan_skips.phase.as_str(),
                    "Skipped KV store event with unknown parent"
                );
                if notices.first_instance {
                    warn!(
                        worker_url = %worker_url,
                        worker_id = worker_id,
                        parent_block_hash = ?stored.parent_block_hash,
                        error = orphan_skip_error_kind(&error),
                        "Skipped KV store event with unknown parent; \
                         first in this stream epoch since {}",
                        orphan_skips.cause.as_str()
                    );
                }
                if let Some(aggregate) = notices.window_aggregate {
                    match aggregate.phase {
                        OrphanSkipPhase::Rebuild => info!(
                            worker_url = %worker_url,
                            cause = orphan_skips.cause.as_str(),
                            skipped = aggregate.count,
                            worker_not_tracked = aggregate.worker_not_tracked,
                            parent_not_found = aggregate.parent_not_found,
                            "Skipped {} orphan KV store events with unknown parents \
                             in 60s window; stream epoch rebuilding",
                            aggregate.count
                        ),
                        OrphanSkipPhase::Steady => warn!(
                            worker_url = %worker_url,
                            cause = orphan_skips.cause.as_str(),
                            skipped = aggregate.count,
                            worker_not_tracked = aggregate.worker_not_tracked,
                            parent_not_found = aggregate.parent_not_found,
                            "Skipped {} orphan KV store events with unknown parents \
                             in 60s window; steady-phase unknown-parent drift",
                            aggregate.count
                        ),
                    }
                }
            }
        }
    }

    /// Convert proto `KvBlocksRemoved` and apply to the indexer.
    fn apply_removed(
        removed: &KvBlocksRemoved,
        worker_id: u32,
        indexer: &PositionalIndexer,
        worker_blocks: &mut WorkerBlockMap,
    ) {
        let seq_hashes: Vec<SequenceHash> = removed
            .block_hashes
            .iter()
            .map(|&h| SequenceHash::from(h))
            .collect();

        indexer.apply_removed(worker_id, &seq_hashes, worker_blocks);
    }
}

/// Convert a proto `KvBlock` to a kv-index `StoredBlock`.
fn convert_kv_block(block: &KvBlock) -> StoredBlock {
    StoredBlock {
        seq_hash: SequenceHash::from(block.block_hash),
        content_hash: compute_content_hash(&block.token_ids),
    }
}

impl Drop for KvEventMonitor {
    fn drop(&mut self) {
        if let Ok(mut handles) = self.worker_handles.try_lock() {
            for (_, sub) in handles.drain() {
                let _ = sub.shutdown_tx.send(());
                sub.handle.abort(); // Can't await in Drop, abort as fallback
            }
        }
    }
}

impl fmt::Debug for KvEventMonitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvEventMonitor")
            .field("models", &self.indexers.len())
            .field("block_sizes", &self.block_sizes.len())
            .field("jump_size", &self.jump_size)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    // -----------------------------------------------------------------------
    // Proto → kv-index conversion
    // -----------------------------------------------------------------------

    #[test]
    fn test_convert_kv_block() {
        let block = KvBlock {
            block_hash: 42,
            token_ids: vec![1, 2, 3, 4],
            block_size: 4,
            lora_id: None,
            cache_level: None,
        };
        let stored = convert_kv_block(&block);
        assert_eq!(stored.seq_hash, SequenceHash::from(42i64));
        assert_eq!(stored.content_hash, compute_content_hash(&[1, 2, 3, 4]));
    }

    #[test]
    fn test_convert_kv_block_negative_hash() {
        let block = KvBlock {
            block_hash: -1,
            token_ids: vec![10, 20],
            block_size: 2,
            lora_id: None,
            cache_level: None,
        };
        let stored = convert_kv_block(&block);
        assert_eq!(stored.seq_hash, SequenceHash(u64::MAX));
    }

    #[test]
    fn test_convert_kv_block_empty_tokens() {
        let block = KvBlock {
            block_hash: 100,
            token_ids: vec![],
            block_size: 0,
            lora_id: None,
            cache_level: None,
        };
        let stored = convert_kv_block(&block);
        assert_eq!(stored.seq_hash, SequenceHash::from(100i64));
        assert_eq!(stored.content_hash, compute_content_hash(&[]));
    }

    // -----------------------------------------------------------------------
    // apply_event integration with PositionalIndexer
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_stored_no_parent() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();
        let stored = KvBlocksStored {
            blocks: vec![
                KvBlock {
                    block_hash: 1,
                    token_ids: vec![10, 20, 30, 40],
                    block_size: 4,
                    lora_id: None,
                    cache_level: None,
                },
                KvBlock {
                    block_hash: 2,
                    token_ids: vec![50, 60, 70, 80],
                    block_size: 4,
                    lora_id: None,
                    cache_level: None,
                },
            ],
            parent_block_hash: None,
        };

        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_stored(
            &stored,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 2);
    }

    #[test]
    fn test_apply_stored_with_parent() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();

        let stored1 = KvBlocksStored {
            blocks: vec![KvBlock {
                block_hash: 1,
                token_ids: vec![10, 20, 30, 40],
                block_size: 4,
                lora_id: None,
                cache_level: None,
            }],
            parent_block_hash: None,
        };
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_stored(
            &stored1,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );

        let stored2 = KvBlocksStored {
            blocks: vec![KvBlock {
                block_hash: 2,
                token_ids: vec![50, 60, 70, 80],
                block_size: 4,
                lora_id: None,
                cache_level: None,
            }],
            parent_block_hash: Some(1),
        };
        KvEventMonitor::apply_stored(
            &stored2,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 2);
    }

    #[test]
    fn test_apply_stored_unknown_parent_never_becomes_false_root() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://new-worker:8000").unwrap();
        let mut wb = WorkerBlockMap::default();

        // A live-only stream may start with a child whose parent predates this
        // gateway. It must not be re-rooted at position zero.
        let worker_url = "http://new-worker:8000";
        let stored = KvBlocksStored {
            blocks: vec![KvBlock {
                block_hash: 1,
                token_ids: vec![10, 20, 30, 40],
                block_size: 4,
                lora_id: None,
                cache_level: None,
            }],
            parent_block_hash: Some(999),
        };
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_stored(
            &stored,
            worker_url,
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 0);
        assert!(wb.is_empty());
    }

    // -----------------------------------------------------------------------
    // Orphan-skip tracker: bounded notices per epoch and per 60s window
    // -----------------------------------------------------------------------

    #[test]
    fn test_orphan_skip_tracker_first_instance_once_per_epoch() {
        let t0 = Instant::now();
        let mut tracker = OrphanSkipTracker::default();

        let first = tracker.record_skip(&ApplyError::ParentBlockNotFound, t0);
        assert!(first.first_instance);
        assert!(first.window_aggregate.is_none());

        let second =
            tracker.record_skip(&ApplyError::WorkerNotTracked, t0 + Duration::from_secs(1));
        assert!(!second.first_instance);
        assert!(second.window_aggregate.is_none());
        assert_eq!(tracker.cause, OrphanSkipCause::ColdStart);
        assert_eq!(tracker.phase, OrphanSkipPhase::Rebuild);
    }

    #[test]
    fn test_orphan_skip_tracker_window_rollover_emits_exactly_one_aggregate() {
        let t0 = Instant::now();
        let mut tracker = OrphanSkipTracker::default();

        tracker.record_skip(&ApplyError::ParentBlockNotFound, t0);
        tracker.record_skip(
            &ApplyError::ParentBlockNotFound,
            t0 + Duration::from_secs(30),
        );

        let step = ORPHAN_SKIP_WINDOW + Duration::from_secs(1);

        let closed = tracker.record_skip(&ApplyError::WorkerNotTracked, t0 + step);
        let aggregate = closed
            .window_aggregate
            .expect("a full 60s window closes exactly once on rollover");
        assert!(!closed.first_instance);
        assert_eq!(aggregate.phase, OrphanSkipPhase::Rebuild);
        assert_eq!(aggregate.count, 2);
        assert_eq!(aggregate.parent_not_found, 2);
        assert_eq!(aggregate.worker_not_tracked, 0);

        let next = tracker.record_skip(&ApplyError::WorkerNotTracked, t0 + 2 * step);
        let aggregate = next
            .window_aggregate
            .expect("the second window closes after the rollover skip was re-windowed");
        assert_eq!(aggregate.count, 1);
        assert_eq!(aggregate.worker_not_tracked, 1);
    }

    #[test]
    fn test_orphan_skip_tracker_rebuild_to_steady_on_first_success() {
        let t0 = Instant::now();
        let mut tracker = OrphanSkipTracker::default();

        tracker.record_skip(&ApplyError::ParentBlockNotFound, t0);
        tracker.record_apply_success();
        assert_eq!(tracker.phase, OrphanSkipPhase::Steady);

        let steady_skip =
            tracker.record_skip(&ApplyError::WorkerNotTracked, t0 + Duration::from_secs(1));
        // The first-instance notice stays consumed for the rest of this epoch,
        // and the success reset pending rebuild aggregation.
        assert!(!steady_skip.first_instance);
        assert!(steady_skip.window_aggregate.is_none());

        let closed = tracker.record_skip(
            &ApplyError::WorkerNotTracked,
            t0 + Duration::from_secs(1) + ORPHAN_SKIP_WINDOW + Duration::from_secs(1),
        );
        let aggregate = closed
            .window_aggregate
            .expect("the steady window closes after a full 60s of drift");
        assert_eq!(aggregate.phase, OrphanSkipPhase::Steady);
        assert_eq!(aggregate.count, 1);
        assert_eq!(aggregate.worker_not_tracked, 1);
    }

    #[test]
    fn test_orphan_skip_tracker_reset_on_clear_with_new_cause() {
        let t0 = Instant::now();
        let mut tracker = OrphanSkipTracker::default();

        tracker.record_skip(&ApplyError::ParentBlockNotFound, t0);
        tracker.record_clear(OrphanSkipCause::SequenceGap);

        let new_epoch =
            tracker.record_skip(&ApplyError::WorkerNotTracked, t0 + Duration::from_secs(1));
        assert!(new_epoch.first_instance);
        assert!(new_epoch.window_aggregate.is_none());
        assert_eq!(tracker.cause, OrphanSkipCause::SequenceGap);
        assert_eq!(tracker.phase, OrphanSkipPhase::Rebuild);

        let closed = tracker.record_skip(
            &ApplyError::WorkerNotTracked,
            t0 + Duration::from_secs(1) + ORPHAN_SKIP_WINDOW + Duration::from_secs(1),
        );
        let aggregate = closed
            .window_aggregate
            .expect("the cleared window closes with only new-epoch skips");
        assert_eq!(aggregate.count, 1);
        assert_eq!(aggregate.worker_not_tracked, 1);
        assert_eq!(aggregate.parent_not_found, 0);
    }

    #[test]
    fn test_orphan_skip_tracker_clear_without_skips_emits_nothing() {
        let t0 = Instant::now();
        let mut tracker = OrphanSkipTracker::default();

        tracker.record_skip(&ApplyError::ParentBlockNotFound, t0);
        tracker.record_clear(OrphanSkipCause::ClearedEvent);
        tracker.record_apply_success();

        let lone_steady = tracker.record_skip(
            &ApplyError::WorkerNotTracked,
            t0 + ORPHAN_SKIP_WINDOW + Duration::from_secs(1),
        );
        // No stale aggregate survived the clear; the new epoch's first skip
        // only consumes its first-instance notice and opens its own window.
        assert!(lone_steady.first_instance);
        assert!(lone_steady.window_aggregate.is_none());
    }

    // -----------------------------------------------------------------------
    // Orphan-skip integration via process_batch: metrics, phase routing, and
    // the false-root invariant across gap recovery
    // -----------------------------------------------------------------------

    fn stored_event(event_id: u64, block_hash: i64, parent: Option<i64>) -> KvCacheEvent {
        KvCacheEvent {
            event_id,
            data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                blocks: vec![KvBlock {
                    block_hash,
                    token_ids: vec![1, 2, 3, 4],
                    block_size: 4,
                    lora_id: None,
                    cache_level: None,
                }],
                parent_block_hash: parent,
            })),
        }
    }

    fn event_batch(sequence_number: u64, events: Vec<KvCacheEvent>) -> KvEventBatch {
        KvEventBatch {
            sequence_number,
            timestamp: 0.0,
            events,
            dp_rank: None,
        }
    }

    #[test]
    fn test_process_batch_gap_orphans_then_root_store_ends_rebuild() {
        let indexer = PositionalIndexer::new(64);
        let worker_url = "http://w1:8000";
        let worker_id = indexer.intern_worker(worker_url).unwrap();
        let mut worker_blocks = WorkerBlockMap::default();
        let mut orphan_skips = OrphanSkipTracker::default();
        let mut last_seq = 0;
        let mut seen_batch = false;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            // Batch 1 establishes one root (cold-start success → steady).
            let applied = KvEventMonitor::process_batch(
                &event_batch(1, vec![stored_event(1, 1, None)]),
                worker_url,
                worker_id,
                &indexer,
                &mut worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
                &mut |_| {},
            );
            assert_eq!(applied, BatchResult::Applied);
            assert_eq!(orphan_skips.phase, OrphanSkipPhase::Steady);

            // A sequence gap clears the worker; two parented stores in the
            // recovery batch are orphan skips in the rebuild phase. The
            // first-instance notice is consumed exactly once.
            let recovered = KvEventMonitor::process_batch(
                &event_batch(
                    4,
                    vec![stored_event(2, 2, Some(999)), stored_event(3, 3, Some(999))],
                ),
                worker_url,
                worker_id,
                &indexer,
                &mut worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
                &mut |_| {},
            );
            assert_eq!(
                recovered,
                BatchResult::GapRecovered {
                    expected: 2,
                    received: 4
                }
            );
            assert_eq!(orphan_skips.cause, OrphanSkipCause::SequenceGap);
            assert_eq!(orphan_skips.phase, OrphanSkipPhase::Rebuild);
            assert!(orphan_skips.first_notice_sent);
            assert_eq!(orphan_skips.pending, 2);

            // A root store re-establishes tracking and ends rebuild volume.
            let applied = KvEventMonitor::process_batch(
                &event_batch(5, vec![stored_event(4, 4, None)]),
                worker_url,
                worker_id,
                &indexer,
                &mut worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
                &mut |_| {},
            );
            assert_eq!(applied, BatchResult::Applied);
            assert_eq!(orphan_skips.phase, OrphanSkipPhase::Steady);
            assert_eq!(orphan_skips.pending, 0);

            // Orphan skips inside one short window produce no aggregate and
            // no second first-instance notice; only the steady counter moves.
            let applied = KvEventMonitor::process_batch(
                &event_batch(6, vec![stored_event(5, 5, Some(999))]),
                worker_url,
                worker_id,
                &indexer,
                &mut worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
                &mut |_| {},
            );
            assert_eq!(applied, BatchResult::Applied);
            assert_eq!(orphan_skips.pending, 1);

            // The false-root invariant holds across the whole flow: orphan
            // stores never manufactured index entries.
            assert_eq!(indexer.current_size(), 1);
            assert!(worker_blocks.contains_key(&SequenceHash::from(4i64)));
            assert!(!worker_blocks.contains_key(&SequenceHash::from(2i64)));
            assert!(!worker_blocks.contains_key(&SequenceHash::from(3i64)));
            assert!(!worker_blocks.contains_key(&SequenceHash::from(5i64)));
        });

        let rendered = handle.render();
        let counter_line = |phase: &str, error: &str, count: &str| {
            rendered.lines().any(|line| {
                line.starts_with("smg_kv_event_unknown_parent_skips_total{")
                    && line.contains(&format!(r#"phase="{phase}""#))
                    && line.contains(&format!(r#"error="{error}""#))
                    && line.contains(&format!(r#"worker="{worker_url}""#))
                    && line.ends_with(&format!("}} {count}"))
            })
        };
        assert!(
            counter_line("rebuild", "worker_not_tracked", "2"),
            "two rebuild-phase orphan skips missing from the counter; rendered:\n{rendered}"
        );
        assert!(
            counter_line("steady", "parent_not_found", "1"),
            "one steady-phase orphan skip missing from the counter; rendered:\n{rendered}"
        );
        assert!(
            rendered.lines().any(|line| {
                line.starts_with("smg_kv_event_sequence_gap_recoveries_total{")
                    && line.ends_with("} 1")
            }),
            "the gap recovery is the expected co-signal for attribution; rendered:\n{rendered}"
        );
    }

    #[test]
    fn test_apply_removed() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();

        let stored = KvBlocksStored {
            blocks: vec![
                KvBlock {
                    block_hash: 1,
                    token_ids: vec![10, 20, 30, 40],
                    block_size: 4,
                    lora_id: None,
                    cache_level: None,
                },
                KvBlock {
                    block_hash: 2,
                    token_ids: vec![50, 60, 70, 80],
                    block_size: 4,
                    lora_id: None,
                    cache_level: None,
                },
            ],
            parent_block_hash: None,
        };
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_stored(
            &stored,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );

        let removed = KvBlocksRemoved {
            block_hashes: vec![2],
            cache_level: None,
        };
        KvEventMonitor::apply_removed(&removed, w1, &indexer, &mut wb);
        assert_eq!(indexer.current_size(), 1);
    }

    #[test]
    fn test_apply_cleared_event() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();

        let stored = KvBlocksStored {
            blocks: vec![KvBlock {
                block_hash: 1,
                token_ids: vec![10, 20, 30, 40],
                block_size: 4,
                lora_id: None,
                cache_level: None,
            }],
            parent_block_hash: None,
        };
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_stored(
            &stored,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 1);

        indexer.apply_cleared(w1, &mut wb);
        assert_eq!(indexer.current_size(), 0);
    }

    #[test]
    fn test_sequence_gap_clears_stale_worker_state_before_resuming() {
        let indexer = PositionalIndexer::new(64);
        let worker_url = "http://w1:8000";
        let worker_id = indexer.intern_worker(worker_url).unwrap();
        let mut worker_blocks = WorkerBlockMap::default();
        let mut last_seq = 1;
        let mut seen_batch = true;

        let stale = KvBlocksStored {
            blocks: vec![KvBlock {
                block_hash: 1,
                token_ids: vec![10, 20, 30, 40],
                block_size: 4,
                lora_id: None,
                cache_level: None,
            }],
            parent_block_hash: None,
        };
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_stored(
            &stale,
            worker_url,
            worker_id,
            &indexer,
            &mut worker_blocks,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 1);

        let live = KvEventBatch {
            sequence_number: 3,
            timestamp: 0.0,
            events: vec![KvCacheEvent {
                event_id: 2,
                data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                    blocks: vec![KvBlock {
                        block_hash: 2,
                        token_ids: vec![50, 60, 70, 80],
                        block_size: 4,
                        lora_id: None,
                        cache_level: None,
                    }],
                    parent_block_hash: Some(999),
                })),
            }],
            dp_rank: None,
        };

        let result = KvEventMonitor::process_batch(
            &live,
            worker_url,
            worker_id,
            &indexer,
            &mut worker_blocks,
            &mut last_seq,
            &mut seen_batch,
            &mut orphan_skips,
            &mut |_| {},
        );

        assert_eq!(
            result,
            BatchResult::GapRecovered {
                expected: 2,
                received: 3
            }
        );
        assert_eq!(last_seq, 3);
        assert_eq!(indexer.current_size(), 0);
        assert!(!worker_blocks.contains_key(&SequenceHash::from(1i64)));
        assert!(!worker_blocks.contains_key(&SequenceHash::from(2i64)));

        let contiguous = KvEventBatch {
            sequence_number: 4,
            timestamp: 0.0,
            events: vec![KvCacheEvent {
                event_id: 3,
                data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                    blocks: vec![KvBlock {
                        block_hash: 3,
                        token_ids: vec![90, 100, 110, 120],
                        block_size: 4,
                        lora_id: None,
                        cache_level: None,
                    }],
                    parent_block_hash: None,
                })),
            }],
            dp_rank: None,
        };
        let result = KvEventMonitor::process_batch(
            &contiguous,
            worker_url,
            worker_id,
            &indexer,
            &mut worker_blocks,
            &mut last_seq,
            &mut seen_batch,
            &mut orphan_skips,
            &mut |_| {},
        );

        assert_eq!(result, BatchResult::Applied);
        assert_eq!(last_seq, 4);
        assert_eq!(indexer.current_size(), 1);
        assert!(worker_blocks.contains_key(&SequenceHash::from(3i64)));
    }

    #[test]
    fn sequence_zero_then_gap_clears_old_positive() {
        let indexer = PositionalIndexer::new(64);
        let worker_url = "http://w1:8000";
        let worker_id = indexer.intern_worker(worker_url).unwrap();
        let mut worker_blocks = WorkerBlockMap::default();
        let mut last_seq = 0;
        let mut seen_batch = false;
        let mut orphan_skips = OrphanSkipTracker::default();
        let root = KvEventBatch {
            sequence_number: 0,
            timestamp: 0.0,
            events: vec![KvCacheEvent {
                event_id: 1,
                data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                    blocks: vec![KvBlock {
                        block_hash: 1,
                        token_ids: vec![10, 20, 30, 40],
                        block_size: 4,
                        lora_id: None,
                        cache_level: None,
                    }],
                    parent_block_hash: None,
                })),
            }],
            dp_rank: None,
        };
        assert_eq!(
            KvEventMonitor::process_batch(
                &root,
                worker_url,
                worker_id,
                &indexer,
                &mut worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
                &mut |_| {},
            ),
            BatchResult::Applied
        );
        assert!(seen_batch);
        assert_eq!(last_seq, 0);
        assert_eq!(indexer.current_size(), 1);

        // Missing sequence 1 may have removed that root. Sequence 2 must
        // clear it even though the last observed value was the valid zero.
        let jumped = KvEventBatch {
            sequence_number: 2,
            timestamp: 0.0,
            events: Vec::new(),
            dp_rank: None,
        };
        assert_eq!(
            KvEventMonitor::process_batch(
                &jumped,
                worker_url,
                worker_id,
                &indexer,
                &mut worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
                &mut |_| {},
            ),
            BatchResult::GapRecovered {
                expected: 1,
                received: 2,
            }
        );
        assert_eq!(indexer.current_size(), 0);
        assert!(worker_blocks.is_empty());
    }

    #[test]
    fn equal_and_lower_sequence_resets_clear_old_positive() {
        fn root_batch(sequence_number: u64, block_hash: i64) -> KvEventBatch {
            KvEventBatch {
                sequence_number,
                timestamp: 0.0,
                events: vec![KvCacheEvent {
                    event_id: sequence_number,
                    data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                        blocks: vec![KvBlock {
                            block_hash,
                            token_ids: vec![1, 2, 3, 4],
                            block_size: 4,
                            lora_id: None,
                            cache_level: None,
                        }],
                        parent_block_hash: None,
                    })),
                }],
                dp_rank: None,
            }
        }

        for (old_sequence, reset_sequence) in [(0, 0), (5, 1)] {
            let indexer = PositionalIndexer::new(64);
            let worker_url = "http://w1:8000";
            let worker_id = indexer.intern_worker(worker_url).unwrap();
            let mut worker_blocks = WorkerBlockMap::default();
            let mut last_seq = 0;
            let mut seen_batch = false;
            let mut orphan_skips = OrphanSkipTracker::default();
            KvEventMonitor::process_batch(
                &root_batch(old_sequence, 1),
                worker_url,
                worker_id,
                &indexer,
                &mut worker_blocks,
                &mut last_seq,
                &mut seen_batch,
                &mut orphan_skips,
                &mut |_| {},
            );
            assert_eq!(indexer.current_size(), 1);

            let reset = KvEventBatch {
                sequence_number: reset_sequence,
                timestamp: 0.0,
                events: Vec::new(),
                dp_rank: None,
            };
            assert_eq!(
                KvEventMonitor::process_batch(
                    &reset,
                    worker_url,
                    worker_id,
                    &indexer,
                    &mut worker_blocks,
                    &mut last_seq,
                    &mut seen_batch,
                    &mut orphan_skips,
                    &mut |_| {},
                ),
                BatchResult::EpochReset {
                    previous: old_sequence,
                    received: reset_sequence,
                }
            );
            assert_eq!(indexer.current_size(), 0);
            assert!(worker_blocks.is_empty());
        }
    }

    #[test]
    fn reconnect_reset_clears_index_and_invalidates_generation() {
        let monitor = KvEventMonitor::new(Some(4));
        let indexer = PositionalIndexer::new(64);
        let worker_url = "http://w1:8000";
        let worker_id = indexer.intern_worker(worker_url).unwrap();
        let mut worker_blocks = WorkerBlockMap::default();
        indexer
            .apply_stored(
                worker_id,
                &[StoredBlock {
                    seq_hash: SequenceHash(1),
                    content_hash: compute_content_hash(&[1, 2, 3, 4]),
                }],
                None,
                &mut worker_blocks,
            )
            .unwrap();
        monitor.mark_event_stream_ready_for_test(worker_url);
        let generation = monitor
            .certified_generation(worker_url, Duration::from_secs(30))
            .unwrap();
        let mut last_seq = 9;
        let mut seen_batch = true;

        monitor.mark_event_stream_not_ready_for_test(worker_url);
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::reset_stream_epoch(
            &indexer,
            worker_id,
            &mut worker_blocks,
            &mut last_seq,
            &mut seen_batch,
            &mut orphan_skips,
        );

        assert_eq!(indexer.current_size(), 0);
        assert!(worker_blocks.is_empty());
        assert_eq!(last_seq, 0);
        assert!(!seen_batch);
        assert!(!monitor.certified_generation_unchanged(
            worker_url,
            generation,
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn test_apply_event_dispatch_stored() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();
        let event = KvCacheEvent {
            event_id: 1,
            data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                blocks: vec![KvBlock {
                    block_hash: 42,
                    token_ids: vec![1, 2, 3, 4],
                    block_size: 4,
                    lora_id: None,
                    cache_level: None,
                }],
                parent_block_hash: None,
            })),
        };

        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_event(
            &event,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 1);
    }

    #[test]
    fn test_apply_event_dispatch_removed() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();

        let stored_event = KvCacheEvent {
            event_id: 1,
            data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                blocks: vec![KvBlock {
                    block_hash: 1,
                    token_ids: vec![1, 2, 3, 4],
                    block_size: 4,
                    lora_id: None,
                    cache_level: None,
                }],
                parent_block_hash: None,
            })),
        };
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_event(
            &stored_event,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );

        let removed_event = KvCacheEvent {
            event_id: 2,
            data: Some(kv_cache_event::Data::Removed(KvBlocksRemoved {
                block_hashes: vec![1],
                cache_level: None,
            })),
        };
        KvEventMonitor::apply_event(
            &removed_event,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 0);
    }

    #[test]
    fn test_apply_event_dispatch_cleared() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();

        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_event(
            &KvCacheEvent {
                event_id: 1,
                data: Some(kv_cache_event::Data::Stored(KvBlocksStored {
                    blocks: vec![KvBlock {
                        block_hash: 1,
                        token_ids: vec![1, 2, 3, 4],
                        block_size: 4,
                        lora_id: None,
                        cache_level: None,
                    }],
                    parent_block_hash: None,
                })),
            },
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );

        // Clear
        KvEventMonitor::apply_event(
            &KvCacheEvent {
                event_id: 2,
                data: Some(kv_cache_event::Data::Cleared(
                    smg_grpc_client::common_proto::KvCacheCleared {},
                )),
            },
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 0);
    }

    #[test]
    fn test_apply_event_no_data() {
        let indexer = PositionalIndexer::new(64);
        let w1 = indexer.intern_worker("http://w1:8000").unwrap();
        let mut wb = WorkerBlockMap::default();
        let event = KvCacheEvent {
            event_id: 1,
            data: None,
        };
        let mut orphan_skips = OrphanSkipTracker::default();
        KvEventMonitor::apply_event(
            &event,
            "http://w1:8000",
            w1,
            &indexer,
            &mut wb,
            &mut orphan_skips,
        );
        assert_eq!(indexer.current_size(), 0);
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_monitor_new() {
        let monitor = KvEventMonitor::new(None);
        assert!(!monitor.is_running().await);
    }

    #[tokio::test]
    async fn test_monitor_new_clamps_zero_jump_size() {
        let monitor = KvEventMonitor::new(Some(0));
        assert_eq!(monitor.jump_size, 1);
    }

    #[tokio::test]
    async fn test_get_indexer_nonexistent() {
        let monitor = KvEventMonitor::new(None);
        assert!(monitor.get_indexer("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_stop_empty_monitor() {
        let monitor = KvEventMonitor::new(None);
        monitor.stop().await;
    }

    #[tokio::test]
    async fn test_on_worker_removed_nonexistent() {
        let monitor = KvEventMonitor::new(None);
        monitor.on_worker_removed("http://nonexistent:8000").await;
    }

    // -----------------------------------------------------------------------
    // block_size learning
    // -----------------------------------------------------------------------

    #[test]
    fn test_set_block_size() {
        let monitor = KvEventMonitor::new(None);

        // Initially no block_size
        assert!(monitor.block_size("llama").is_none());

        // Set it
        monitor.set_block_size("llama", 32);
        assert_eq!(monitor.block_size("llama"), Some(32));

        // set_block_size doesn't overwrite existing value
        monitor.set_block_size("llama", 64);
        assert_eq!(monitor.block_size("llama"), Some(32));
    }

    #[tokio::test]
    async fn test_stop_clears_block_sizes() {
        let monitor = KvEventMonitor::new(None);
        monitor.set_block_size("llama", 16);
        assert_eq!(monitor.block_size("llama"), Some(16));

        monitor.stop().await;
        assert!(monitor.block_size("llama").is_none());
    }
}
