//! Request events for observability and monitoring.
//!
//! Events use DEBUG level when OTEL is disabled, INFO when enabled.

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

/// Normalized request-level stats collected from engine-specific responses.
#[derive(Debug, Clone)]
pub struct UnifiedRequestStats {
    pub engine: &'static str,
    pub error_message: Option<String>,
    pub request_received_timestamp_s: Option<f64>,
    pub first_token_generated_timestamp_s: Option<f64>,
    pub request_finished_timestamp_s: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub spec_decoding_acceptance_rate: Option<f64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

/// Unified request-stats event emitted once per backend request.
#[derive(Debug, Clone)]
pub struct RequestStatsEvent<'a> {
    pub request_id: &'a str,
    pub model: &'a str,
    pub router_backend: &'a str,
    pub http_status_code: Option<u16>,
    pub error_message: Option<&'a str>,
    pub stats: &'a UnifiedRequestStats,
}

impl Event for RequestStatsEvent<'_> {
    #[inline]
    fn emit(&self) {
        let request_received_timestamp_s =
            format_optional_f64(self.stats.request_received_timestamp_s);
        let first_token_generated_timestamp_s =
            format_optional_f64(self.stats.first_token_generated_timestamp_s);
        let request_finished_timestamp_s =
            format_optional_f64(self.stats.request_finished_timestamp_s);
        let cache_hit_rate = format_optional_f64(self.stats.cache_hit_rate);
        let spec_decoding_acceptance_rate =
            format_optional_f64(self.stats.spec_decoding_acceptance_rate);
        let http_status_code = format_optional_u16(self.http_status_code);
        let error_message = self
            .error_message
            .or(self.stats.error_message.as_deref())
            .unwrap_or("None");

        if is_otel_enabled() {
            event!(
                Level::INFO,
                request_id = %self.request_id,
                model = %self.model,
                router_backend = %self.router_backend,
                http_status_code = %http_status_code,
                error_message = %error_message,
                engine = %self.stats.engine,
                request_received_timestamp_s = %request_received_timestamp_s,
                first_token_generated_timestamp_s = %first_token_generated_timestamp_s,
                request_finished_timestamp_s = %request_finished_timestamp_s,
                cache_hit_rate = %cache_hit_rate,
                spec_decoding_acceptance_rate = %spec_decoding_acceptance_rate,
                prompt_tokens = self.stats.prompt_tokens,
                completion_tokens = self.stats.completion_tokens,
                cached_tokens = self.stats.cached_tokens,
                "request_stats"
            );
        } else {
            debug!(
                request_id = %self.request_id,
                model = %self.model,
                router_backend = %self.router_backend,
                http_status_code = %http_status_code,
                error_message = %error_message,
                engine = %self.stats.engine,
                request_received_timestamp_s = %request_received_timestamp_s,
                first_token_generated_timestamp_s = %first_token_generated_timestamp_s,
                request_finished_timestamp_s = %request_finished_timestamp_s,
                cache_hit_rate = %cache_hit_rate,
                spec_decoding_acceptance_rate = %spec_decoding_acceptance_rate,
                prompt_tokens = self.stats.prompt_tokens,
                completion_tokens = self.stats.completion_tokens,
                cached_tokens = self.stats.cached_tokens,
                "request_stats"
            );
        }
    }
}

#[inline]
fn format_optional_f64(value: Option<f64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "None".to_string())
}

#[inline]
fn format_optional_u16(value: Option<u16>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "None".to_string())
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
