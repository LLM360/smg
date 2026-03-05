//! Request events for observability and monitoring.
//!
//! Events use DEBUG level when OTEL is disabled, INFO when enabled.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, event, Level};

use super::otel_trace::is_otel_enabled;

/// Module path used by CustomOtelFilter to identify events for OTEL export.
#[inline]
pub const fn get_module_path() -> &'static str {
    "smg::observability::events"
}

pub trait Event {
    fn emit(&self);
}

/// Event emitted when a prefill-decode request pair is sent.
#[derive(Debug, Clone, Copy)]
pub struct RequestPDSentEvent<'a> {
    pub prefill_url: &'a str,
    pub decode_url: &'a str,
}

impl Event for RequestPDSentEvent<'_> {
    #[inline]
    fn emit(&self) {
        if is_otel_enabled() {
            event!(
                Level::INFO,
                prefill_url = %self.prefill_url,
                decode_url = %self.decode_url,
                "Sending concurrent requests"
            );
        } else {
            debug!(
                prefill_url = %self.prefill_url,
                decode_url = %self.decode_url,
                "Sending concurrent requests"
            );
        }
    }
}

/// Event emitted when a request is sent to a worker.
#[derive(Debug, Clone, Copy)]
pub struct RequestSentEvent<'a> {
    pub url: &'a str,
}

impl Event for RequestSentEvent<'_> {
    #[inline]
    fn emit(&self) {
        if is_otel_enabled() {
            event!(Level::INFO, url = %self.url, "Sending request");
        } else {
            debug!(url = %self.url, "Sending request");
        }
    }
}

/// Event emitted when concurrent requests are received.
#[derive(Debug, Clone, Copy)]
pub struct RequestReceivedEvent;

impl Event for RequestReceivedEvent {
    #[inline]
    fn emit(&self) {
        if is_otel_enabled() {
            event!(Level::INFO, "Received concurrent requests");
        } else {
            debug!("Received concurrent requests");
        }
    }
}

/// Backend-specific field names mapped into the unified request stats schema.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestStatsFieldMapping {
    pub request_received_timestamp: Option<&'static str>,
    pub first_token_generated_timestamp: Option<&'static str>,
    pub request_finished_timestamp: Option<&'static str>,
    pub cache_hit_rate: Option<&'static str>,
    pub spec_decoding_acceptance_rate: Option<&'static str>,
    pub prompt_tokens: Option<&'static str>,
    pub completion_tokens: Option<&'static str>,
}

/// Normalized request-level stats collected from engine-specific responses.
#[derive(Debug, Clone)]
pub struct UnifiedRequestStats {
    pub engine: &'static str,
    pub field_mapping: RequestStatsFieldMapping,
    pub request_received_timestamp_s: Option<f64>,
    pub first_token_generated_timestamp_s: Option<f64>,
    pub request_finished_timestamp_s: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub spec_decoding_acceptance_rate: Option<f64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl UnifiedRequestStats {
    /// Fill in missing timestamps with SMG-derived wall-clock values.
    pub fn apply_timestamp_fallbacks(&mut self, fallbacks: RequestTimestampFallbacks) {
        if self.request_received_timestamp_s.is_none() {
            self.request_received_timestamp_s = fallbacks.request_received_timestamp_s;
        }
        if self.first_token_generated_timestamp_s.is_none() {
            self.first_token_generated_timestamp_s = fallbacks.first_token_generated_timestamp_s;
        }
        if self.request_finished_timestamp_s.is_none() {
            self.request_finished_timestamp_s = fallbacks.request_finished_timestamp_s;
        }
    }
}

/// Optional wall-clock timestamp fallbacks in Unix seconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestTimestampFallbacks {
    pub request_received_timestamp_s: Option<f64>,
    pub first_token_generated_timestamp_s: Option<f64>,
    pub request_finished_timestamp_s: Option<f64>,
}

/// Current wall-clock timestamp as Unix epoch seconds.
#[inline]
pub fn now_unix_timestamp_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Build fallback timestamps from streaming/non-streaming local timing.
#[inline]
pub fn request_timestamps_from_local_timing(
    start_time: Instant,
    first_token_time: Option<Instant>,
) -> RequestTimestampFallbacks {
    let finished_at = now_unix_timestamp_seconds();
    let elapsed = start_time.elapsed().as_secs_f64();
    let received_at = (finished_at - elapsed).max(0.0);
    let first_token_at = first_token_time
        .map(|t| t.duration_since(start_time).as_secs_f64())
        .map(|ttft| received_at + ttft);

    RequestTimestampFallbacks {
        request_received_timestamp_s: Some(received_at),
        first_token_generated_timestamp_s: first_token_at,
        request_finished_timestamp_s: Some(finished_at),
    }
}

/// Unified request-stats event emitted once per backend request.
#[derive(Debug, Clone)]
pub struct RequestStatsEvent<'a> {
    pub request_id: &'a str,
    pub model: &'a str,
    pub router_backend: &'a str,
    pub stats: &'a UnifiedRequestStats,
}

impl Event for RequestStatsEvent<'_> {
    #[inline]
    fn emit(&self) {
        if is_otel_enabled() {
            event!(
                Level::INFO,
                request_id = %self.request_id,
                model = %self.model,
                router_backend = %self.router_backend,
                engine = %self.stats.engine,
                request_received_timestamp_s = ?self.stats.request_received_timestamp_s,
                first_token_generated_timestamp_s = ?self.stats.first_token_generated_timestamp_s,
                request_finished_timestamp_s = ?self.stats.request_finished_timestamp_s,
                cache_hit_rate = ?self.stats.cache_hit_rate,
                spec_decoding_acceptance_rate = ?self.stats.spec_decoding_acceptance_rate,
                prompt_tokens = self.stats.prompt_tokens,
                completion_tokens = self.stats.completion_tokens,
                "request_stats"
            );
        } else {
            debug!(
                request_id = %self.request_id,
                model = %self.model,
                router_backend = %self.router_backend,
                engine = %self.stats.engine,
                request_received_timestamp_s = ?self.stats.request_received_timestamp_s,
                first_token_generated_timestamp_s = ?self.stats.first_token_generated_timestamp_s,
                request_finished_timestamp_s = ?self.stats.request_finished_timestamp_s,
                cache_hit_rate = ?self.stats.cache_hit_rate,
                spec_decoding_acceptance_rate = ?self.stats.spec_decoding_acceptance_rate,
                prompt_tokens = self.stats.prompt_tokens,
                completion_tokens = self.stats.completion_tokens,
                "request_stats"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::*;

    #[test]
    fn test_event_sizes() {
        assert_eq!(size_of::<RequestReceivedEvent>(), 0);
        assert_eq!(size_of::<RequestSentEvent>(), 16);
        assert_eq!(size_of::<RequestPDSentEvent>(), 32);
    }
}
