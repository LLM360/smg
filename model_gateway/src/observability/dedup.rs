//! Log deduplication for repeated signatures inside a fixed tumbling window.
//!
//! [`DedupLayer`] wraps the local fmt layers (stdout and optional file) so
//! that repeated logs from one callsite emit the first instance immediately
//! and then at most one count summary per window
//! (`repeated N=<count> times in the last minute`). ERROR events, events from
//! the state-change allowlist (worker health, monitor, circuit breaker), and
//! the summary target itself always pass through, uncounted.

use std::{
    collections::{hash_map::Entry, HashMap},
    fmt,
    hash::Hash,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use tracing::{
    callsite::{DefaultCallsite, Identifier},
    field::{Field, FieldSet, Visit},
    metadata::Kind,
    span, Event, Level, Metadata, Subscriber,
};
use tracing_subscriber::layer::{Context, Layer};

/// Fixed tumbling window length for deduplication (one minute).
const DEDUP_WINDOW: Duration = Duration::from_secs(60);

/// Target of the dedup summary events. Events with this target are exempt
/// from deduplication so a summary is never suppressed by its own layer.
pub const SUMMARY_TARGET: &str = "smg::log_dedup";

/// Modules whose warnings signal state changes (worker health checks,
/// circuit-breaker transitions, monitor lag rebuilds). Warnings from these
/// targets keep full emission: every event passes, uncounted.
///
/// Any addition here must come with a guarding test that names the specific
/// state-change warn site.
const STATE_CHANGE_ALLOWLIST: &[&str] = &[
    "smg::worker::circuit_breaker",
    "smg::worker::monitor",
    "smg::worker::worker",
];

/// True when an event with this target and level is subject to dedup.
/// ERROR events, the summary target, and allowlisted targets are exempt.
fn is_dedup_eligible(target: &str, level: Level) -> bool {
    if level == Level::ERROR {
        return false;
    }
    if target == SUMMARY_TARGET {
        return false;
    }
    !STATE_CHANGE_ALLOWLIST.iter().any(|allowed| {
        target == *allowed
            || target
                .strip_prefix(allowed)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// Result of recording one event observation against the dedup state.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// First instance of the current window: forward the event.
    Pass,
    /// Repeat inside the current window: counted and dropped.
    Drop,
    /// Window rollover: the expired window ended with repeats, so emit one
    /// summary with the count and the first-instance message, then forward
    /// the current event as the new window's first instance.
    Summary {
        /// Number of repeats suppressed in the expired window.
        suppressed: u64,
        /// Message of the expired window's first instance, if recorded.
        first_message: Option<String>,
    },
}

/// Per-signature window state.
#[derive(Debug, PartialEq, Eq)]
struct WindowEntry {
    window_start: Instant,
    suppressed: u64,
    first_message: Option<String>,
}

/// Pure dedup state machine over a tumbling window, keyed by signature.
/// The key type abstracts the callsite identity so tests can drive the
/// logic over synthetic keys; in production K is the event callsite.
#[derive(Debug)]
struct DedupState<K> {
    window: Duration,
    windows: HashMap<K, WindowEntry>,
}

impl<K: Eq + Hash> DedupState<K> {
    fn new(window: Duration) -> Self {
        Self {
            window,
            windows: HashMap::new(),
        }
    }

    /// Record one event of signature `key` at `now`. `message` is evaluated
    /// only when the event opens a window (first instance or rollover), so
    /// dropped repeats pay no message extraction cost.
    fn record<M: FnOnce() -> Option<String>>(
        &mut self,
        key: K,
        now: Instant,
        message: M,
    ) -> Decision {
        match self.windows.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(WindowEntry {
                    window_start: now,
                    suppressed: 0,
                    first_message: message(),
                });
                Decision::Pass
            }
            Entry::Occupied(mut slot) => {
                let entry = slot.get_mut();
                if now.duration_since(entry.window_start) < self.window {
                    entry.suppressed += 1;
                    return Decision::Drop;
                }
                let expired = std::mem::replace(
                    entry,
                    WindowEntry {
                        window_start: now,
                        suppressed: 0,
                        first_message: message(),
                    },
                );
                if expired.suppressed == 0 {
                    Decision::Pass
                } else {
                    Decision::Summary {
                        suppressed: expired.suppressed,
                        first_message: expired.first_message,
                    }
                }
            }
        }
    }
}

/// Visitor that captures an event's `message` field (the rendered
/// `format_args!` text) for the summary's first-instance message.
struct MessageVisitor(Option<String>);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" && self.0.is_none() {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

fn extract_message(event: &Event<'_>) -> Option<String> {
    let mut visitor = MessageVisitor(None);
    event.record(&mut visitor);
    visitor.0
}

/// Field names of the dedup summary events, in definition order.
const SUMMARY_FIELD_NAMES: &[&str] = &[
    "message",
    "repeated_count",
    "original_target",
    "original_module",
    "original_location",
];

/// One static callsite/metadata pair for the summary event of one level.
/// The summary is emitted at the suppressed signature's level, and
/// `Metadata` (and therefore its level) is statically bound per callsite.
macro_rules! summary_callsite {
    ($metadata:ident, $callsite:ident, $level:expr) => {
        static $callsite: DefaultCallsite = DefaultCallsite::new(&$metadata);
        static $metadata: Metadata<'static> = Metadata::new(
            "dedup window summary",
            SUMMARY_TARGET,
            $level,
            Some(file!()),
            Some(line!()),
            Some(module_path!()),
            FieldSet::new(SUMMARY_FIELD_NAMES, Identifier(&$callsite)),
            Kind::EVENT,
        );
    };
}

summary_callsite!(SUMMARY_METADATA_WARN, SUMMARY_CALLSITE_WARN, Level::WARN);
summary_callsite!(SUMMARY_METADATA_INFO, SUMMARY_CALLSITE_INFO, Level::INFO);
summary_callsite!(SUMMARY_METADATA_DEBUG, SUMMARY_CALLSITE_DEBUG, Level::DEBUG);
summary_callsite!(SUMMARY_METADATA_TRACE, SUMMARY_CALLSITE_TRACE, Level::TRACE);

fn summary_metadata(level: Level) -> &'static Metadata<'static> {
    if level == Level::WARN {
        &SUMMARY_METADATA_WARN
    } else if level == Level::INFO {
        &SUMMARY_METADATA_INFO
    } else if level == Level::DEBUG {
        &SUMMARY_METADATA_DEBUG
    } else {
        // ERROR is exempt from dedup and never reaches this call.
        debug_assert_eq!(level, Level::TRACE);
        &SUMMARY_METADATA_TRACE
    }
}

/// Emit the per-window summary for a suppressed signature through the inner
/// fmt layers. The summary carries the suppressed signature's level, the
/// suppressed count, the original target/module/location, and the
/// first-instance message, so the count stays visible to alerting in both
/// text and JSON output.
///
/// The event is constructed directly instead of being logged through the
/// `tracing` macros: macro emission dispatches through the thread's current
/// dispatcher, and tracing-core deliberately hands nested dispatches (an
/// event emitted from inside `on_event`) a `Dispatch::none` as a recursion
/// guard, which would silently drop the summary. Handing the event to the
/// inner layers bypasses the dispatcher entirely, so the summary cannot be
/// swallowed and the layer never re-enters itself.
fn emit_summary<S: Subscriber>(
    inner: &[Box<dyn Layer<S> + Send + Sync + 'static>],
    ctx: Context<'_, S>,
    level: Level,
    suppressed: u64,
    first_message: Option<&str>,
    metadata: &Metadata<'_>,
) {
    use tracing::field::{display, Value};

    // Build every value as a local first so the `ValueSet` (and the `Event`
    // that borrows it) stays alive across the per-layer `on_event` calls.
    let message = match first_message {
        Some(first) => format!("repeated N={suppressed} times in the last minute: {first}"),
        None => format!("repeated N={suppressed} times in the last minute"),
    };
    let location = format!(
        "{}:{}",
        metadata.file().unwrap_or_default(),
        metadata.line().unwrap_or_default()
    );
    let display_message = display(&message);
    let display_location = display(&location);
    let original_target = metadata.target();
    let original_module = metadata.module_path().unwrap_or_default();
    let summary_metadata = summary_metadata(level);
    // The fieldset is built from `SUMMARY_FIELD_NAMES`, so it always has
    // exactly the five handles below, in that definition order.
    let handles: Vec<Field> = summary_metadata.fields().iter().collect();
    let values: [(&Field, Option<&dyn Value>); 5] = [
        (&handles[0], Some(&display_message as &dyn Value)),
        (&handles[1], Some(&suppressed as &dyn Value)),
        (&handles[2], Some(&original_target as &dyn Value)),
        (&handles[3], Some(&original_module as &dyn Value)),
        (&handles[4], Some(&display_location as &dyn Value)),
    ];
    let value_set = summary_metadata.fields().value_set(&values);
    let event = Event::new(summary_metadata, &value_set);
    for layer in inner {
        layer.on_event(&event, ctx.clone());
    }
}

/// Layer that dedups repeated event signatures inside a fixed one-minute
/// tumbling window and emits one count summary per window.
///
/// It holds the inner fmt layers and delegates the full layer protocol to
/// them. It implements no global `enabled` filter, so `EnvFilter` behavior,
/// callsite interest caching, span context, and any sibling layer (OTEL)
/// are unchanged for every other path.
///
/// State is one mutex-guarded map keyed by callsite, bounded by the
/// compile-time set of event callsites in the workspace; it cannot grow
/// with traffic volume. Window rollover is lazy: a signature whose repeats
/// stop emits its tail summary at its next event (no background thread in
/// the logging path).
pub struct DedupLayer<S> {
    inner: Vec<Box<dyn Layer<S> + Send + Sync + 'static>>,
    state: Mutex<DedupState<Identifier>>,
    now: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl<S> fmt::Debug for DedupLayer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DedupLayer").finish_non_exhaustive()
    }
}

impl<S> DedupLayer<S> {
    /// Wrap the given fmt layers with the production one-minute window and
    /// the system clock.
    pub fn new(inner: Vec<Box<dyn Layer<S> + Send + Sync + 'static>>) -> Self {
        Self::with_clock(inner, DEDUP_WINDOW, Arc::new(Instant::now))
    }

    /// Constructor with window and clock seams; used only by tests.
    fn with_clock(
        inner: Vec<Box<dyn Layer<S> + Send + Sync + 'static>>,
        window: Duration,
        now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            state: Mutex::new(DedupState::new(window)),
            now,
        }
    }

    fn classify(&self, event: &Event<'_>) -> Decision {
        let metadata = event.metadata();
        let now = (self.now)();
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(metadata.callsite(), now, || extract_message(event))
    }
}

impl<S: Subscriber> Layer<S> for DedupLayer<S> {
    fn on_layer(&mut self, subscriber: &mut S) {
        self.inner.on_layer(subscriber);
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_new_span(attrs, id, ctx);
    }

    fn on_record(&self, span: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        self.inner.on_record(span, values, ctx);
    }

    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_follows_from(span, follows, ctx);
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_exit(id, ctx);
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        self.inner.on_close(id, ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if !is_dedup_eligible(metadata.target(), *metadata.level()) {
            self.inner.on_event(event, ctx);
            return;
        }
        match self.classify(event) {
            Decision::Pass => self.inner.on_event(event, ctx),
            Decision::Drop => {}
            Decision::Summary {
                suppressed,
                first_message,
            } => {
                // The dedup lock is released before this call. The summary
                // goes straight to the inner layers (see `emit_summary`), so
                // it never re-enters this layer and cannot be suppressed.
                emit_summary(
                    &self.inner,
                    ctx.clone(),
                    *metadata.level(),
                    suppressed,
                    first_message.as_deref(),
                    metadata,
                );
                self.inner.on_event(event, ctx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io, sync::Mutex as StdMutex};

    use serde_json::Value;
    use tracing::subscriber::with_default;
    use tracing_subscriber::{
        fmt::MakeWriter,
        layer::{Layered, SubscriberExt},
        EnvFilter, Registry,
    };

    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    /// The subscriber type under the dedup layer in these tests.
    type Stack = Layered<EnvFilter, Registry>;

    // ----- Pure state-machine tests (synthetic timelines, injected clock) -----

    fn at(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    mod state_machine {
        use super::*;

        #[test]
        fn first_occurrence_passes_and_emits_no_summary() {
            let base = Instant::now();
            let mut state = DedupState::<u64>::new(WINDOW);
            assert_eq!(
                state.record(1, base, || Some("first".to_string())),
                Decision::Pass
            );
            // A later event of the same signature after an empty window
            // opens a new window without any summary.
            assert_eq!(
                state.record(1, at(base, 120), || Some("later".to_string())),
                Decision::Pass
            );
        }

        #[test]
        fn repeats_within_window_are_suppressed_and_counted() {
            let base = Instant::now();
            let mut state = DedupState::<u64>::new(WINDOW);
            assert_eq!(state.record(1, base, || None), Decision::Pass);
            for _ in 0..784 {
                assert_eq!(
                    state.record(1, base + Duration::from_millis(1), || None),
                    Decision::Drop
                );
            }
            let decision = state.record(1, at(base, 60), || Some("new window".to_string()));
            assert_eq!(
                decision,
                Decision::Summary {
                    suppressed: 784,
                    first_message: None
                }
            );
        }

        #[test]
        fn rollover_emits_one_summary_for_expired_window_and_passes_current() {
            let base = Instant::now();
            let mut state = DedupState::<u64>::new(WINDOW);
            assert_eq!(
                state.record(1, base, || Some("v1".to_string())),
                Decision::Pass
            );
            assert_eq!(state.record(1, at(base, 1), || None), Decision::Drop);
            assert_eq!(state.record(1, at(base, 59), || None), Decision::Drop);
            // Rollover at exactly the window boundary (tumbling from first instance).
            let decision = state.record(1, at(base, 60), || Some("v2".to_string()));
            assert_eq!(
                decision,
                Decision::Summary {
                    suppressed: 2,
                    first_message: Some("v1".to_string())
                }
            );
            // The current event is the new window's first instance: its
            // message seeds the next summary.
            assert_eq!(state.record(1, at(base, 61), || None), Decision::Drop);
            let decision = state.record(1, at(base, 120), || Some("v3".to_string()));
            assert_eq!(
                decision,
                Decision::Summary {
                    suppressed: 1,
                    first_message: Some("v2".to_string())
                }
            );
        }

        #[test]
        fn zero_repeat_window_emits_no_summary() {
            let base = Instant::now();
            let mut state = DedupState::<u64>::new(WINDOW);
            assert_eq!(state.record(1, base, || None), Decision::Pass);
            assert_eq!(state.record(1, at(base, 700), || None), Decision::Pass);
        }

        #[test]
        fn two_callsites_keep_independent_windows() {
            let base = Instant::now();
            let mut state = DedupState::<u64>::new(WINDOW);
            assert_eq!(state.record(1, base, || None), Decision::Pass);
            assert_eq!(state.record(2, at(base, 30), || None), Decision::Pass);
            // Key 1 repeats are suppressed without disturbing key 2's window.
            assert_eq!(state.record(1, at(base, 31), || None), Decision::Drop);
            assert_eq!(state.record(2, at(base, 89), || None), Decision::Drop);
            assert_eq!(
                state.record(2, at(base, 90), || Some("b".to_string())),
                Decision::Summary {
                    suppressed: 1,
                    first_message: None
                }
            );
            assert_eq!(
                state.record(1, at(base, 91), || Some("a".to_string())),
                Decision::Summary {
                    suppressed: 1,
                    first_message: None
                }
            );
        }
    }

    mod policy {
        use super::*;

        #[test]
        fn error_is_never_deduped() {
            assert!(!is_dedup_eligible(
                "smg::worker::kv_event_monitor",
                Level::ERROR
            ));
        }

        #[test]
        fn warning_and_below_are_deduped() {
            for level in [Level::WARN, Level::INFO, Level::DEBUG, Level::TRACE] {
                assert!(is_dedup_eligible("smg::worker::kv_event_monitor", level));
                assert!(is_dedup_eligible("llm_tokenizer::tiktoken", level));
            }
        }

        #[test]
        fn summary_target_never_suppressed() {
            assert!(!is_dedup_eligible(SUMMARY_TARGET, Level::WARN));
        }

        /// Guarding test for the state-change allowlist: each entry names
        /// the specific warn sites it protects. Additions to the allowlist
        /// extend this test with their exact target.
        #[test]
        fn state_change_allowlist_passes_uncounted() {
            for target in STATE_CHANGE_ALLOWLIST {
                assert!(!is_dedup_eligible(target, Level::WARN), "{target}");
                // Submodules of an allowlisted module inherit the exemption.
                let submodule = format!("{target}::detail");
                assert!(!is_dedup_eligible(&submodule, Level::WARN), "{submodule}");
            }
            let guard = "smg::worker::other";
            assert!(!STATE_CHANGE_ALLOWLIST.contains(&guard));
            assert!(is_dedup_eligible(guard, Level::WARN));
        }
    }

    // ----- Subscriber-level integration tests (manual clock) -----

    /// Manual clock for deterministic tumbling windows.
    #[derive(Clone)]
    struct ManualClock {
        base: Instant,
        offset: Arc<StdMutex<Duration>>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset: Arc::new(StdMutex::new(Duration::ZERO)),
            }
        }

        fn now_fn(&self) -> Arc<dyn Fn() -> Instant + Send + Sync> {
            let base = self.base;
            let offset = Arc::clone(&self.offset);
            Arc::new(move || base + *offset.lock().unwrap())
        }

        fn advance(&self, by: Duration) {
            *self.offset.lock().unwrap() += by;
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedEvent {
        target: String,
        level: Level,
        message: Option<String>,
        repeated_count: Option<u64>,
    }

    struct FieldRecorder {
        target: String,
        message: Option<String>,
        repeated_count: Option<u64>,
    }

    impl FieldRecorder {
        fn new(target: &str) -> Self {
            Self {
                target: target.to_string(),
                message: None,
                repeated_count: None,
            }
        }
    }

    impl Visit for FieldRecorder {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == "message" && self.message.is_none() {
                self.message = Some(format!("{value:?}"));
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "repeated_count" {
                self.repeated_count = Some(value);
            }
        }
    }

    /// Recording layer that captures every forwarded event.
    #[derive(Clone, Default)]
    struct Recorder {
        events: Arc<StdMutex<Vec<RecordedEvent>>>,
    }

    impl Recorder {
        fn events(&self) -> Vec<RecordedEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl<S: Subscriber> Layer<S> for Recorder {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut fields = FieldRecorder::new(event.metadata().target());
            event.record(&mut fields);
            self.events.lock().unwrap().push(RecordedEvent {
                target: fields.target,
                level: *event.metadata().level(),
                message: fields.message,
                repeated_count: fields.repeated_count,
            });
        }
    }

    fn dedup_subscriber(clock: &ManualClock, recorder: &Recorder) -> impl Subscriber + Send + Sync {
        let inner: Vec<Box<dyn Layer<Stack> + Send + Sync>> = vec![Box::new(recorder.clone())];
        let dedup = DedupLayer::with_clock(inner, WINDOW, clock.now_fn());
        Registry::default()
            .with(EnvFilter::new("smg=debug"))
            .with(dedup)
    }

    mod integration {
        use super::*;

        #[test]
        fn flood_from_one_callsite_yields_first_instance_plus_window_summaries() {
            let clock = ManualClock::new();
            let recorder = Recorder::default();
            let subscriber = dedup_subscriber(&clock, &recorder);
            with_default(subscriber, || {
                // 1001 events from one callsite: 16 full windows of 60
                // events at 1/s spacing (1 pass + 59 suppressed per window),
                // then a tail window holding the 40 events at t=960..999
                // (1 pass + 39 suppressed).
                for i in 0..1001 {
                    if i == 1000 {
                        // Expire the tail window, then emit from the same
                        // callsite: emits the tail summary (39 suppressed)
                        // and this event passes as first instance of the
                        // next window.
                        clock.advance(Duration::from_secs(20));
                    }
                    tracing::warn!("flood signature message");
                    clock.advance(Duration::from_secs(1));
                }
            });
            let events = recorder.events();
            let (passes, summaries): (Vec<_>, Vec<_>) =
                events.iter().partition(|e| e.target != SUMMARY_TARGET);
            assert_eq!(passes.len(), 18, "one pass per window, all windows");
            assert!(passes
                .iter()
                .all(|e| e.message.as_deref() == Some("flood signature message")));
            assert_eq!(summaries.len(), 17, "one summary per expired window");
            assert_eq!(
                summaries[0].message.as_deref(),
                Some("repeated N=59 times in the last minute: flood signature message")
            );
            assert_eq!(summaries[0].repeated_count, Some(59));
            assert_eq!(summaries[0].level, Level::WARN);
            // Counts reconcile with the suppressed volume: alerts can trust
            // the summaries.
            let total_suppressed: u64 = summaries.iter().map(|e| e.repeated_count.unwrap()).sum();
            assert_eq!(total_suppressed, 1001 - 18);
            assert_eq!(summaries.last().unwrap().repeated_count, Some(39));
        }

        #[test]
        fn error_flood_passes_every_time_uncounted() {
            let clock = ManualClock::new();
            let recorder = Recorder::default();
            let subscriber = dedup_subscriber(&clock, &recorder);
            with_default(subscriber, || {
                for _ in 0..100 {
                    tracing::error!("unrecoverable backend failure");
                    clock.advance(Duration::from_millis(10));
                }
            });
            let events = recorder.events();
            assert_eq!(events.len(), 100);
            assert!(events.iter().all(|e| e.target != SUMMARY_TARGET));
        }

        #[test]
        fn allowlisted_state_change_warnings_pass_every_time_uncounted() {
            let clock = ManualClock::new();
            let recorder = Recorder::default();
            let subscriber = dedup_subscriber(&clock, &recorder);
            with_default(subscriber, || {
                for _ in 0..10 {
                    tracing::warn!(target: "smg::worker::worker", "health check failed");
                    tracing::warn!(target: "smg::worker::circuit_breaker", "breaker open");
                    tracing::warn!(target: "smg::worker::monitor", "lagged events rebuilt");
                    clock.advance(Duration::from_secs(1));
                }
            });
            let events = recorder.events();
            assert_eq!(events.len(), 30);
            assert!(events.iter().all(|e| e.target != SUMMARY_TARGET));
        }

        #[test]
        fn summary_events_are_never_suppressed() {
            let clock = ManualClock::new();
            let recorder = Recorder::default();
            let subscriber = dedup_subscriber(&clock, &recorder);
            with_default(subscriber, || {
                for _ in 0..5 {
                    tracing::warn!(target: SUMMARY_TARGET, "external producer text");
                }
            });
            let events = recorder.events();
            assert_eq!(events.len(), 5);
        }

        #[test]
        fn two_callsites_same_template_keep_independent_windows() {
            let clock = ManualClock::new();
            let recorder = Recorder::default();
            let subscriber = dedup_subscriber(&clock, &recorder);
            fn site_a() {
                tracing::warn!("same template text");
            }
            fn site_b() {
                tracing::warn!("same template text");
            }
            with_default(subscriber, || {
                site_a();
                site_b(); // distinct callsite: passes immediately
                site_a(); // repeat of site A: suppressed
                site_b(); // repeat of site B: suppressed
            });
            let events = recorder.events();
            assert_eq!(events.len(), 2);
            assert!(events
                .iter()
                .all(|e| e.message.as_deref() == Some("same template text")));
        }
    }

    // ----- Summary shape tests through the real fmt layers -----

    #[derive(Clone, Default)]
    struct SharedBuffer {
        bytes: Arc<StdMutex<Vec<u8>>>,
    }

    impl SharedBuffer {
        fn lines(&self) -> Vec<String> {
            let bytes = self.bytes.lock().unwrap().clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    struct BufferWriter {
        buffer: SharedBuffer,
    }

    impl io::Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = BufferWriter;

        fn make_writer(&'a self) -> Self::Writer {
            BufferWriter {
                buffer: self.clone(),
            }
        }
    }

    /// Drive one small flood through a full `EnvFilter` + `DedupLayer` stack
    /// with the given inner fmt layer and return the captured lines.
    fn flood_lines(
        fmt_layer: impl FnOnce(SharedBuffer) -> Box<dyn Layer<Stack> + Send + Sync>,
    ) -> Vec<String> {
        let clock = ManualClock::new();
        let buffer = SharedBuffer::default();
        let inner: Vec<Box<dyn Layer<Stack> + Send + Sync>> = vec![fmt_layer(buffer.clone())];
        let dedup = DedupLayer::with_clock(inner, WINDOW, clock.now_fn());
        let subscriber = Registry::default()
            .with(EnvFilter::new("smg=info"))
            .with(dedup);
        with_default(subscriber, || {
            // All five events come from this one macro site: the dedup
            // signature is the callsite, so they share one window.
            for i in 0..5 {
                if i == 4 {
                    // Expire the window before the fifth event: it emits
                    // the summary for the three suppressed repeats and then
                    // passes as the first instance of the new window.
                    clock.advance(Duration::from_secs(60));
                }
                tracing::info!("decode fallback used");
                clock.advance(Duration::from_secs(1));
            }
        });
        buffer.lines()
    }

    #[test]
    fn summary_text_matches_repeated_n_form_with_first_instance_message() {
        let lines = flood_lines(|buffer| {
            Box::new(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(buffer),
            )
        });
        assert_eq!(lines.len(), 3, "first pass + summary + new-window pass");
        assert!(lines[0].contains("decode fallback used"));
        assert!(
            lines[1].contains("repeated N=3 times in the last minute: decode fallback used"),
            "summary line: {}",
            lines[1]
        );
        assert!(lines[1].contains("smg::log_dedup"));
        assert!(lines[2].contains("decode fallback used"));
    }

    #[test]
    fn summary_json_carries_count_and_original_context_as_fields() {
        let lines = flood_lines(|buffer| {
            Box::new(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_writer(buffer),
            )
        });
        assert_eq!(lines.len(), 3);
        let summary: Value = serde_json::from_str(&lines[1]).expect("json summary line");
        assert_eq!(summary["target"], SUMMARY_TARGET);
        assert_eq!(summary["level"], "INFO");
        assert_eq!(summary["repeated_count"], 3);
        assert_eq!(
            summary["message"],
            "repeated N=3 times in the last minute: decode fallback used"
        );
        assert_eq!(
            summary["original_target"],
            "smg::observability::dedup::tests"
        );
        assert!(summary["original_module"].is_string());
        assert!(summary["original_location"].is_string());
        // The first-instance event itself is intact in JSON mode.
        let first: Value = serde_json::from_str(&lines[0]).expect("json first line");
        assert_eq!(first["message"], "decode fallback used");
    }
}
