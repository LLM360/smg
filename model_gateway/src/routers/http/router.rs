use std::{error::Error as _, sync::Arc, time::Instant};

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{
        header::{CONTENT_TYPE, RETRY_AFTER},
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{stream, StreamExt};
use openai_protocol::{
    chat::ChatCompletionRequest,
    classify::ClassifyRequest,
    common::GenerationRequest,
    completion::CompletionRequest,
    embedding::EmbeddingRequest,
    generate::GenerateRequest,
    messages::CreateMessageRequest,
    realtime_session::{
        RealtimeClientSecretCreateRequest, RealtimeSessionCreateRequest,
        RealtimeTranscriptionSessionCreateRequest,
    },
    rerank::{RerankRequest, RerankResponse, RerankResult},
    responses::ResponsesRequest,
    transcription::{AudioFile, TranscriptionRequest},
};
use reqwest::{
    multipart::{Form, Part},
    Client, Request as ReqwestRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};
use tracing::{error, warn};

use crate::{
    app_context::{AppContext, AppContextBuilder},
    config::types::RetryConfig,
    middleware::{
        scheduler::{
            LocalAdaptiveRejection, RedeemedCapacityCreditAuthorization,
            ADMISSION_PARTITION_HEADER, ADMISSION_PARTITION_LABEL,
        },
        TenantRequestMeta,
    },
    observability::{
        events::{self, Event},
        metrics::{bool_to_static_str, metrics_labels, Metrics},
        otel_trace::inject_trace_context_http,
    },
    policies::{CacheColdBootstrapRoute, LoadBalancingPolicy, PolicyRegistry, SelectWorkerInfo},
    routers::{
        common::{
            header_utils,
            realtime::{
                rest::forward_realtime_rest, webrtc, webrtc::handle_realtime_webrtc,
                ws::handle_realtime_ws, RealtimeLabels, RealtimeRegistry,
            },
            retry::{is_retryable_status, RetryExecutor},
            sse::{SseDecoder, SseFrame},
            worker_selection::{SelectWorkerRequest, WorkerSelector},
        },
        error::{self, extract_error_code_from_response},
        grpc::{
            adaptive_admission::{
                AdaptiveAdmissionController, AdaptiveRequestTracker, PredictionFeatures,
                FLAG_MULTIPLE_COMPLETIONS, FLAG_REASONING, FLAG_STREAMING, FLAG_STRUCTURED_OUTPUT,
                FLAG_TOOLS,
            },
            utils::{error_type_from_status, route_to_endpoint},
        },
        openai::strip_default_sglang_fields,
        RouterTrait,
    },
    worker::{AttachedBody, ConnectionMode, Worker, WorkerLoadGuard, WorkerRegistry, WorkerType},
};

/// Max body size for a WebRTC `/v1/realtime/calls` SDP offer (10 MiB).
const WEBRTC_REQUEST_BODY_LIMIT: usize = 10 * 1024 * 1024;
const ADAPTIVE_RESPONSE_TAIL_LIMIT: usize = 256 * 1024;
const COMET_USER_HEADER: &str = "x-comet-user";
const COMET_WORKLOAD_TYPE_HEADER: &str = "x-comet-workload-type";

/// Exact bytes already buffered by the non-streaming relay. Keeping a cheap
/// clone in response extensions lets the distribution path prove completion
/// without consuming or rewriting the client-visible body.
#[derive(Clone)]
struct BufferedResponseBytes(bytes::Bytes);

/// Regular router that uses injected load balancing policies
pub struct Router {
    worker_registry: Arc<WorkerRegistry>,
    policy_registry: Arc<PolicyRegistry>,
    client: Client,
    no_redirect_client: Client,
    retry_config: RetryConfig,
    realtime_registry: Arc<RealtimeRegistry>,
    webrtc_bind_addr: Option<std::net::IpAddr>,
    webrtc_stun_server: Option<String>,
    adaptive_admission: Option<Arc<AdaptiveAdmissionController>>,
}

struct WorkerSelection {
    worker: Arc<dyn Worker>,
    policy: Arc<dyn LoadBalancingPolicy>,
    reservation_cost: Option<u64>,
    cold_bootstrap_route: Option<CacheColdBootstrapRoute>,
}

enum WorkerSelectionResult {
    Selected(WorkerSelection),
    NoAvailable,
    UnleasedOwnerExpansionBlocked,
}

struct HttpAdmission {
    tracker: Option<AdaptiveRequestTracker>,
    distribution_partition: Option<String>,
    owner_expansion_guard_partition: Option<String>,
}

fn trusted_header(headers: Option<&HeaderMap>, name: &str) -> Option<String> {
    headers
        .and_then(|headers| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn worker_admission_partition(worker: &dyn Worker) -> &str {
    worker
        .metadata()
        .spec
        .labels
        .get(ADMISSION_PARTITION_LABEL)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| worker.model_id())
}

fn workers_in_admission_partition(
    workers: &[Arc<dyn Worker>],
    partition: &str,
) -> Vec<Arc<dyn Worker>> {
    workers
        .iter()
        .filter(|worker| worker_admission_partition(worker.as_ref()) == partition)
        .cloned()
        .collect()
}

/// Publish a terminal cache-owner proof only while the exact dispatched
/// worker is still authoritative in the registry.
///
/// `with_authoritative_worker` holds the target's mutation lock through the
/// policy commit, so remove and same-URL replace cannot land between this
/// membership check and owner publication. The worker list is deliberately
/// re-read inside that critical section instead of reusing a pre-dispatch
/// snapshot.
fn commit_cache_owner_if_authoritative(
    worker_registry: &WorkerRegistry,
    expected_worker: &Arc<dyn Worker>,
    model_id: &str,
    partition: &str,
    commit: impl FnOnce(&[Arc<dyn Worker>]) -> bool,
) -> bool {
    worker_registry
        .with_authoritative_worker(expected_worker, || {
            let model_filter = (model_id != crate::worker::UNKNOWN_MODEL_ID).then_some(model_id);
            let current_workers = workers_in_admission_partition(
                &worker_registry.get_workers_filtered(
                    model_filter,
                    Some(WorkerType::Regular),
                    Some(ConnectionMode::Http),
                    None,
                    false,
                ),
                partition,
            );
            commit(&current_workers)
        })
        .unwrap_or(false)
}

fn local_distribution_rejection(message: &'static str) -> Response {
    let mut response = error::create_error(
        StatusCode::TOO_MANY_REQUESTS,
        "adaptive_distribution_unavailable",
        message,
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response.extensions_mut().insert(LocalAdaptiveRejection);
    response
}

fn should_retry_upstream_response(response: &Response, owner_expansion_guarded: bool) -> bool {
    !owner_expansion_guarded
        && is_retryable_status(response.status())
        && response
            .extensions()
            .get::<LocalAdaptiveRejection>()
            .is_none()
}

fn approximate_prompt_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count();
    chars
        .saturating_add(3)
        .checked_div(4)
        .unwrap_or(usize::MAX)
        .min(u32::MAX as usize) as u32
}

fn value_u32(value: Option<&serde_json::Value>) -> Option<u32> {
    value
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.min(u64::from(u32::MAX)) as u32)
}

fn positive_u32(value: Option<&serde_json::Value>) -> Option<u32> {
    value_u32(value).filter(|value| *value > 0)
}

fn request_multiplicity(value: &serde_json::Value) -> u32 {
    let returned = positive_u32(value.get("n"))
        .or_else(|| positive_u32(value.pointer("/sampling_params/n")))
        .unwrap_or(1);
    positive_u32(value.get("best_of"))
        .unwrap_or(returned)
        .max(returned)
}

fn populated(value: &serde_json::Value, field: &str) -> bool {
    value.get(field).is_some_and(|value| {
        !value.is_null()
            && !value.as_array().is_some_and(Vec::is_empty)
            && !value.as_object().is_some_and(serde_json::Map::is_empty)
    })
}

fn http_prediction_features<T: GenerationRequest + serde::Serialize>(
    typed_req: &T,
    headers: Option<&HeaderMap>,
    route: &'static str,
    model_id: &str,
    text: &str,
) -> PredictionFeatures {
    let value = serde_json::to_value(typed_req).unwrap_or(serde_json::Value::Null);
    let multiplicity = request_multiplicity(&value);
    let mut flags = 0;
    if multiplicity > 1 {
        flags |= FLAG_MULTIPLE_COMPLETIONS;
    }
    if populated(&value, "tools") {
        flags |= FLAG_TOOLS;
    }
    if ["response_format", "regex", "ebnf", "json_schema", "text"]
        .into_iter()
        .any(|field| populated(&value, field))
        || value.get("sampling_params").is_some_and(|params| {
            ["regex", "ebnf", "json_schema"]
                .into_iter()
                .any(|field| populated(params, field))
        })
    {
        flags |= FLAG_STRUCTURED_OUTPUT;
    }
    if ["reasoning_effort", "reasoning", "thinking"]
        .into_iter()
        .any(|field| populated(&value, field))
    {
        flags |= FLAG_REASONING;
    }
    if typed_req.is_stream() {
        flags |= FLAG_STREAMING;
    }

    PredictionFeatures {
        model: model_id.to_string(),
        user: trusted_header(headers, COMET_USER_HEADER).unwrap_or_else(|| "anonymous".to_string()),
        workload_type: trusted_header(headers, COMET_WORKLOAD_TYPE_HEADER)
            .unwrap_or_else(|| "unclassified".to_string()),
        endpoint: route_to_endpoint(route),
        prompt_tokens: approximate_prompt_tokens(text),
        max_output_tokens: typed_req
            .max_output_tokens_for_routing()
            .map(|limit| limit.saturating_mul(multiplicity)),
        generation_flags: flags,
    }
}

fn output_tokens_from_value(value: &serde_json::Value) -> Option<u32> {
    [
        "/usage/completion_tokens",
        "/usage/output_tokens",
        "/meta_info/completion_tokens",
    ]
    .into_iter()
    .find_map(|pointer| value_u32(value.pointer(pointer)))
}

fn output_tokens_from_truncated_json_tail(body: &[u8]) -> Option<u32> {
    [b"\"usage\"".as_slice(), b"\"meta_info\"".as_slice()]
        .into_iter()
        .find_map(|key| {
            let offset = body.windows(key.len()).rposition(|window| window == key)? + key.len();
            let remainder = &body[offset..];
            let object = &remainder[remainder.iter().position(|byte| *byte == b':')? + 1..];
            let value = serde_json::Deserializer::from_slice(object)
                .into_iter::<serde_json::Value>()
                .next()?
                .ok()?;
            value_u32(value.get("completion_tokens"))
                .or_else(|| value_u32(value.get("output_tokens")))
        })
}

fn observed_output_tokens(body: &[u8]) -> Option<u32> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(tokens) = output_tokens_from_value(&value) {
            return Some(tokens);
        }
    }
    if let Some(tokens) = output_tokens_from_truncated_json_tail(body) {
        return Some(tokens);
    }

    body.split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = line
                .strip_suffix(b"\r")
                .unwrap_or(line)
                .strip_prefix(b"data:")?;
            let line = line.strip_prefix(b" ").unwrap_or(line);
            if line == b"[DONE]" {
                return None;
            }
            serde_json::from_slice::<serde_json::Value>(line)
                .ok()
                .and_then(|value| output_tokens_from_value(&value))
        })
        .next_back()
}

struct AdaptiveTrackingBody {
    inner: Body,
    tracker: Option<AdaptiveRequestTracker>,
    tail: Vec<u8>,
}

impl AdaptiveTrackingBody {
    fn wrap_response(response: Response, tracker: AdaptiveRequestTracker) -> Response {
        let (parts, body) = response.into_parts();
        Response::from_parts(
            parts,
            Body::new(Self {
                inner: body,
                tracker: Some(tracker),
                tail: Vec::new(),
            }),
        )
    }

    fn append_tail(&mut self, data: &[u8]) {
        if data.len() >= ADAPTIVE_RESPONSE_TAIL_LIMIT {
            self.tail.clear();
            self.tail
                .extend_from_slice(&data[data.len() - ADAPTIVE_RESPONSE_TAIL_LIMIT..]);
            return;
        }
        let overflow = self
            .tail
            .len()
            .saturating_add(data.len())
            .saturating_sub(ADAPTIVE_RESPONSE_TAIL_LIMIT);
        if overflow > 0 {
            self.tail.drain(..overflow);
        }
        self.tail.extend_from_slice(data);
    }

    fn finish(&mut self) {
        let Some(tracker) = self.tracker.take() else {
            return;
        };
        if let Some(tokens) = observed_output_tokens(&self.tail) {
            tracker.complete(tokens);
        }
    }
}

impl http_body::Body for AdaptiveTrackingBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.append_tail(data);
                }
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                this.tracker.take();
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                this.finish();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Default)]
struct CacheDistributionStreamProof {
    decoder: SseDecoder,
    saw_usage: bool,
    saw_openai_finish: bool,
    terminal_success: bool,
    failed: bool,
}

impl CacheDistributionStreamProof {
    fn observe(&mut self, data: &[u8]) {
        if self.failed {
            return;
        }
        if self.decoder.push(data).is_err() {
            self.failed = true;
            return;
        }
        while let Some(frame) = self.decoder.next_frame() {
            match frame {
                Ok(frame) => self.observe_frame(&frame),
                Err(_) => self.failed = true,
            }
        }
        self.decoder.compact();
    }

    fn observe_frame(&mut self, frame: &SseFrame<'_>) {
        if frame.is_done() {
            self.terminal_success |= self.saw_usage && self.saw_openai_finish;
            return;
        }

        let Ok(value) = frame.decode_data::<serde_json::Value>() else {
            self.failed = true;
            return;
        };
        let event_type = frame
            .event_type
            .as_deref()
            .or_else(|| value.get("type").and_then(serde_json::Value::as_str));
        if stream_frame_is_error(event_type, &value) {
            self.failed = true;
            self.terminal_success = false;
            return;
        }

        let frame_usage = stream_output_tokens_from_value(&value).is_some();
        self.saw_usage |= frame_usage;
        self.saw_openai_finish |= value
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("finish_reason")
                        .is_some_and(|reason| !reason.is_null())
                })
            });

        let responses_completed = event_type == Some("response.completed")
            && value
                .pointer("/response/status")
                .and_then(serde_json::Value::as_str)
                == Some("completed")
            && frame_usage;
        let anthropic_completed = event_type == Some("message_stop") && self.saw_usage;
        let sglang_completed = value
            .pointer("/meta_info/finish_reason")
            .is_some_and(sglang_finish_reason_is_terminal)
            && frame_usage;
        self.terminal_success |= responses_completed || anthropic_completed || sglang_completed;
    }

    fn clean_terminal_success(&self) -> bool {
        !self.failed && self.decoder.buffered_len() == 0 && self.terminal_success
    }
}

fn stream_output_tokens_from_value(value: &serde_json::Value) -> Option<u32> {
    output_tokens_from_value(value)
        .or_else(|| value_u32(value.pointer("/response/usage/output_tokens")))
}

fn stream_frame_is_error(event_type: Option<&str>, value: &serde_json::Value) -> bool {
    event_type.is_some_and(|event| {
        event == "error" || event.ends_with(".error") || event.ends_with(".failed")
    }) || value.get("error").is_some_and(|error| !error.is_null())
        || matches!(
            value.get("type").and_then(serde_json::Value::as_str),
            Some("error" | "response.failed" | "response.incomplete")
        )
}

fn sglang_finish_reason_is_terminal(reason: &serde_json::Value) -> bool {
    let reason_type = reason
        .as_str()
        .or_else(|| reason.get("type").and_then(serde_json::Value::as_str));
    matches!(reason_type, Some("stop" | "length"))
}

fn buffered_completion_has_proof(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    if stream_frame_is_error(None, &value) || stream_output_tokens_from_value(&value).is_none() {
        return false;
    }

    let openai_completed = value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|choices| {
            !choices.is_empty()
                && choices.iter().all(|choice| {
                    choice
                        .get("finish_reason")
                        .is_some_and(|reason| !reason.is_null())
                })
        });
    let sglang_completed = ["/meta_info/finish_reason", "/finish_reason"]
        .into_iter()
        .any(|pointer| {
            value
                .pointer(pointer)
                .is_some_and(sglang_finish_reason_is_terminal)
        });
    let event_type = value.get("type").and_then(serde_json::Value::as_str);
    let responses_completed = if event_type == Some("response.completed") {
        value
            .pointer("/response/status")
            .and_then(serde_json::Value::as_str)
            == Some("completed")
    } else {
        value.get("status").and_then(serde_json::Value::as_str) == Some("completed")
    };
    let anthropic_completed = value.get("type").and_then(serde_json::Value::as_str)
        == Some("message")
        && value
            .get("stop_reason")
            .is_some_and(|reason| !reason.is_null());

    openai_completed || sglang_completed || responses_completed || anthropic_completed
}

/// Verify that a guarded streaming request has an endpoint-level contract for
/// terminal output usage before it is dispatched.
///
/// Native SGLang, Anthropic Messages, and OpenAI Responses streams carry usage
/// in their terminal protocol events. Chat Completions and legacy Completions
/// make the terminal usage chunk optional, so a distribution seed may use
/// those endpoints only when the exact outgoing request explicitly asks for
/// it. We inspect the prepared request, after worker-specific rewriting, and
/// fail closed if its in-memory JSON body cannot be proven.
fn validate_guarded_stream_usage_contract(
    route: &'static str,
    request: &ReqwestRequest,
) -> Result<(), &'static str> {
    match route {
        "/generate" | "/v1/messages" | "/v1/responses" => Ok(()),
        "/v1/chat/completions" | "/v1/completions" => {
            let body = request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .ok_or("guarded OpenAI stream body was not inspectable before dispatch")?;
            let value: serde_json::Value = serde_json::from_slice(body)
                .map_err(|_| "guarded OpenAI stream body was not valid JSON before dispatch")?;
            if value
                .pointer("/stream_options/include_usage")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                Ok(())
            } else {
                Err("guarded OpenAI streaming requires stream_options.include_usage=true")
            }
        }
        _ => Err("guarded streaming endpoint has no terminal usage contract"),
    }
}

/// Holds the exact cache-distribution proof and worker accounting through a
/// streaming response. Ownership is published only after a clean end-of-stream
/// with protocol-level terminal success and observed output usage.
struct CacheDistributionStreamingBody {
    inner: Body,
    commit: Box<dyn FnMut() + Send>,
    _load_guard: Option<WorkerLoadGuard>,
    proof: CacheDistributionStreamProof,
    commit_attempted: bool,
}

impl CacheDistributionStreamingBody {
    fn wrap_response(
        response: Response,
        load_guard: WorkerLoadGuard,
        commit: impl FnMut() + Send + 'static,
    ) -> Response {
        Self::wrap_response_inner(response, Some(load_guard), commit)
    }

    /// The ordinary send path already attached its load guard to the inner
    /// body. Keep only the provisional ownership lease in this outer proof.
    fn wrap_response_with_attached_guard(
        response: Response,
        commit: impl FnMut() + Send + 'static,
    ) -> Response {
        Self::wrap_response_inner(response, None, commit)
    }

    fn wrap_response_inner(
        response: Response,
        load_guard: Option<WorkerLoadGuard>,
        commit: impl FnMut() + Send + 'static,
    ) -> Response {
        let (parts, body) = response.into_parts();
        Response::from_parts(
            parts,
            Body::new(Self {
                inner: body,
                commit: Box::new(commit),
                _load_guard: load_guard,
                proof: CacheDistributionStreamProof::default(),
                commit_attempted: false,
            }),
        )
    }

    fn commit_if_complete(&mut self) {
        if !self.commit_attempted && self.proof.clean_terminal_success() {
            self.commit_attempted = true;
            (self.commit)();
        }
    }
}

impl http_body::Body for CacheDistributionStreamingBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.proof.observe(data);
                }
                if this.inner.is_end_stream() {
                    this.commit_if_complete();
                }
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                this.proof.failed = true;
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                this.commit_if_complete();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("worker_registry", &self.worker_registry)
            .field("policy_registry", &self.policy_registry)
            .field("client", &self.client)
            .field("retry_config", &self.retry_config)
            .finish_non_exhaustive()
    }
}

impl Router {
    /// Create a new router with injected policy and client
    #[expect(
        clippy::unused_async,
        reason = "async for API consistency with other router constructors"
    )]
    pub async fn new(ctx: &Arc<AppContext>) -> Result<Self, String> {
        Ok(Router {
            worker_registry: ctx.worker_registry.clone(),
            policy_registry: ctx.policy_registry.clone(),
            client: ctx.client.clone(),
            no_redirect_client: AppContextBuilder::build_worker_client(
                &ctx.router_config,
                ctx.router_config.request_timeout_secs,
                true,
            )?,
            retry_config: ctx.router_config.effective_retry_config(),
            realtime_registry: ctx.realtime_registry.clone(),
            webrtc_bind_addr: ctx.webrtc_bind_addr,
            webrtc_stun_server: ctx.webrtc_stun_server.clone(),
            adaptive_admission: ctx.adaptive_admission.clone(),
        })
    }

    fn select_first_worker(&self) -> Result<String, String> {
        let workers = self.worker_registry.get_all();
        let healthy_workers: Vec<_> = workers.iter().filter(|w| w.is_healthy()).collect();
        if healthy_workers.is_empty() {
            Err("No workers are available".to_string())
        } else {
            Ok(healthy_workers[0].url().to_string())
        }
    }

    async fn proxy_get_request(&self, req: Request<Body>, endpoint: &str) -> Response {
        let headers = header_utils::copy_request_headers(&req);

        match self.select_first_worker() {
            Ok(worker_url) => {
                let mut request_builder = self.client.get(format!("{worker_url}/{endpoint}"));
                for (name, value) in headers {
                    if header_utils::should_forward_request_header(&name) {
                        request_builder = request_builder.header(name, value);
                    }
                }

                match request_builder.send().await {
                    Ok(res) => {
                        let status = StatusCode::from_u16(res.status().as_u16())
                            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

                        // Preserve headers from backend
                        let response_headers =
                            header_utils::preserve_response_headers(res.headers());

                        match res.bytes().await {
                            Ok(body) => {
                                let mut response = Response::new(Body::from(body));
                                *response.status_mut() = status;
                                *response.headers_mut() = response_headers;
                                response
                            }
                            Err(e) => error::internal_error(
                                "read_response_failed",
                                format!("Failed to read response: {e}"),
                            ),
                        }
                    }
                    Err(e) => convert_reqwest_error(e),
                }
            }
            Err(e) => error::service_unavailable("no_workers", e),
        }
    }

    /// Select worker considering circuit breaker state.
    /// Filters to workers serving the specified model. When model is "unknown"
    /// (generate endpoint without model), considers all HTTP workers.
    fn select_worker_for_model(
        &self,
        model_id: &str,
        text: Option<&str>,
        headers: Option<&HeaderMap>,
        max_output_tokens: Option<u64>,
        owner_expansion_guard_partition: Option<&str>,
    ) -> WorkerSelectionResult {
        // UNKNOWN_MODEL_ID means caller didn't specify a model — find any available worker
        let model_filter = if model_id == crate::worker::UNKNOWN_MODEL_ID {
            None
        } else {
            Some(model_id)
        };
        let workers = self.worker_registry.get_workers_filtered(
            model_filter,
            Some(WorkerType::Regular),
            Some(ConnectionMode::Http),
            None,  // any runtime type
            false, // get all workers, we'll filter by is_available() next
        );

        let guarded_workers = owner_expansion_guard_partition
            .map(|partition| workers_in_admission_partition(&workers, partition));
        let observed_workers = guarded_workers.as_ref().unwrap_or(&workers);
        let available: Vec<Arc<dyn Worker>> = observed_workers
            .iter()
            .filter(|w| w.is_available())
            .cloned()
            .collect();
        if available.is_empty() {
            return if owner_expansion_guard_partition.is_some() {
                WorkerSelectionResult::UnleasedOwnerExpansionBlocked
            } else {
                WorkerSelectionResult::NoAvailable
            };
        }
        let forbid_unleased_cache_owner_expansion = owner_expansion_guard_partition.is_some();

        // Get the appropriate policy for this model
        let policy = self.policy_registry.get_policy_or_default(model_id);

        // Get cached hash ring for consistent hashing (O(log n) lookup)
        let hash_ring = self.worker_registry.get_hash_ring(model_id);

        let info = SelectWorkerInfo {
            request_text: text,
            tokens: None, // HTTP doesn't have tokens, use gRPC for PrefixHash
            headers,
            hash_ring,
            max_output_tokens,
            reserve_work: true,
            forbid_unleased_cache_owner_expansion,
            leg: crate::policies::WorkerLeg::Single,
        };
        let reservation_cost = policy.reservation_cost(&info);
        // In guarded mode the policy must see healthy circuit-open workers so
        // it can distinguish an unavailable recorded owner from a genuinely
        // cold prefix. Its own routing-state filter still prevents selecting
        // a worker that cannot execute.
        let selection_workers = if forbid_unleased_cache_owner_expansion {
            observed_workers
        } else {
            &available
        };
        let (idx, cold_bootstrap_route) = if let Some(partition) = owner_expansion_guard_partition {
            let Some((idx, route)) = self.policy_registry.begin_guarded_cache_route(
                &policy,
                model_id,
                selection_workers,
                &info,
                partition,
            ) else {
                return WorkerSelectionResult::UnleasedOwnerExpansionBlocked;
            };
            (idx, route)
        } else {
            let Some(idx) = self
                .policy_registry
                .select_worker(&policy, selection_workers, &info)
            else {
                return WorkerSelectionResult::NoAvailable;
            };
            (idx, None)
        };

        // Record worker selection metric (Layer 3)
        Metrics::record_worker_selection(
            metrics_labels::WORKER_REGULAR,
            metrics_labels::CONNECTION_HTTP,
            model_id,
            policy.name(),
        );

        WorkerSelectionResult::Selected(WorkerSelection {
            worker: selection_workers[idx].clone(),
            policy,
            reservation_cost,
            cold_bootstrap_route,
        })
    }

    /// Select a local, realtime-capable worker for the given model.
    ///
    /// Uses the shared [`WorkerSelector`] (least-loaded) filtered to regular
    /// HTTP workers advertising the `realtime` label, so realtime traffic
    /// never lands on a worker that can't serve it.
    async fn select_realtime_worker(
        &self,
        model_id: &str,
        headers: Option<&HeaderMap>,
    ) -> Result<Arc<dyn Worker>, Response> {
        WorkerSelector::new(&self.worker_registry, &self.client)
            .select_worker(&SelectWorkerRequest {
                model_id,
                headers,
                worker_type: Some(WorkerType::Regular),
                connection_mode: Some(ConnectionMode::Http),
                require_realtime_capable: true,
                ..Default::default()
            })
            .await
    }

    fn begin_http_admission<T: GenerationRequest + serde::Serialize>(
        &self,
        headers: Option<&HeaderMap>,
        typed_req: &T,
        route: &'static str,
        model_id: &str,
        text: &str,
    ) -> Result<HttpAdmission, Box<Response>> {
        let Some(controller) = self.adaptive_admission.clone() else {
            return Ok(HttpAdmission {
                tracker: None,
                distribution_partition: None,
                owner_expansion_guard_partition: None,
            });
        };
        if matches!(route, "/v1/embeddings" | "/v1/classify" | "/v1/rerank") {
            return Ok(HttpAdmission {
                tracker: None,
                distribution_partition: None,
                owner_expansion_guard_partition: None,
            });
        }
        let partition = trusted_header(headers, ADMISSION_PARTITION_HEADER)
            .unwrap_or_else(|| model_id.to_string());
        if model_id == crate::worker::UNKNOWN_MODEL_ID
            && controller.distribution_headroom_enabled(&partition)
        {
            return Err(Box::new(local_distribution_rejection(
                "distribution-enabled generation requests must name the model explicitly",
            )));
        }
        let tracker = controller.begin(
            partition.clone(),
            http_prediction_features(typed_req, headers, route, model_id, text),
        );
        let distribution_partition = tracker
            .distribution_headroom_partition()
            .map(str::to_string);
        let owner_expansion_guard_partition = (partition == model_id
            && controller.distribution_headroom_enabled(&partition))
        .then(|| partition.clone());
        if tracker.should_reject() && distribution_partition.is_none() {
            let mut response = error::create_error(
                StatusCode::TOO_MANY_REQUESTS,
                "adaptive_admission_saturated",
                "model fleet is temporarily saturated",
            );
            if let Ok(value) = HeaderValue::from_str(&tracker.retry_after_secs().max(1).to_string())
            {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
            response.extensions_mut().insert(LocalAdaptiveRejection);
            return Err(Box::new(response));
        }
        Ok(HttpAdmission {
            tracker: Some(tracker),
            distribution_partition,
            owner_expansion_guard_partition,
        })
    }

    pub async fn route_typed_request<T: GenerationRequest + serde::Serialize + Clone>(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        typed_req: &T,
        route: &'static str,
        model_id: &str,
    ) -> Response {
        let start = Instant::now();
        let is_stream = typed_req.is_stream();
        let text = typed_req.extract_text_for_routing();
        // Resolve once, here, so every registry, policy and metrics lookup
        // below is keyed by the canonical model ID. Only `get_by_model`
        // understands aliases; retry configs, hash rings and policies do not,
        // and an alias would silently fall back to router defaults.
        let canonical_model = self.worker_registry.resolve_model_alias(model_id);
        let model_id = canonical_model.as_deref().unwrap_or(model_id);
        let model = model_id;
        let endpoint = route_to_endpoint(route);
        let admission = match self.begin_http_admission(headers, typed_req, route, model_id, &text)
        {
            Ok(admission) => admission,
            Err(response) => return *response,
        };
        let HttpAdmission {
            tracker: adaptive_tracker,
            distribution_partition,
            owner_expansion_guard_partition,
        } = admission;

        // Record request start (Layer 2)
        Metrics::record_router_request(
            metrics_labels::ROUTER_HTTP,
            metrics_labels::BACKEND_REGULAR,
            metrics_labels::CONNECTION_HTTP,
            model,
            endpoint,
            bool_to_static_str(is_stream),
        );

        let mut response = if let Some(partition) = distribution_partition.as_deref() {
            let Some(tracker) = adaptive_tracker.as_ref() else {
                return local_distribution_rejection("adaptive request state was unavailable");
            };
            self.route_distribution_request_once(
                headers,
                tenant_meta,
                tracker,
                typed_req,
                route,
                model_id,
                canonical_model.as_deref(),
                is_stream,
                &text,
                partition,
            )
            .await
        } else {
            // Use per-model retry config if set by a worker, otherwise fall back to router default.
            let per_model_retry_config = self.worker_registry.get_retry_config(model_id);
            let retry_config = per_model_retry_config
                .as_ref()
                .unwrap_or(&self.retry_config);

            RetryExecutor::execute_response_with_retry(
                retry_config,
                // operation per attempt
                |_: u32| async {
                    let res = self
                        .route_typed_request_once(
                            headers,
                            owner_expansion_guard_partition.as_deref(),
                            typed_req,
                            route,
                            model_id,
                            canonical_model.as_deref(),
                            is_stream,
                            &text,
                        )
                        .await;

                    // Need to be outside `route_typed_request_once` because that function has multiple return paths
                    Metrics::record_router_upstream_response(
                        metrics_labels::ROUTER_HTTP,
                        res.status().as_u16(),
                        extract_error_code_from_response(&res),
                    );

                    res
                },
                // should_retry predicate
                |res, _attempt| {
                    should_retry_upstream_response(res, owner_expansion_guard_partition.is_some())
                },
                // on_backoff hook
                |delay, attempt| {
                    // Layer 3 worker metrics
                    Metrics::record_worker_retry(metrics_labels::WORKER_REGULAR, endpoint);
                    Metrics::record_worker_retry_backoff(attempt, delay);
                },
                // on_exhausted hook
                || {
                    Metrics::record_worker_retries_exhausted(
                        metrics_labels::WORKER_REGULAR,
                        endpoint,
                    );
                },
            )
            .await
        };

        if response.status().is_success() {
            let duration = start.elapsed();
            Metrics::record_router_duration(
                metrics_labels::ROUTER_HTTP,
                metrics_labels::BACKEND_REGULAR,
                metrics_labels::CONNECTION_HTTP,
                model,
                endpoint,
                duration,
            );
        } else if !is_retryable_status(response.status()) {
            Metrics::record_router_error(
                metrics_labels::ROUTER_HTTP,
                metrics_labels::BACKEND_REGULAR,
                metrics_labels::CONNECTION_HTTP,
                model,
                endpoint,
                error_type_from_status(response.status()),
            );
        }

        if let Some(tracker) = adaptive_tracker {
            if response.status().is_success() {
                response = AdaptiveTrackingBody::wrap_response(response, tracker);
            }
        }

        response
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "exact distribution route composes independent admission, cache, and HTTP proofs"
    )]
    async fn route_distribution_request_once<T: GenerationRequest + serde::Serialize + Clone>(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        tracker: &AdaptiveRequestTracker,
        typed_req: &T,
        route: &'static str,
        model_id: &str,
        canonical_model: Option<&str>,
        is_stream: bool,
        text: &str,
        partition: &str,
    ) -> Response {
        let Some(controller) = self.adaptive_admission.as_ref() else {
            return local_distribution_rejection("adaptive controller was unavailable");
        };
        let Some(authorization) = tenant_meta.extension::<RedeemedCapacityCreditAuthorization>()
        else {
            return local_distribution_rejection("a scheduler capacity credit is required");
        };
        if !authorization.matches(partition, model_id) {
            return local_distribution_rejection("capacity credit binding did not match the route");
        }

        let Some((mut headroom_permit, candidates)) =
            controller.try_acquire_distribution_headroom(partition, model_id)
        else {
            return local_distribution_rejection("fresh worker headroom was unavailable");
        };
        let allowed_targets: Vec<_> = candidates
            .into_iter()
            .map(|candidate| {
                (
                    candidate.worker_url,
                    candidate.worker_generation_id,
                    candidate.worker_revision,
                )
            })
            .collect();

        let model_filter = (model_id != crate::worker::UNKNOWN_MODEL_ID).then_some(model_id);
        let workers: Vec<_> = self
            .worker_registry
            .get_workers_filtered(
                model_filter,
                Some(WorkerType::Regular),
                Some(ConnectionMode::Http),
                None,
                false,
            )
            .into_iter()
            // Keep Ready but circuit-open cached owners in the ownership and
            // pressure proof. The policy separately requires the exact target
            // candidate to be executable.
            .filter(|worker| worker.is_healthy())
            // The adaptive headroom proof is partition-scoped. A private
            // reservation can share the model and cache prefix, but it must
            // neither suppress nor become a target for a public seed.
            .filter(|worker| worker_admission_partition(worker.as_ref()) == partition)
            .collect();
        if workers.is_empty() {
            return local_distribution_rejection("no routeable HTTP workers were available");
        }
        let hash_ring = self.worker_registry.get_hash_ring(model_id);
        let info = SelectWorkerInfo {
            request_text: Some(text),
            tokens: None,
            headers,
            hash_ring,
            max_output_tokens: typed_req.max_output_tokens_for_routing().map(u64::from),
            reserve_work: false,
            forbid_unleased_cache_owner_expansion: false,
            leg: crate::policies::WorkerLeg::Single,
        };
        let Some(mut cache_route) = self.policy_registry.begin_cache_distribution_route(
            model_id,
            &workers,
            &info,
            partition,
            &allowed_targets,
        ) else {
            return local_distribution_rejection("cache policy found no exact eligible route");
        };
        if cache_route.partition() != partition
            || !headroom_permit.bind_target(
                cache_route.target_url(),
                cache_route.target_generation_id(),
                cache_route.target_revision(),
            )
            || !cache_route.validate_before_dispatch(
                &self.policy_registry,
                model_id,
                &workers,
                &info,
            )
            || !headroom_permit.validate_for_dispatch()
        {
            return local_distribution_rejection("exact route validation changed before dispatch");
        }

        let worker = cache_route.worker();
        if worker.url() != cache_route.target_url()
            || worker.generation_id() != cache_route.target_generation_id()
            || worker.revision() != cache_route.target_revision()
        {
            return local_distribution_rejection("selected worker generation did not match proof");
        }
        let mut headers_with_trace = headers.cloned().unwrap_or_default();
        inject_trace_context_http(&mut headers_with_trace);
        let prepared_request = match self.prepare_typed_request(
            Some(&headers_with_trace),
            typed_req,
            route,
            canonical_model,
            worker.as_ref(),
        ) {
            Ok(request) => request,
            Err(mut response) => {
                // Serialization, worker preparation, and request building are
                // definite local pre-execution failures. Tell fair-share to
                // refund the provisional credit without rewriting the useful
                // client-facing error status.
                response.extensions_mut().insert(LocalAdaptiveRejection);
                return *response;
            }
        };
        if is_stream {
            if let Err(message) = validate_guarded_stream_usage_contract(route, &prepared_request) {
                // This is still a local pre-execution refusal. The scheduler
                // credit has not been claimed and both provisional headroom
                // permits are released when this function returns.
                return local_distribution_rejection(message);
            }
        }
        if !authorization.try_claim(partition, model_id) {
            return local_distribution_rejection(
                "capacity credit route claim was already consumed",
            );
        }
        if !cache_route.validate_before_dispatch(&self.policy_registry, model_id, &workers, &info)
            || !headroom_permit.validate_for_dispatch()
        {
            return local_distribution_rejection("exact route changed while claiming capacity");
        }
        let Some(load_guard) = cache_route.create_load_guard(headers) else {
            return local_distribution_rejection("exact target work reservation was unavailable");
        };
        // Tuple field drop order is intentional: clear the pending cache lease
        // before releasing exact work and worker load on every early return.
        let mut route_lifetime = (cache_route, load_guard);
        if worker.url() != route_lifetime.0.target_url()
            || worker.generation_id() != route_lifetime.0.target_generation_id()
            || worker.revision() != route_lifetime.0.target_revision()
            || !route_lifetime.0.validate_before_dispatch(
                &self.policy_registry,
                model_id,
                &workers,
                &info,
            )
        {
            return local_distribution_rejection(
                "exact worker generation changed immediately before dispatch",
            );
        }
        // Atomically revalidate and transfer the adaptive reservation into the
        // ordinary worker load accounting. Acquirers see at least one of the
        // two, and a telemetry epoch cannot change between proof and release.
        if !headroom_permit.try_transfer_after_worker_load_reserved() {
            return local_distribution_rejection(
                "exact worker headroom changed immediately before dispatch",
            );
        }
        if !tracker.authorize_distribution_headroom() {
            return local_distribution_rejection("adaptive route authorization was not valid");
        }

        Metrics::record_worker_selection(
            metrics_labels::WORKER_REGULAR,
            metrics_labels::CONNECTION_HTTP,
            model_id,
            "cache_aware_distribution",
        );
        events::RequestSentEvent { url: worker.url() }.emit();
        let mut response = self
            .send_prepared_typed_request(
                &self.no_redirect_client,
                prepared_request,
                route,
                worker.as_ref(),
                is_stream,
                None,
            )
            .await;
        events::RequestReceivedEvent {}.emit();

        let status = response.status();
        worker.record_outcome(status.as_u16());
        Metrics::record_router_upstream_response(
            metrics_labels::ROUTER_HTTP,
            status.as_u16(),
            extract_error_code_from_response(&response),
        );
        if status.is_server_error() {
            Metrics::record_worker_error(
                metrics_labels::WORKER_REGULAR,
                metrics_labels::CONNECTION_HTTP,
                error_type_from_status(status),
            );
        }

        if is_stream {
            if status.is_success() {
                let (mut cache_route, load_guard) = route_lifetime;
                let policy_registry = Arc::clone(&self.policy_registry);
                let worker_registry = Arc::clone(&self.worker_registry);
                let expected_worker = Arc::clone(&worker);
                let commit_model = model_id.to_string();
                let commit_text = text.to_string();
                let commit_partition = partition.to_string();
                let commit_worker_url = worker.url().to_string();
                response = CacheDistributionStreamingBody::wrap_response(
                    response,
                    load_guard,
                    move || {
                        if !commit_cache_owner_if_authoritative(
                            &worker_registry,
                            &expected_worker,
                            &commit_model,
                            &commit_partition,
                            |current_workers| {
                                cache_route.commit_after_success(
                                    &policy_registry,
                                    &commit_model,
                                    current_workers,
                                    &commit_text,
                                )
                            },
                        ) {
                            warn!(
                                model_id = commit_model,
                                partition = commit_partition,
                                worker_url = commit_worker_url,
                                "cache distribution route could not commit after terminal stream success"
                            );
                        }
                    },
                );
            } else {
                // The relay may remain live after response headers, even for a
                // non-success status. Retain both guards until its body ends or
                // the client disconnects, but never publish cache ownership.
                response = AttachedBody::wrap_response(response, route_lifetime);
            }
            return response;
        }

        if status.is_success() {
            let completion_proved = response
                .extensions()
                .get::<BufferedResponseBytes>()
                .is_some_and(|body| buffered_completion_has_proof(&body.0));
            if !completion_proved {
                // The backend accepted work, so preserve its response and do
                // not refund the consumed fair-share credit. The seed lease is
                // dropped without publishing ownership.
                warn!(
                    model_id,
                    partition,
                    worker_url = worker.url(),
                    "cache distribution route lacked non-streaming terminal usage proof"
                );
            } else if !commit_cache_owner_if_authoritative(
                &self.worker_registry,
                &worker,
                model_id,
                partition,
                |current_workers| {
                    route_lifetime.0.commit_after_success(
                        &self.policy_registry,
                        model_id,
                        current_workers,
                        text,
                    )
                },
            ) {
                warn!(
                    model_id,
                    partition,
                    worker_url = worker.url(),
                    "cache distribution route could not commit after proved backend completion"
                );
            }
        }

        drop(route_lifetime);
        response
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "per-attempt state threaded from route_typed_request; a struct would only move the arity"
    )]
    async fn route_typed_request_once<T: GenerationRequest + serde::Serialize + Clone>(
        &self,
        headers: Option<&HeaderMap>,
        owner_expansion_guard_partition: Option<&str>,
        typed_req: &T,
        route: &'static str,
        model_id: &str,
        canonical_model: Option<&str>,
        is_stream: bool,
        text: &str,
    ) -> Response {
        let selection = match self.select_worker_for_model(
            model_id,
            Some(text),
            headers,
            typed_req.max_output_tokens_for_routing().map(u64::from),
            owner_expansion_guard_partition,
        ) {
            WorkerSelectionResult::Selected(selection) => selection,
            WorkerSelectionResult::UnleasedOwnerExpansionBlocked => {
                return local_distribution_rejection(
                    "ordinary routing cannot create an unleased cache owner",
                );
            }
            WorkerSelectionResult::NoAvailable => {
                // Distinguish "no workers for this model" from "workers exist but unavailable"
                let model_filter = if model_id == crate::worker::UNKNOWN_MODEL_ID {
                    None
                } else {
                    Some(model_id)
                };
                let total = self.worker_registry.get_workers_filtered(
                    model_filter,
                    Some(WorkerType::Regular),
                    Some(ConnectionMode::Http),
                    None,
                    false,
                );
                return if total.is_empty() {
                    error::model_not_found(model_id)
                } else {
                    error::service_unavailable(
                        "no_available_workers",
                        "All workers are unavailable (circuit breaker open or unhealthy)",
                    )
                };
            }
        };
        let WorkerSelection {
            worker,
            policy,
            reservation_cost,
            cold_bootstrap_route,
        } = selection;

        let load_guard = if let Some(cost) = reservation_cost {
            Some(WorkerLoadGuard::with_policy_reservation(
                worker.clone(),
                headers,
                policy,
                cost,
            ))
        } else {
            ["cache_aware", "manual"]
                .contains(&policy.name())
                .then(|| WorkerLoadGuard::new(worker.clone(), headers))
        };

        // Note: Using borrowed reference avoids heap allocation
        events::RequestSentEvent { url: worker.url() }.emit();
        let mut headers_with_trace = headers.cloned().unwrap_or_default();
        inject_trace_context_http(&mut headers_with_trace);
        let headers = Some(&headers_with_trace);

        let mut response = if let Some(mut cold_route) = cold_bootstrap_route {
            let Some(partition) = owner_expansion_guard_partition else {
                return local_distribution_rejection(
                    "cold-bootstrap route lost its guarded partition",
                );
            };
            if worker.url() != cold_route.target_url()
                || worker.generation_id() != cold_route.target_generation_id()
                || worker.revision() != cold_route.target_revision()
                || !Arc::ptr_eq(&worker, &cold_route.worker())
            {
                return local_distribution_rejection(
                    "cold-bootstrap worker generation did not match its lease",
                );
            }
            let model_filter = (model_id != crate::worker::UNKNOWN_MODEL_ID).then_some(model_id);
            let current_workers = workers_in_admission_partition(
                &self.worker_registry.get_workers_filtered(
                    model_filter,
                    Some(WorkerType::Regular),
                    Some(ConnectionMode::Http),
                    None,
                    false,
                ),
                partition,
            );
            let hash_ring = self.worker_registry.get_hash_ring(model_id);
            let info = SelectWorkerInfo {
                request_text: Some(text),
                tokens: None,
                headers,
                hash_ring,
                max_output_tokens: typed_req.max_output_tokens_for_routing().map(u64::from),
                reserve_work: true,
                forbid_unleased_cache_owner_expansion: true,
                leg: crate::policies::WorkerLeg::Single,
            };
            let prepared_request = match self.prepare_typed_request(
                headers,
                typed_req,
                route,
                canonical_model,
                worker.as_ref(),
            ) {
                Ok(request) => request,
                Err(response) => return *response,
            };
            if is_stream {
                if let Err(message) =
                    validate_guarded_stream_usage_contract(route, &prepared_request)
                {
                    // No backend attempt has started. Dropping the provisional
                    // cold-owner lease and ordinary load guard makes a later
                    // request eligible to bootstrap the prefix again.
                    return local_distribution_rejection(message);
                }
            }
            if !cold_route.validate_before_dispatch(
                &self.policy_registry,
                model_id,
                &current_workers,
                &info,
            ) {
                return local_distribution_rejection(
                    "cold-bootstrap route changed immediately before dispatch",
                );
            }

            let mut response = self
                .send_prepared_typed_request(
                    &self.no_redirect_client,
                    prepared_request,
                    route,
                    worker.as_ref(),
                    is_stream,
                    load_guard,
                )
                .await;
            let status = response.status();
            if is_stream {
                if status.is_success() {
                    let policy_registry = Arc::clone(&self.policy_registry);
                    let worker_registry = Arc::clone(&self.worker_registry);
                    let expected_worker = Arc::clone(&worker);
                    let commit_model = model_id.to_string();
                    let commit_text = text.to_string();
                    let commit_partition = partition.to_string();
                    let commit_worker_url = worker.url().to_string();
                    response = CacheDistributionStreamingBody::wrap_response_with_attached_guard(
                        response,
                        move || {
                            if !commit_cache_owner_if_authoritative(
                                &worker_registry,
                                &expected_worker,
                                &commit_model,
                                &commit_partition,
                                |current_workers| {
                                    cold_route.commit_after_success(
                                        &policy_registry,
                                        &commit_model,
                                        current_workers,
                                        &commit_text,
                                    )
                                },
                            ) {
                                warn!(
                                    model_id = commit_model,
                                    partition = commit_partition,
                                    worker_url = commit_worker_url,
                                    "cold-bootstrap route could not commit after terminal stream success"
                                );
                            }
                        },
                    );
                }
            } else if status.is_success() {
                let completion_proved = response
                    .extensions()
                    .get::<BufferedResponseBytes>()
                    .is_some_and(|body| buffered_completion_has_proof(&body.0));
                if !completion_proved {
                    warn!(
                        model_id,
                        partition,
                        worker_url = worker.url(),
                        "cold-bootstrap route lacked non-streaming terminal usage proof"
                    );
                } else if !commit_cache_owner_if_authoritative(
                    &self.worker_registry,
                    &worker,
                    model_id,
                    partition,
                    |current_workers| {
                        cold_route.commit_after_success(
                            &self.policy_registry,
                            model_id,
                            current_workers,
                            text,
                        )
                    },
                ) {
                    warn!(
                        model_id,
                        partition,
                        worker_url = worker.url(),
                        "cold-bootstrap route could not commit after proved backend completion"
                    );
                }
            }
            response
        } else {
            self.send_typed_request(
                headers,
                typed_req,
                route,
                canonical_model,
                worker.as_ref(),
                is_stream,
                load_guard,
            )
            .await
        };

        events::RequestReceivedEvent {}.emit();

        let status = response.status();
        worker.record_outcome(status.as_u16());

        // Record worker errors for server errors (5xx)
        if status.is_server_error() {
            Metrics::record_worker_error(
                metrics_labels::WORKER_REGULAR,
                metrics_labels::CONNECTION_HTTP,
                error_type_from_status(status),
            );
        }

        response
    }

    // Generic simple routing for GET/POST without JSON body
    async fn route_simple_request(
        &self,
        headers: Option<&HeaderMap>,
        endpoint: &str,
        method: Method,
    ) -> Response {
        // TODO: currently the sglang worker is using in-memory state management, so this implementation has to fan out to all workers.
        // Eventually, we need to have router to manage the chat history with a proper database, will update this implementation accordingly.
        let workers = self.worker_registry.get_all();
        if workers.is_empty() {
            return error::service_unavailable("no_workers", "No available workers");
        }

        let filtered_headers: Vec<_> = headers
            .map(|hdrs| {
                hdrs.iter()
                    .filter(|(name, _)| header_utils::should_forward_request_header(name.as_str()))
                    .collect()
            })
            .unwrap_or_default();

        let futures: Vec<_> = workers
            .into_iter()
            .map(|worker| {
                let url = format!("{}/{}", worker.base_url(), endpoint);
                let client = self.client.clone();
                let method = method.clone();

                let headers = filtered_headers.clone();

                let api_key = worker.api_key().cloned();

                async move {
                    let mut request_builder = match method {
                        Method::GET => client.get(url),
                        Method::POST => client.post(url),
                        _ => {
                            return Err(error::method_not_allowed(
                                "unsupported_method",
                                "Unsupported method for simple routing",
                            ))
                        }
                    };

                    if let Some(key) = api_key {
                        let mut auth_header = String::with_capacity(7 + key.len());
                        auth_header.push_str("Bearer ");
                        auth_header.push_str(&key);
                        request_builder = request_builder.header("Authorization", auth_header);
                    }

                    for (name, value) in headers {
                        request_builder = request_builder.header(name.clone(), value.clone());
                    }

                    request_builder.send().await.map_err(convert_reqwest_error)
                }
            })
            .collect();

        // Now execute the collected futures concurrently
        let mut stream = stream::iter(futures).buffer_unordered(32);
        let mut last_response: Option<Response> = None;

        while let Some(result) = stream.next().await {
            match result {
                Ok(res) => {
                    let status = StatusCode::from_u16(res.status().as_u16())
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

                    let response_headers = header_utils::preserve_response_headers(res.headers());

                    match res.bytes().await {
                        Ok(body) => {
                            let mut response = Response::new(Body::from(body));
                            *response.status_mut() = status;
                            *response.headers_mut() = response_headers;

                            if status.is_success() {
                                return response;
                            }
                            last_response = Some(response);
                        }
                        Err(e) => {
                            last_response = Some(error::internal_error(
                                "read_response_failed",
                                format!("Failed to read response: {e}"),
                            ));
                        }
                    }
                }
                Err(e) => {
                    last_response = Some(e);
                }
            }
        }

        last_response
            .unwrap_or_else(|| error::bad_gateway("no_worker_response", "No worker response"))
    }

    // Route a POST request with empty body to a specific endpoint
    async fn route_post_empty_request(
        &self,
        headers: Option<&HeaderMap>,
        endpoint: &str,
    ) -> Response {
        self.route_simple_request(headers, endpoint, Method::POST)
            .await
    }

    /// Forward an audio transcription request to an audio-capable worker as
    /// `multipart/form-data`. Separate from `route_typed_request` because the
    /// endpoint is not JSON-bodied.
    async fn route_multipart_transcription(
        &self,
        headers: Option<&HeaderMap>,
        body: &TranscriptionRequest,
        audio: AudioFile,
        route: &'static str,
        model_id: &str,
    ) -> Response {
        let start = Instant::now();
        let is_stream = body.is_stream();
        let text = body.extract_text_for_routing();
        // Resolve once, here, for the same reason as `route_typed_request`:
        // only `get_by_model` understands aliases, so the policy and hash ring
        // lookups below would silently fall back to router defaults on an
        // alias. This path cannot reuse that resolution because multipart
        // never goes through `route_typed_request`.
        let canonical_model = self.worker_registry.resolve_model_alias(model_id);
        let model_id = canonical_model.as_deref().unwrap_or(model_id);
        let endpoint = route_to_endpoint(route);

        Metrics::record_router_request(
            metrics_labels::ROUTER_HTTP,
            metrics_labels::BACKEND_REGULAR,
            metrics_labels::CONNECTION_HTTP,
            model_id,
            endpoint,
            bool_to_static_str(is_stream),
        );

        // Finalize router metrics for an early error that never reached an
        // upstream worker (model_not_found, dp_aware_not_supported, no
        // available workers, build failure). Without this, pre-send failures
        // silently disappear from router_upstream_responses / router_error.
        let record_pre_send_error = |response: &Response| {
            let rstatus = response.status();
            Metrics::record_router_upstream_response(
                metrics_labels::ROUTER_HTTP,
                rstatus.as_u16(),
                extract_error_code_from_response(response),
            );
            if !is_retryable_status(rstatus) {
                Metrics::record_router_error(
                    metrics_labels::ROUTER_HTTP,
                    metrics_labels::BACKEND_REGULAR,
                    metrics_labels::CONNECTION_HTTP,
                    model_id,
                    endpoint,
                    error_type_from_status(rstatus),
                );
            }
        };

        // Multipart transcription can't route through `worker.prepare_request`,
        // which is the hook that injects `data_parallel_rank` for DP-aware
        // workers. Pre-filter DP-aware workers out of the candidate pool so
        // the policy can pick a non-DP worker when one exists; only fall back
        // to model_not_found / 400 when every candidate is DP-aware.
        let model_filter = if model_id == crate::worker::UNKNOWN_MODEL_ID {
            None
        } else {
            Some(model_id)
        };
        let all_workers = self.worker_registry.get_workers_filtered(
            model_filter,
            Some(WorkerType::Regular),
            Some(ConnectionMode::Http),
            None,
            false,
        );
        if all_workers.is_empty() {
            let resp = error::model_not_found(model_id);
            record_pre_send_error(&resp);
            return resp;
        }
        let non_dp_workers: Vec<Arc<dyn Worker>> = all_workers
            .iter()
            .filter(|w| !w.is_dp_aware())
            .cloned()
            .collect();
        if non_dp_workers.is_empty() {
            let resp = error::bad_request(
                "dp_aware_not_supported",
                "/v1/audio/transcriptions does not yet support DP-aware workers",
            );
            record_pre_send_error(&resp);
            return resp;
        }
        let available: Vec<Arc<dyn Worker>> = non_dp_workers
            .iter()
            .filter(|w| w.is_available())
            .cloned()
            .collect();
        if available.is_empty() {
            let resp = error::service_unavailable(
                "no_available_workers",
                "All workers are unavailable (circuit breaker open or unhealthy)",
            );
            record_pre_send_error(&resp);
            return resp;
        }

        let policy = self.policy_registry.get_policy_or_default(model_id);
        let hash_ring = self.worker_registry.get_hash_ring(model_id);
        let idx = match self.policy_registry.select_worker(
            &policy,
            &available,
            &SelectWorkerInfo {
                request_text: Some(&text),
                tokens: None,
                headers,
                hash_ring,
                max_output_tokens: None,
                reserve_work: false,
                forbid_unleased_cache_owner_expansion: false,
                leg: crate::policies::WorkerLeg::Single,
            },
        ) {
            Some(i) => i,
            None => {
                let resp = error::service_unavailable(
                    "no_available_workers",
                    "Policy returned no eligible worker",
                );
                record_pre_send_error(&resp);
                return resp;
            }
        };
        Metrics::record_worker_selection(
            metrics_labels::WORKER_REGULAR,
            metrics_labels::CONNECTION_HTTP,
            model_id,
            policy.name(),
        );
        let worker = available[idx].clone();

        let load_guard = ["cache_aware", "manual"]
            .contains(&policy.name())
            .then(|| WorkerLoadGuard::new(worker.clone(), headers));

        let mut headers_with_trace = headers.cloned().unwrap_or_default();
        inject_trace_context_http(&mut headers_with_trace);
        let headers = Some(&headers_with_trace);

        events::RequestSentEvent { url: worker.url() }.emit();

        let form = match build_transcription_form(body, audio, canonical_model.as_deref()) {
            Ok(f) => f,
            Err(e) => {
                let resp = error::bad_request("multipart_build_failed", e);
                record_pre_send_error(&resp);
                return resp;
            }
        };

        let endpoint_url = worker.endpoint_url(route);
        let mut request_builder = self.client.post(&endpoint_url).multipart(form);

        if let Some(key) = worker.api_key().cloned() {
            let mut auth_header = String::with_capacity(7 + key.len());
            auth_header.push_str("Bearer ");
            auth_header.push_str(&key);
            request_builder = request_builder.header("Authorization", auth_header);
        }

        if let Some(headers) = headers {
            for (name, value) in headers {
                // Skip Content-Type and Content-Length — reqwest sets the
                // correct multipart boundary itself.
                let name_str = name.as_str();
                if name_str.eq_ignore_ascii_case("content-type")
                    || name_str.eq_ignore_ascii_case("content-length")
                {
                    continue;
                }
                if header_utils::should_forward_request_header(name_str) {
                    request_builder = request_builder.header(name, value);
                }
            }
        }

        let res = match request_builder.send().await {
            Ok(res) => res,
            Err(e) => {
                error!(
                    "Failed to send multipart transcription request worker_url={} route={} error={}",
                    worker.url(),
                    route,
                    e
                );
                let err_resp = convert_reqwest_error(e);
                let err_status = err_resp.status();
                // Feed the synthetic status into the worker circuit breaker
                // and worker-error metric; transport failures (timeouts,
                // connect errors) must be visible to health tracking so the
                // same bad worker isn't picked repeatedly.
                worker.record_outcome(err_status.as_u16());
                if err_status.is_server_error() {
                    Metrics::record_worker_error(
                        metrics_labels::WORKER_REGULAR,
                        metrics_labels::CONNECTION_HTTP,
                        error_type_from_status(err_status),
                    );
                }
                Metrics::record_router_upstream_response(
                    metrics_labels::ROUTER_HTTP,
                    err_status.as_u16(),
                    extract_error_code_from_response(&err_resp),
                );
                // Mirror route_typed_request: a send failure must still bump
                // the terminal router_error counter, not just upstream_response.
                Metrics::record_router_error(
                    metrics_labels::ROUTER_HTTP,
                    metrics_labels::BACKEND_REGULAR,
                    metrics_labels::CONNECTION_HTTP,
                    model_id,
                    endpoint,
                    error_type_from_status(err_status),
                );
                return err_resp;
            }
        };

        let status = StatusCode::from_u16(res.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        Metrics::record_router_upstream_response(metrics_labels::ROUTER_HTTP, status.as_u16(), "");

        events::RequestReceivedEvent {}.emit();

        let response = if is_stream {
            // Preserve the upstream content-type verbatim. A `stream=true`
            // hint from the client doesn't guarantee the worker actually
            // streams — whisper backends may ignore it and return a normal
            // JSON body (success or 4xx error). Don't relabel non-SSE
            // responses as SSE; leave that judgment to whatever the worker
            // set.
            let response_headers = header_utils::preserve_response_headers(res.headers());
            let stream = res.bytes_stream();
            // Bounded channel applies backpressure: if the downstream client
            // is slow, the upstream relay awaits on `send` rather than piling
            // chunks in memory.
            const STREAM_RELAY_BUFFER: usize = 32;
            let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, String>>(STREAM_RELAY_BUFFER);
            // Attribute worker-level and router-level outcomes to the actual
            // stream completion from inside the relay task: a mid-stream error
            // after a 2xx header, or a non-streaming 5xx header returned under
            // `stream=true`, must be visible to circuit-breaker + worker-error
            // + router-error metrics. Recording only at header time would mis-
            // classify those.
            let worker_for_stream = worker.clone();
            let stream_header_status = status;
            let stream_model_id = model_id.to_string();
            let stream_endpoint = endpoint;
            let stream_start = start;
            #[expect(
                clippy::disallowed_methods,
                reason = "fire-and-forget stream relay; gateway shutdown need not wait for individual stream forwarding"
            )]
            tokio::spawn(async move {
                let mut stream = stream;
                let mut stream_failed = false;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if tx.send(Ok(bytes)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            stream_failed = true;
                            let _ = tx.send(Err(format!("Stream error: {e}"))).await;
                            break;
                        }
                    }
                }
                // Effective status = BAD_GATEWAY if the relay failed, else the
                // worker's header status. Covers both "5xx header returned
                // while stream=true" and "200 header then mid-stream break".
                let effective_status = if stream_failed {
                    StatusCode::BAD_GATEWAY
                } else {
                    stream_header_status
                };
                worker_for_stream.record_outcome(effective_status.as_u16());
                if effective_status.is_server_error() {
                    Metrics::record_worker_error(
                        metrics_labels::WORKER_REGULAR,
                        metrics_labels::CONNECTION_HTTP,
                        error_type_from_status(effective_status),
                    );
                }
                if effective_status.is_success() {
                    Metrics::record_router_duration(
                        metrics_labels::ROUTER_HTTP,
                        metrics_labels::BACKEND_REGULAR,
                        metrics_labels::CONNECTION_HTTP,
                        &stream_model_id,
                        stream_endpoint,
                        stream_start.elapsed(),
                    );
                } else {
                    Metrics::record_router_error(
                        metrics_labels::ROUTER_HTTP,
                        metrics_labels::BACKEND_REGULAR,
                        metrics_labels::CONNECTION_HTTP,
                        &stream_model_id,
                        stream_endpoint,
                        error_type_from_status(effective_status),
                    );
                }
            });
            let stream = ReceiverStream::new(rx);
            let body = Body::from_stream(stream);
            let mut response = Response::new(body);
            *response.status_mut() = status;
            *response.headers_mut() = response_headers;
            if let Some(guard) = load_guard {
                response = AttachedBody::wrap_response(response, guard);
            }
            response
        } else {
            let response_headers = header_utils::preserve_response_headers(res.headers());
            match res.bytes().await {
                Ok(body) => {
                    let mut response = Response::new(Body::from(body));
                    *response.status_mut() = status;
                    *response.headers_mut() = response_headers;
                    response
                }
                Err(e) => error::internal_error(
                    "read_response_body_failed",
                    format!("Failed to read response body: {e}"),
                ),
            }
        };

        // Non-streaming: classify metrics off the final response the client
        // will actually see. A body-read failure can rewrite a 2xx upstream
        // into a local 5xx, and we want the circuit breaker + metrics to see
        // that. Streaming outcomes are owned by the relay task above.
        if !is_stream {
            let final_status = response.status();
            worker.record_outcome(final_status.as_u16());
            if final_status.is_server_error() {
                Metrics::record_worker_error(
                    metrics_labels::WORKER_REGULAR,
                    metrics_labels::CONNECTION_HTTP,
                    error_type_from_status(final_status),
                );
            }
            if final_status.is_success() {
                Metrics::record_router_duration(
                    metrics_labels::ROUTER_HTTP,
                    metrics_labels::BACKEND_REGULAR,
                    metrics_labels::CONNECTION_HTTP,
                    model_id,
                    endpoint,
                    start.elapsed(),
                );
            } else {
                Metrics::record_router_error(
                    metrics_labels::ROUTER_HTTP,
                    metrics_labels::BACKEND_REGULAR,
                    metrics_labels::CONNECTION_HTTP,
                    model_id,
                    endpoint,
                    error_type_from_status(final_status),
                );
            }
        }

        response
    }

    // Send typed request directly without conversion.
    //
    // `canonical_model` is set only when the client addressed the model by an
    // alias. The worker was registered under the canonical ID and has never
    // heard of the alias, so the body it receives carries the canonical name.
    #[expect(
        clippy::too_many_arguments,
        reason = "per-request state threaded from route_typed_request_once; a struct would only move the arity"
    )]
    async fn send_typed_request<T: serde::Serialize>(
        &self,
        headers: Option<&HeaderMap>,
        typed_req: &T,
        route: &'static str,
        canonical_model: Option<&str>,
        worker: &dyn Worker,
        is_stream: bool,
        load_guard: Option<WorkerLoadGuard>,
    ) -> Response {
        let request =
            match self.prepare_typed_request(headers, typed_req, route, canonical_model, worker) {
                Ok(request) => request,
                Err(response) => return *response,
            };

        self.send_prepared_typed_request(
            &self.client,
            request,
            route,
            worker,
            is_stream,
            load_guard,
        )
        .await
    }

    fn prepare_typed_request<T: serde::Serialize>(
        &self,
        headers: Option<&HeaderMap>,
        typed_req: &T,
        route: &'static str,
        canonical_model: Option<&str>,
        worker: &dyn Worker,
    ) -> Result<ReqwestRequest, Box<Response>> {
        let api_key = worker.api_key().cloned();
        let endpoint_url = worker.endpoint_url(route);

        let mut json_val = serde_json::to_value(typed_req).map_err(|error| {
            Box::new(error::bad_request(
                "serialization_failed",
                format!("Convert into serde_json::Value failed: {error}"),
            ))
        })?;

        if let Some(canonical_model) = canonical_model {
            super::set_request_model(&mut json_val, canonical_model);
        }

        let mut json_val = worker.prepare_request(json_val).map_err(|error| {
            Box::new(error::bad_request(
                "request_preparation_failed",
                format!("Failed to prepare request: {error}"),
            ))
        })?;
        strip_default_sglang_fields(&mut json_val);

        let mut request_builder = self.client.post(&endpoint_url).json(&json_val);

        if let Some(key) = api_key {
            // Pre-allocate string with capacity to avoid reallocation
            let mut auth_header = String::with_capacity(7 + key.len());
            auth_header.push_str("Bearer ");
            auth_header.push_str(&key);
            request_builder = request_builder.header("Authorization", auth_header);
        }

        if let Some(headers) = headers {
            for (name, value) in headers {
                if header_utils::should_forward_request_header(name.as_str()) {
                    request_builder = request_builder.header(name, value);
                }
            }
        }

        request_builder
            .build()
            .map_err(|error| Box::new(convert_reqwest_error(error)))
    }

    async fn send_prepared_typed_request(
        &self,
        client: &Client,
        request: ReqwestRequest,
        route: &'static str,
        worker: &dyn Worker,
        is_stream: bool,
        load_guard: Option<WorkerLoadGuard>,
    ) -> Response {
        let res = match client.execute(request).await {
            Ok(res) => res,
            Err(e) => {
                error!(
                    "Failed to send typed request worker_url={} route={} error={}",
                    worker.url(),
                    route,
                    e
                );

                return convert_reqwest_error(e);
            }
        };

        let status = StatusCode::from_u16(res.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        if is_stream {
            // Preserve headers for streaming response
            let mut response_headers = header_utils::preserve_response_headers(res.headers());
            // Ensure we set the correct content-type for SSE
            response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));

            let stream = res.bytes_stream();
            let (tx, rx) = mpsc::unbounded_channel();

            // Spawn task to forward stream
            #[expect(
                clippy::disallowed_methods,
                reason = "fire-and-forget stream relay; gateway shutdown need not wait for individual stream forwarding"
            )]
            tokio::spawn(async move {
                let mut stream = stream;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if tx.send(Ok(bytes)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("Stream error: {e}")));
                            break;
                        }
                    }
                }
            });

            let stream = UnboundedReceiverStream::new(rx);
            let body = Body::from_stream(stream);

            let mut response = Response::new(body);
            *response.status_mut() = status;
            *response.headers_mut() = response_headers;

            // Attach load guard to response body for proper RAII lifecycle
            // Guard is dropped when response body is consumed or client disconnects
            if let Some(guard) = load_guard {
                response = AttachedBody::wrap_response(response, guard);
            }
            response
        } else {
            // For non-streaming requests, preserve headers
            let response_headers = header_utils::preserve_response_headers(res.headers());

            let response = match res.bytes().await {
                Ok(body) => {
                    let mut response = Response::new(Body::from(body.clone()));
                    *response.status_mut() = status;
                    *response.headers_mut() = response_headers;
                    response
                        .extensions_mut()
                        .insert(BufferedResponseBytes(body));
                    response
                }
                Err(e) => {
                    let error_msg = format!("Failed to get response body: {e}");
                    error::internal_error("read_response_body_failed", error_msg)
                }
            };

            // load_guard dropped here automatically after response body is read
            response
        }
    }

    /// Build the public rerank response.
    ///
    /// Rerank is the one HTTP route whose response the gateway constructs
    /// itself instead of passing the worker's through, so the model it reports
    /// has to be canonicalized here. `canonical_model` is set only when the
    /// client addressed the model by an alias; reporting the alias would make
    /// this route disagree with every other one about which model ran.
    async fn build_rerank_response(
        req: &RerankRequest,
        canonical_model: Option<&str>,
        response: Response,
    ) -> anyhow::Result<Response> {
        let (_, response_body) = response.into_parts();
        let body_bytes = to_bytes(response_body, usize::MAX).await?;
        let rerank_results = serde_json::from_slice::<Vec<RerankResult>>(&body_bytes)?;
        let model = canonical_model.map_or_else(|| req.model.clone(), ToOwned::to_owned);
        let mut rerank_response = RerankResponse::new(rerank_results, model, req.rid.clone());
        // Sorting is handled by Python worker (serving_rerank.py)
        if let Some(top_k) = req.top_k {
            rerank_response.apply_top_k(top_k);
        }
        if !req.return_documents {
            rerank_response.drop_documents();
        }
        Ok(Json(rerank_response).into_response())
    }
}

/// Build the multipart body forwarded to the worker.
///
/// `canonical_model` is set only when the client addressed the model by an
/// alias. The worker was registered under the canonical ID and has never heard
/// of the alias, so that is the name the form carries.
fn build_transcription_form(
    body: &TranscriptionRequest,
    audio: AudioFile,
    canonical_model: Option<&str>,
) -> Result<Form, String> {
    let AudioFile {
        bytes,
        file_name,
        content_type,
    } = audio;

    // Wrap the already-buffered Bytes in a reqwest Body (Arc refcount, no
    // additional copy) instead of Part::bytes, which would force a Vec copy.
    let file_len = bytes.len() as u64;
    let mut file_part =
        Part::stream_with_length(reqwest::Body::from(bytes), file_len).file_name(file_name);
    if let Some(ct) = content_type.as_deref() {
        file_part = file_part
            .mime_str(ct)
            .map_err(|e| format!("Invalid audio content-type '{ct}': {e}"))?;
    }

    let mut form = Form::new().part("file", file_part).text(
        "model",
        canonical_model.map_or_else(|| body.model.clone(), ToOwned::to_owned),
    );

    if let Some(ref language) = body.language {
        form = form.text("language", language.clone());
    }
    if let Some(ref prompt) = body.prompt {
        form = form.text("prompt", prompt.clone());
    }
    if let Some(ref fmt) = body.response_format {
        form = form.text("response_format", fmt.clone());
    }
    if let Some(temp) = body.temperature {
        form = form.text("temperature", temp.to_string());
    }
    if let Some(ref grans) = body.timestamp_granularities {
        for g in grans {
            form = form.text("timestamp_granularities[]", g.clone());
        }
    }
    if let Some(stream) = body.stream {
        form = form.text("stream", stream.to_string());
    }

    Ok(form)
}

fn convert_reqwest_error(e: reqwest::Error) -> Response {
    let url = e
        .url()
        .map(|u| u.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let message = format!("{e}. URL: {url}");

    // TODO improve error status code
    let (status, code) = if let Some(upstream_status) = e.status() {
        (upstream_status, "call_upstream_status_error")
    } else if e.is_builder() {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "call_upstream_builder_error",
        )
    } else if e.is_request() {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "call_upstream_request_error",
        )
    } else if e.is_redirect() {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "call_upstream_redirect_error",
        )
    } else if e.is_body() {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "call_upstream_body_error",
        )
    } else if e.is_decode() {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "call_upstream_decode_error",
        )
    } else if e.is_timeout() {
        (StatusCode::GATEWAY_TIMEOUT, "call_upstream_timeout")
    } else if e.is_connect() {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "call_upstream_connection_failed",
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "call_upstream_request_failed",
        )
    };

    error::create_error(status, code, message)
}

use async_trait::async_trait;

#[async_trait]
impl RouterTrait for Router {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn health_generate(&self, req: Request<Body>) -> Response {
        self.proxy_get_request(req, "health_generate").await
    }

    async fn get_server_info(&self, req: Request<Body>) -> Response {
        self.proxy_get_request(req, "get_server_info").await
    }

    async fn get_model_info(&self, req: Request<Body>) -> Response {
        self.proxy_get_request(req, "get_model_info").await
    }

    async fn route_generate(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &GenerateRequest,
        model_id: &str,
    ) -> Response {
        self.route_typed_request(headers, tenant_meta, body, "/generate", model_id)
            .await
    }

    async fn route_chat(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &ChatCompletionRequest,
        model_id: &str,
    ) -> Response {
        self.route_typed_request(headers, tenant_meta, body, "/v1/chat/completions", model_id)
            .await
    }

    async fn route_messages(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &CreateMessageRequest,
        model_id: &str,
    ) -> Response {
        self.route_typed_request(headers, tenant_meta, body, "/v1/messages", model_id)
            .await
    }

    async fn route_completion(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &CompletionRequest,
        model_id: &str,
    ) -> Response {
        self.route_typed_request(headers, tenant_meta, body, "/v1/completions", model_id)
            .await
    }

    async fn route_responses(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &ResponsesRequest,
        model_id: &str,
    ) -> Response {
        self.route_typed_request(headers, tenant_meta, body, "/v1/responses", model_id)
            .await
    }

    async fn cancel_response(&self, headers: Option<&HeaderMap>, response_id: &str) -> Response {
        let endpoint = format!("v1/responses/{response_id}/cancel");
        self.route_post_empty_request(headers, &endpoint).await
    }

    async fn route_embeddings(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &EmbeddingRequest,
        model_id: &str,
    ) -> Response {
        self.route_typed_request(headers, tenant_meta, body, "/v1/embeddings", model_id)
            .await
    }

    async fn route_classify(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &ClassifyRequest,
        model_id: &str,
    ) -> Response {
        self.route_typed_request(headers, tenant_meta, body, "/v1/classify", model_id)
            .await
    }

    async fn route_audio_transcriptions(
        &self,
        headers: Option<&HeaderMap>,
        _tenant_meta: &TenantRequestMeta,
        body: &TranscriptionRequest,
        audio: AudioFile,
        model_id: &str,
    ) -> Response {
        self.route_multipart_transcription(
            headers,
            body,
            audio,
            "/v1/audio/transcriptions",
            model_id,
        )
        .await
    }

    async fn route_rerank(
        &self,
        headers: Option<&HeaderMap>,
        tenant_meta: &TenantRequestMeta,
        body: &RerankRequest,
        model_id: &str,
    ) -> Response {
        let canonical_model = self.worker_registry.resolve_model_alias(model_id);
        let response = self
            .route_typed_request(headers, tenant_meta, body, "/v1/rerank", model_id)
            .await;
        if response.status().is_success() {
            match Self::build_rerank_response(body, canonical_model.as_deref(), response).await {
                Ok(rerank_response) => rerank_response,
                Err(e) => {
                    error!("Failed to build rerank response: {}", e);
                    return error::internal_error(
                        "rerank_response_build_failed",
                        "Failed to build rerank response",
                    );
                }
            }
        } else {
            response
        }
    }

    async fn route_realtime_session(
        &self,
        headers: Option<&HeaderMap>,
        body: &RealtimeSessionCreateRequest,
    ) -> Response {
        let model = body.model.as_deref().unwrap_or_default();
        let worker = self.select_realtime_worker(model, headers).await;
        forward_realtime_rest(
            RealtimeLabels::HTTP,
            &self.client,
            worker,
            headers,
            body,
            model,
            "/v1/realtime/sessions",
            metrics_labels::ENDPOINT_REALTIME_SESSIONS,
        )
        .await
    }

    async fn route_realtime_client_secret(
        &self,
        headers: Option<&HeaderMap>,
        body: &RealtimeClientSecretCreateRequest,
    ) -> Response {
        let model = body.session.model.as_deref().unwrap_or_default();
        let worker = self.select_realtime_worker(model, headers).await;
        forward_realtime_rest(
            RealtimeLabels::HTTP,
            &self.client,
            worker,
            headers,
            body,
            model,
            "/v1/realtime/client_secrets",
            metrics_labels::ENDPOINT_REALTIME_CLIENT_SECRETS,
        )
        .await
    }

    async fn route_realtime_transcription_session(
        &self,
        headers: Option<&HeaderMap>,
        body: &RealtimeTranscriptionSessionCreateRequest,
    ) -> Response {
        let model = body.model.as_deref().unwrap_or_default();
        let worker = self.select_realtime_worker(model, headers).await;
        forward_realtime_rest(
            RealtimeLabels::HTTP,
            &self.client,
            worker,
            headers,
            body,
            model,
            "/v1/realtime/transcription_sessions",
            metrics_labels::ENDPOINT_REALTIME_TRANSCRIPTION,
        )
        .await
    }

    async fn route_realtime_ws(&self, req: Request<Body>, model: &str) -> Response {
        let (parts, _body) = req.into_parts();

        Metrics::record_router_request(
            metrics_labels::ROUTER_HTTP,
            metrics_labels::BACKEND_REGULAR,
            metrics_labels::CONNECTION_WEBSOCKET,
            model,
            metrics_labels::ENDPOINT_REALTIME,
            "false",
        );

        let auth_header = header_utils::extract_auth_header(Some(&parts.headers), None);
        let worker = self
            .select_realtime_worker(model, Some(&parts.headers))
            .await;

        handle_realtime_ws(
            RealtimeLabels::HTTP,
            parts,
            model.to_owned(),
            worker,
            auth_header,
            Arc::clone(&self.realtime_registry),
        )
        .await
    }

    async fn route_realtime_webrtc(&self, req: Request<Body>, model: &str) -> Response {
        let (parts, body) = req.into_parts();
        let body = match to_bytes(body, WEBRTC_REQUEST_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => {
                if e.source()
                    .and_then(|s| s.downcast_ref::<http_body_util::LengthLimitError>())
                    .is_some()
                {
                    return StatusCode::PAYLOAD_TOO_LARGE.into_response();
                }
                return error::bad_request("invalid_body", format!("Failed to read body: {e}"));
            }
        };

        let parsed = match webrtc::parse_webrtc_request(&parts, &body, model).await {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        Metrics::record_router_request(
            metrics_labels::ROUTER_HTTP,
            metrics_labels::BACKEND_REGULAR,
            metrics_labels::CONNECTION_WEBRTC,
            &parsed.model,
            metrics_labels::ENDPOINT_REALTIME,
            "false",
        );

        let auth_header = header_utils::extract_auth_header(Some(&parts.headers), None);
        let worker = self
            .select_realtime_worker(&parsed.model, Some(&parts.headers))
            .await;

        let bind_addr = self
            .webrtc_bind_addr
            .unwrap_or_else(|| std::net::Ipv4Addr::UNSPECIFIED.into());

        handle_realtime_webrtc(
            RealtimeLabels::HTTP,
            parts.headers,
            parsed,
            worker,
            auth_header,
            self.client.clone(),
            bind_addr,
            self.webrtc_stun_server.clone(),
            Arc::clone(&self.realtime_registry),
        )
        .await
    }

    fn router_type(&self) -> &'static str {
        "regular"
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
        time::Instant,
    };

    use http_body_util::BodyExt;
    use openai_protocol::{
        chat::{ChatMessage, MessageContent},
        common::StreamOptions,
        model_card::ModelCard,
        worker::{HealthCheckConfig, SchedulerLoadSnapshot, WorkerLoadResponse},
    };
    use serde::Serialize;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::watch,
    };

    use super::*;
    use crate::{
        config::{
            AdaptiveAdmissionConfig, AdaptiveAdmissionMode, AdaptiveAdmissionStrategy, PolicyConfig,
        },
        middleware::scheduler::capacity_credit::CapacityCreditBinding,
        policies::{CacheAwarePolicy, SizeAwarePowerOfTwoPolicy},
        tenant::TenantKey,
        worker::{
            monitor::{ObservedWorkerLoad, WorkerLoadSource},
            BasicWorkerBuilder,
        },
    };

    #[derive(Clone, Serialize)]
    struct TestGenerationRequest {
        stream: bool,
        n: u32,
        max_tokens: u32,
        tools: Vec<serde_json::Value>,
        response_format: serde_json::Value,
        reasoning_effort: String,
    }

    impl GenerationRequest for TestGenerationRequest {
        fn is_stream(&self) -> bool {
            self.stream
        }

        fn get_model(&self) -> Option<&str> {
            Some("test-model")
        }

        fn extract_text_for_routing(&self) -> String {
            "abcdefgh".to_string()
        }

        fn max_output_tokens_for_routing(&self) -> Option<u32> {
            Some(self.max_tokens)
        }
    }

    fn no_health_check() -> HealthCheckConfig {
        HealthCheckConfig {
            disable_health_check: true,
            ..Default::default()
        }
    }

    #[test]
    fn distribution_worker_scope_excludes_other_admission_partitions() {
        let public: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://public:8080")
                .model(ModelCard::new("test-model"))
                .label(ADMISSION_PARTITION_LABEL, "test-model")
                .health_config(no_health_check())
                .build(),
        );
        let private: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://private:8080")
                .model(ModelCard::new("test-model"))
                .label(ADMISSION_PARTITION_LABEL, "private")
                .health_config(no_health_check())
                .build(),
        );
        let fallback: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://fallback:8080")
                .model(ModelCard::new("test-model"))
                .health_config(no_health_check())
                .build(),
        );

        let scoped: Vec<_> =
            workers_in_admission_partition(&[public, private, fallback], "test-model")
                .into_iter()
                .map(|worker| worker.url().to_string())
                .collect();

        assert_eq!(scoped, ["http://public:8080", "http://fallback:8080"]);
    }

    #[test]
    fn guarded_selection_never_falls_through_to_private_partition() {
        let public: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://public:8080")
                .model(ModelCard::new("test-model"))
                .label(ADMISSION_PARTITION_LABEL, "test-model")
                .health_config(no_health_check())
                .build(),
        );
        for _ in 0..5 {
            public.record_outcome(503);
        }
        assert!(public.is_healthy());
        assert!(!public.is_available());
        let private: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://private:8080")
                .model(ModelCard::new("test-model"))
                .label(ADMISSION_PARTITION_LABEL, "private")
                .health_config(no_health_check())
                .build(),
        );
        let worker_registry = Arc::new(WorkerRegistry::new());
        worker_registry.register_or_replace(public);
        worker_registry.register_or_replace(private);
        let router = Router {
            worker_registry,
            policy_registry: Arc::new(PolicyRegistry::new(PolicyConfig::RoundRobin)),
            client: Client::new(),
            no_redirect_client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            retry_config: RetryConfig::default(),
            realtime_registry: Arc::new(RealtimeRegistry::new()),
            webrtc_bind_addr: None,
            webrtc_stun_server: None,
            adaptive_admission: None,
        };

        assert!(matches!(
            router.select_worker_for_model(
                "test-model",
                Some("shared prefix"),
                None,
                Some(1_000),
                Some("test-model"),
            ),
            WorkerSelectionResult::UnleasedOwnerExpansionBlocked
        ));
    }

    #[test]
    fn local_distribution_rejection_is_never_retried_internally() {
        let local = local_distribution_rejection("blocked before dispatch");
        assert_eq!(local.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!should_retry_upstream_response(&local, false));

        let upstream = error::create_error(
            StatusCode::TOO_MANY_REQUESTS,
            "upstream_busy",
            "retry another worker",
        );
        assert!(should_retry_upstream_response(&upstream, false));
        assert!(!should_retry_upstream_response(&upstream, true));
    }

    #[test]
    fn test_http_prediction_features_use_trusted_headers_and_request_shape() {
        let request = TestGenerationRequest {
            stream: true,
            n: 3,
            max_tokens: 100,
            tools: vec![serde_json::json!({"type": "function"})],
            response_format: serde_json::json!({"type": "json_object"}),
            reasoning_effort: "high".to_string(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(COMET_USER_HEADER, HeaderValue::from_static("junu.kim"));
        headers.insert(
            COMET_WORKLOAD_TYPE_HEADER,
            HeaderValue::from_static("batch-eval"),
        );

        let features = http_prediction_features(
            &request,
            Some(&headers),
            "/v1/chat/completions",
            "test-model",
            "abcdefgh",
        );

        assert_eq!(features.model, "test-model");
        assert_eq!(features.user, "junu.kim");
        assert_eq!(features.workload_type, "batch-eval");
        assert_eq!(features.endpoint, metrics_labels::ENDPOINT_CHAT);
        assert_eq!(features.prompt_tokens, 2);
        assert_eq!(features.max_output_tokens, Some(300));
        assert_ne!(features.generation_flags & FLAG_MULTIPLE_COMPLETIONS, 0);
        assert_ne!(features.generation_flags & FLAG_TOOLS, 0);
        assert_ne!(features.generation_flags & FLAG_STRUCTURED_OUTPUT, 0);
        assert_ne!(features.generation_flags & FLAG_REASONING, 0);
        assert_ne!(features.generation_flags & FLAG_STREAMING, 0);
    }

    #[test]
    fn test_observed_output_tokens_support_json_and_terminal_sse_usage() {
        assert_eq!(
            observed_output_tokens(br#"{"usage":{"completion_tokens":42}}"#),
            Some(42)
        );
        assert_eq!(
            observed_output_tokens(br#"{"usage":{"output_tokens":13}}"#),
            Some(13)
        );
        assert_eq!(
            observed_output_tokens(
                b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"completion_tokens\":17}}\n\ndata: [DONE]\n\n"
            ),
            Some(17)
        );
        assert_eq!(
            observed_output_tokens(br#"{"usage":{"completion_tokens":0}}"#),
            Some(0)
        );
        assert_eq!(observed_output_tokens(br#"{"choices":[]}"#), None);

        let large_response = format!(
            "{{\"choices\":[{{\"text\":\"{}\"}}],\"usage\":{{\"completion_tokens\":29}}}}",
            "x".repeat(ADAPTIVE_RESPONSE_TAIL_LIMIT)
        );
        let tail =
            &large_response.as_bytes()[large_response.len() - ADAPTIVE_RESPONSE_TAIL_LIMIT..];
        assert_eq!(observed_output_tokens(tail), Some(29));
    }

    #[test]
    fn buffered_distribution_commit_requires_terminal_usage_proof() {
        for body in [
            br#"{"choices":[{"finish_reason":"stop"}],"usage":{"completion_tokens":7}}"#.as_slice(),
            br#"{"text":"ok","meta_info":{"completion_tokens":7,"finish_reason":{"type":"stop"}}}"#
                .as_slice(),
            br#"{"status":"completed","usage":{"output_tokens":7}}"#.as_slice(),
            br#"{"type":"message","stop_reason":"end_turn","usage":{"output_tokens":7}}"#
                .as_slice(),
        ] {
            assert!(buffered_completion_has_proof(body));
        }

        for body in [
            br#"{"choices":[{"finish_reason":null}],"usage":{"completion_tokens":7}}"#.as_slice(),
            br#"{"choices":[{"finish_reason":"stop"}]}"#.as_slice(),
            br#"{"text":"partial","meta_info":{"completion_tokens":7,"finish_reason":{"type":"abort"}}}"#
                .as_slice(),
            br#"{"type":"response.completed","response":{"status":"failed","usage":{"output_tokens":7}}}"#
                .as_slice(),
            br#"{"type":"response.completed","response":{"usage":{"output_tokens":7}}}"#
                .as_slice(),
            br#"{"error":{"message":"OOM"},"usage":{"completion_tokens":7}}"#.as_slice(),
            br#"{"status":"incomplete","usage":{"output_tokens":7}}"#.as_slice(),
            br#"not-json"#.as_slice(),
            b"".as_slice(),
        ] {
            assert!(!buffered_completion_has_proof(body));
        }
    }

    fn tracked_stream_response(
        worker: &Arc<dyn Worker>,
        body: Body,
        commits: &Arc<AtomicUsize>,
    ) -> Response {
        let commits_for_body = Arc::clone(commits);
        CacheDistributionStreamingBody::wrap_response(
            Response::new(body),
            WorkerLoadGuard::new(Arc::clone(worker), None),
            move || {
                commits_for_body.fetch_add(1, Ordering::SeqCst);
            },
        )
    }

    fn test_stream_worker() -> Arc<dyn Worker> {
        Arc::new(
            BasicWorkerBuilder::new("http://worker:8080")
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        )
    }

    #[tokio::test]
    async fn cache_distribution_stream_commits_only_after_clean_terminal_success() {
        let worker: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://worker:8080")
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        );
        let reservation_policy = Arc::new(SizeAwarePowerOfTwoPolicy::new(1_000));
        let reservation_info = SelectWorkerInfo {
            request_text: Some("x"),
            max_output_tokens: Some(500),
            ..Default::default()
        };
        let reservation_cost =
            reservation_policy.reserve_exact_worker(worker.url(), &reservation_info);
        let load_guard = WorkerLoadGuard::with_policy_reservation(
            Arc::clone(&worker),
            None,
            reservation_policy.clone(),
            reservation_cost,
        );
        let commits = Arc::new(AtomicUsize::new(0));
        let commits_for_body = Arc::clone(&commits);
        let backend_stream = stream::iter([
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
            )),
            Ok(bytes::Bytes::from_static(
                b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":17}}\n\ndata: [DONE]\n\n",
            )),
        ]);
        let response = CacheDistributionStreamingBody::wrap_response(
            Response::new(Body::from_stream(backend_stream)),
            load_guard,
            move || {
                commits_for_body.fetch_add(1, Ordering::SeqCst);
            },
        );

        assert_eq!(worker.load(), 1);
        assert_eq!(
            reservation_policy.reserved_for(worker.url()),
            reservation_cost
        );
        assert_eq!(commits.load(Ordering::SeqCst), 0);

        let mut body = response.into_body();
        assert!(body.frame().await.expect("first frame").is_ok());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert!(body.frame().await.expect("terminal frame").is_ok());
        assert!(body.frame().await.is_none());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        drop(body);
        assert_eq!(worker.load(), 0);
        assert_eq!(reservation_policy.reserved_for(worker.url()), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_rejects_2xx_in_band_error() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let response = tracked_stream_response(
            &worker,
            Body::from("data: {\"error\":{\"message\":\"OOM\"}}\n\n"),
            &commits,
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!body.is_empty());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_ignores_comment_heartbeat() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let response = tracked_stream_response(
            &worker,
            Body::from(": keep-alive\n\nevent: ping\n\n"),
            &commits,
        );
        assert!(to_bytes(response.into_body(), usize::MAX).await.is_ok());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_rejects_truncated_terminal_event() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let response = tracked_stream_response(
            &worker,
            Body::from(
                "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                 data: {\"choices\":[],\"usage\":{\"completion_tokens\":17}}",
            ),
            &commits,
        );
        assert!(to_bytes(response.into_body(), usize::MAX).await.is_ok());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_rejects_client_cancellation_after_terminal_frame() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let terminal = Ok::<_, std::io::Error>(bytes::Bytes::from_static(
            b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
              data: {\"choices\":[],\"usage\":{\"completion_tokens\":17}}\n\n\
              data: [DONE]\n\n",
        ));
        let backend_stream = stream::iter([terminal])
            .chain(stream::pending::<Result<bytes::Bytes, std::io::Error>>());
        let response =
            tracked_stream_response(&worker, Body::from_stream(backend_stream), &commits);
        let mut body = response.into_body();
        assert!(body.frame().await.expect("terminal data frame").is_ok());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        drop(body);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_accepts_sglang_terminal_usage() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let response = tracked_stream_response(
            &worker,
            Body::from(
                "data: {\"text\":\"hello\",\"meta_info\":{\"completion_tokens\":5,\
                 \"finish_reason\":{\"type\":\"stop\"}}}\n\ndata: [DONE]\n\n",
            ),
            &commits,
        );
        assert!(to_bytes(response.into_body(), usize::MAX).await.is_ok());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_accepts_responses_completed_usage() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let response = tracked_stream_response(
            &worker,
            Body::from(
                "event: response.completed\n\
                 data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\
                 \"usage\":{\"output_tokens\":11}}}\n\n",
            ),
            &commits,
        );
        let mut body = response.into_body();

        assert_eq!(worker.load(), 1);
        assert!(body
            .frame()
            .await
            .expect("responses terminal frame")
            .is_ok());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(body.frame().await.is_none());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(body.frame().await.is_none());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        drop(body);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_accepts_anthropic_usage_then_message_stop() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let response = tracked_stream_response(
            &worker,
            Body::from(
                "event: message_delta\n\
                 data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n\
                 event: message_stop\n\
                 data: {\"type\":\"message_stop\"}\n\n",
            ),
            &commits,
        );
        let mut body = response.into_body();

        assert_eq!(worker.load(), 1);
        assert!(body
            .frame()
            .await
            .expect("anthropic terminal frames")
            .is_ok());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(body.frame().await.is_none());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(body.frame().await.is_none());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        drop(body);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn cache_distribution_stream_body_error_never_commits() {
        let worker = test_stream_worker();
        let commits = Arc::new(AtomicUsize::new(0));
        let error_stream = stream::iter([Err::<bytes::Bytes, std::io::Error>(
            std::io::Error::other("backend stream failed"),
        )]);
        let response = tracked_stream_response(&worker, Body::from_stream(error_stream), &commits);
        assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(worker.load(), 0);
    }

    #[tokio::test]
    async fn non_success_stream_holds_attached_state_until_body_drop() {
        struct DropSignal(Arc<AtomicUsize>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let worker: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://worker:8080")
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        );
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut response = Response::new(Body::from("unavailable"));
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        let response = AttachedBody::wrap_response(
            response,
            (
                WorkerLoadGuard::new(Arc::clone(&worker), None),
                DropSignal(Arc::clone(&dropped)),
            ),
        );

        assert_eq!(worker.load(), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "unavailable"
        );
        assert_eq!(worker.load(), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exact_backend_request_never_follows_redirects() {
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_url = format!("http://{}", destination.local_addr().unwrap());
        let redirect = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_url = format!("http://{}", redirect.local_addr().unwrap());
        let location = format!("{destination_url}/generate");
        let redirect_task = tokio::spawn(async move {
            let (mut socket, _) = redirect.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let router = create_test_regular_router();
        let worker: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new(redirect_url)
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        );
        let typed_req = TestGenerationRequest {
            stream: false,
            n: 1,
            max_tokens: 1,
            tools: Vec::new(),
            response_format: serde_json::Value::Null,
            reasoning_effort: String::new(),
        };
        let request = router
            .prepare_typed_request(None, &typed_req, "/generate", None, worker.as_ref())
            .unwrap();
        let response = router
            .send_prepared_typed_request(
                &router.no_redirect_client,
                request,
                "/generate",
                worker.as_ref(),
                false,
                None,
            )
            .await;

        redirect_task.await.unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), destination.accept())
                .await
                .is_err(),
            "the exact-route client must not POST to a redirect destination"
        );
    }

    #[tokio::test]
    async fn exact_nonstream_response_preserves_terminal_proof_and_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_url = format!("http://{}", listener.local_addr().unwrap());
        let body =
            br#"{"text":"ok","meta_info":{"completion_tokens":7,"finish_reason":{"type":"stop"}}}"#;
        let response_bytes = body.to_vec();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_bytes.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(&response_bytes).await.unwrap();
        });

        let router = create_test_regular_router();
        let worker: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new(worker_url)
                .worker_type(WorkerType::Regular)
                .health_config(no_health_check())
                .build(),
        );
        let typed_req = TestGenerationRequest {
            stream: false,
            n: 1,
            max_tokens: 1,
            tools: Vec::new(),
            response_format: serde_json::Value::Null,
            reasoning_effort: String::new(),
        };
        let request = router
            .prepare_typed_request(None, &typed_req, "/generate", None, worker.as_ref())
            .unwrap();
        let response = router
            .send_prepared_typed_request(
                &router.no_redirect_client,
                request,
                "/generate",
                worker.as_ref(),
                false,
                None,
            )
            .await;

        server.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let proof = response
            .extensions()
            .get::<BufferedResponseBytes>()
            .expect("exact non-stream response must retain buffered proof bytes");
        assert!(buffered_completion_has_proof(&proof.0));
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            body.as_slice()
        );
    }

    fn distribution_test_load(
        running_requests: i32,
        waiting_requests: i32,
        max_running_requests: i32,
    ) -> WorkerLoadResponse {
        WorkerLoadResponse {
            dp_rank_count: 1,
            loads: vec![SchedulerLoadSnapshot {
                dp_rank: 0,
                token_usage: 0.1,
                utilization: 0.1,
                num_running_reqs: running_requests,
                num_waiting_reqs: waiting_requests,
                max_running_requests,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn distribution_test_meta(request_id: &str) -> TenantRequestMeta {
        let tenant = TenantKey::new("tenant-a");
        let binding = CapacityCreditBinding::new(
            "generation-1",
            1,
            "test-model",
            "test-model",
            tenant.clone(),
            request_id,
            1,
        )
        .unwrap();
        TenantRequestMeta::new(tenant)
            .with_extension(RedeemedCapacityCreditAuthorization::new(binding))
    }

    struct ColdBootstrapTestFixture {
        router: Router,
        cache_policy: Arc<dyn LoadBalancingPolicy>,
        workers: Vec<Arc<dyn Worker>>,
        partition_headers: HeaderMap,
        // Keep adaptive load telemetry alive across sequential requests.
        _loads_tx: watch::Sender<HashMap<String, WorkerLoadResponse>>,
        _observed_loads_tx: watch::Sender<HashMap<String, ObservedWorkerLoad>>,
    }

    fn cold_bootstrap_test_fixture(worker_url: String) -> ColdBootstrapTestFixture {
        let worker: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new(worker_url)
                .model(ModelCard::new("test-model"))
                .worker_type(WorkerType::Regular)
                .connection_mode(ConnectionMode::Http)
                .label(ADMISSION_PARTITION_LABEL, "test-model")
                .health_config(no_health_check())
                .build(),
        );
        let workers = vec![Arc::clone(&worker)];
        let worker_registry = Arc::new(WorkerRegistry::new());
        worker_registry.register(Arc::clone(&worker)).unwrap();
        let policy_registry = Arc::new(PolicyRegistry::new(PolicyConfig::CacheAware {
            cache_threshold: 0.5,
            balance_abs_threshold: 32,
            balance_rel_threshold: 1.1,
            eviction_interval_secs: 0,
            max_tree_size: 10_000,
            fallback_output_token_estimate: 2_048,
            block_size: 16,
            engine_load: true,
            balance_token_usage_threshold: 1.0,
            overload_token_usage_threshold: 1.0,
            max_cached_owners_per_prefix: 2,
            cache_owner_spill_cooldown_secs: 60,
        }));
        let cache_policy = policy_registry.get_default_policy();
        cache_policy
            .as_any()
            .downcast_ref::<CacheAwarePolicy>()
            .unwrap()
            .init_workers(&workers);

        let load = distribution_test_load(0, 0, 38);
        let loads = HashMap::from([(worker.url().to_string(), load.clone())]);
        cache_policy.update_loads(&loads);
        let observed_loads = HashMap::from([(
            worker.url().to_string(),
            ObservedWorkerLoad {
                response: load,
                observed_at: Instant::now(),
                worker_generation_id: worker.generation_id(),
                worker_revision: worker.revision(),
                router_load_at_observation: worker.load(),
                source: WorkerLoadSource::NativeLoads,
                scheduler_counts_present: true,
            },
        )]);
        let (_loads_tx, loads_rx) = watch::channel(loads);
        let (_observed_loads_tx, observed_loads_rx) = watch::channel(observed_loads);
        let controller = AdaptiveAdmissionController::new(
            AdaptiveAdmissionConfig {
                mode: AdaptiveAdmissionMode::Enforce,
                strategy: AdaptiveAdmissionStrategy::EngineFeedback,
                min_load_coverage: 1.0,
                feedback_max_waiting_requests_per_healthy_replica: 0,
                distribution_headroom_partitions: vec!["test-model".to_string()],
                distribution_headroom_max_inflight: 1,
                ..Default::default()
            },
            Arc::clone(&worker_registry),
        );
        controller.start_load_updates(loads_rx, observed_loads_rx);

        let mut partition_headers = HeaderMap::new();
        partition_headers.insert(
            ADMISSION_PARTITION_HEADER,
            HeaderValue::from_static("test-model"),
        );
        ColdBootstrapTestFixture {
            router: Router {
                worker_registry,
                policy_registry,
                client: Client::new(),
                no_redirect_client: Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                retry_config: RetryConfig::default(),
                realtime_registry: Arc::new(RealtimeRegistry::new()),
                webrtc_bind_addr: None,
                webrtc_stun_server: None,
                adaptive_admission: Some(controller),
            },
            cache_policy,
            workers,
            partition_headers,
            _loads_tx,
            _observed_loads_tx,
        }
    }

    #[derive(Clone, Copy)]
    enum TerminalTopologyMutation {
        Remove,
        ReplaceSameUrl,
    }

    async fn assert_terminal_cold_owner_rejects_topology_mutation(
        mutation: TerminalTopologyMutation,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_url = format!("http://{}", listener.local_addr().unwrap());
        let valid_body =
            br#"{"text":"ok","meta_info":{"completion_tokens":7,"finish_reason":{"type":"stop"}}}"#
                .to_vec();
        let server_body = valid_body.clone();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (send_response_tx, send_response_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            request_seen_tx.send(()).unwrap();
            send_response_rx.await.unwrap();
            let response_head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_body.len()
            );
            socket.write_all(response_head.as_bytes()).await.unwrap();
            socket.write_all(&server_body).await.unwrap();
        });

        let ColdBootstrapTestFixture {
            router,
            cache_policy,
            workers,
            partition_headers,
            _loads_tx,
            _observed_loads_tx,
        } = cold_bootstrap_test_fixture(worker_url.clone());
        let router = Arc::new(router);
        let request_router = Arc::clone(&router);
        let request_headers = partition_headers.clone();
        let request = tokio::spawn(async move {
            request_router
                .route_typed_request(
                    Some(&request_headers),
                    &TenantRequestMeta::new(TenantKey::new("tenant-a")),
                    &TestGenerationRequest {
                        stream: false,
                        n: 1,
                        max_tokens: 1,
                        tools: Vec::new(),
                        response_format: serde_json::Value::Null,
                        reasoning_effort: String::new(),
                    },
                    "/generate",
                    "test-model",
                )
                .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(10), request_seen_rx)
            .await
            .expect("the guarded request must reach its selected worker")
            .unwrap();
        let worker_id = router.worker_registry.get_id_by_url(&worker_url).unwrap();
        match mutation {
            TerminalTopologyMutation::Remove => {
                router.worker_registry.remove(&worker_id).unwrap();
                // A stale Arc can become executable again independently of
                // membership. The authoritative guard must still reject it.
                workers[0].set_status(openai_protocol::worker::WorkerStatus::Ready);
            }
            TerminalTopologyMutation::ReplaceSameUrl => {
                let replacement: Arc<dyn Worker> = Arc::new(
                    BasicWorkerBuilder::new(worker_url)
                        .model(ModelCard::new("test-model"))
                        .worker_type(WorkerType::Regular)
                        .connection_mode(ConnectionMode::Http)
                        .label(ADMISSION_PARTITION_LABEL, "test-model")
                        .health_config(no_health_check())
                        .build(),
                );
                assert!(router.worker_registry.replace(&worker_id, replacement));
            }
        }
        send_response_tx.send(()).unwrap();

        let response = tokio::time::timeout(std::time::Duration::from_secs(10), request)
            .await
            .expect("the guarded request must finish after its worker responds")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .extensions()
            .get::<BufferedResponseBytes>()
            .is_some_and(|body| buffered_completion_has_proof(&body.0)));
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            valid_body
        );
        assert_eq!(
            cache_policy.select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("abcdefgh"),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            ),
            None,
            "terminal success from a stale target must not publish cache ownership"
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), server)
            .await
            .expect("the test worker must finish its response")
            .unwrap();
        drop((_loads_tx, _observed_loads_tx));
    }

    #[tokio::test]
    async fn terminal_cold_owner_rejects_worker_removed_after_dispatch() {
        assert_terminal_cold_owner_rejects_topology_mutation(TerminalTopologyMutation::Remove)
            .await;
    }

    #[tokio::test]
    async fn terminal_cold_owner_rejects_same_url_replacement_after_dispatch() {
        assert_terminal_cold_owner_rejects_topology_mutation(
            TerminalTopologyMutation::ReplaceSameUrl,
        )
        .await;
    }

    #[tokio::test]
    async fn guarded_cold_bootstrap_nonstream_rolls_back_without_terminal_usage() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_url = format!("http://{}", listener.local_addr().unwrap());
        let invalid_body =
            br#"{"text":"ok","meta_info":{"finish_reason":{"type":"stop"}}}"#.to_vec();
        let valid_body =
            br#"{"text":"ok","meta_info":{"completion_tokens":7,"finish_reason":{"type":"stop"}}}"#
                .to_vec();
        let server_bodies = [invalid_body.clone(), valid_body.clone()];
        let server = tokio::spawn(async move {
            for response_bytes in server_bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let response_head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_bytes.len()
                );
                socket.write_all(response_head.as_bytes()).await.unwrap();
                socket.write_all(&response_bytes).await.unwrap();
            }
        });

        let worker: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new(worker_url)
                .model(ModelCard::new("test-model"))
                .worker_type(WorkerType::Regular)
                .connection_mode(ConnectionMode::Http)
                .label(ADMISSION_PARTITION_LABEL, "test-model")
                .health_config(no_health_check())
                .build(),
        );
        let workers = vec![Arc::clone(&worker)];
        let worker_registry = Arc::new(WorkerRegistry::new());
        worker_registry.register(Arc::clone(&worker)).unwrap();
        let policy_registry = Arc::new(PolicyRegistry::new(PolicyConfig::CacheAware {
            cache_threshold: 0.5,
            balance_abs_threshold: 32,
            balance_rel_threshold: 1.1,
            eviction_interval_secs: 0,
            max_tree_size: 10_000,
            fallback_output_token_estimate: 2_048,
            block_size: 16,
            engine_load: true,
            balance_token_usage_threshold: 1.0,
            overload_token_usage_threshold: 1.0,
            max_cached_owners_per_prefix: 2,
            cache_owner_spill_cooldown_secs: 60,
        }));
        let cache_policy = policy_registry.get_default_policy();
        cache_policy
            .as_any()
            .downcast_ref::<CacheAwarePolicy>()
            .unwrap()
            .init_workers(&workers);

        let load = distribution_test_load(0, 0, 38);
        let loads = HashMap::from([(worker.url().to_string(), load.clone())]);
        cache_policy.update_loads(&loads);
        let observed_loads = HashMap::from([(
            worker.url().to_string(),
            ObservedWorkerLoad {
                response: load,
                observed_at: Instant::now(),
                worker_generation_id: worker.generation_id(),
                worker_revision: worker.revision(),
                router_load_at_observation: worker.load(),
                source: WorkerLoadSource::NativeLoads,
                scheduler_counts_present: true,
            },
        )]);
        let (_loads_tx, loads_rx) = watch::channel(loads);
        let (_observed_loads_tx, observed_loads_rx) = watch::channel(observed_loads);
        let controller = AdaptiveAdmissionController::new(
            AdaptiveAdmissionConfig {
                mode: AdaptiveAdmissionMode::Enforce,
                strategy: AdaptiveAdmissionStrategy::EngineFeedback,
                min_load_coverage: 1.0,
                feedback_max_waiting_requests_per_healthy_replica: 0,
                distribution_headroom_partitions: vec!["test-model".to_string()],
                distribution_headroom_max_inflight: 1,
                ..Default::default()
            },
            Arc::clone(&worker_registry),
        );
        controller.start_load_updates(loads_rx, observed_loads_rx);

        let router = Router {
            worker_registry,
            policy_registry,
            client: Client::new(),
            no_redirect_client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            retry_config: RetryConfig::default(),
            realtime_registry: Arc::new(RealtimeRegistry::new()),
            webrtc_bind_addr: None,
            webrtc_stun_server: None,
            adaptive_admission: Some(controller),
        };
        let mut partition_headers = HeaderMap::new();
        partition_headers.insert(
            ADMISSION_PARTITION_HEADER,
            HeaderValue::from_static("test-model"),
        );
        let typed_req = TestGenerationRequest {
            stream: false,
            n: 1,
            max_tokens: 1,
            tools: Vec::new(),
            response_format: serde_json::Value::Null,
            reasoning_effort: String::new(),
        };
        let tenant_meta = TenantRequestMeta::new(TenantKey::new("tenant-a"));

        let invalid_response = router
            .route_typed_request(
                Some(&partition_headers),
                &tenant_meta,
                &typed_req,
                "/generate",
                "test-model",
            )
            .await;
        assert_eq!(invalid_response.status(), StatusCode::OK);
        assert!(invalid_response
            .extensions()
            .get::<BufferedResponseBytes>()
            .is_some_and(|body| !buffered_completion_has_proof(&body.0)));
        assert_eq!(
            to_bytes(invalid_response.into_body(), usize::MAX)
                .await
                .unwrap(),
            invalid_body
        );
        assert_eq!(
            cache_policy.select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("abcdefgh"),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            ),
            None,
            "an invalid 2xx body must not publish the cold owner"
        );

        let valid_response = router
            .route_typed_request(
                Some(&partition_headers),
                &tenant_meta,
                &typed_req,
                "/generate",
                "test-model",
            )
            .await;
        assert_eq!(valid_response.status(), StatusCode::OK);
        assert!(valid_response
            .extensions()
            .get::<BufferedResponseBytes>()
            .is_some_and(|body| buffered_completion_has_proof(&body.0)));
        assert_eq!(
            to_bytes(valid_response.into_body(), usize::MAX)
                .await
                .unwrap(),
            valid_body
        );
        assert_eq!(
            cache_policy.select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("abcdefgh"),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            ),
            Some(0),
            "a terminal body with usage must publish the exact cold owner"
        );

        server.await.unwrap();
        drop((_loads_tx, _observed_loads_tx));
    }

    #[tokio::test]
    async fn guarded_cold_bootstrap_openai_stream_without_usage_rejects_before_dispatch() {
        let ColdBootstrapTestFixture {
            router,
            cache_policy,
            workers,
            partition_headers,
            _loads_tx,
            _observed_loads_tx,
        } = cold_bootstrap_test_fixture("http://127.0.0.1:9".to_string());
        let tenant_meta = TenantRequestMeta::new(TenantKey::new("tenant-a"));

        for include_usage in [None, Some(false)] {
            let response = router
                .route_typed_request(
                    Some(&partition_headers),
                    &tenant_meta,
                    &streaming_chat_request(include_usage),
                    "/v1/chat/completions",
                    "test-model",
                )
                .await;

            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(response
                .extensions()
                .get::<LocalAdaptiveRejection>()
                .is_some());
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("stream_options.include_usage=true"));
            assert_eq!(
                workers[0].load(),
                0,
                "local refusal must release worker load"
            );
            assert_eq!(
                cache_policy.select_worker(
                    &workers,
                    &SelectWorkerInfo {
                        request_text: Some("abcdefgh"),
                        headers: Some(&partition_headers),
                        forbid_unleased_cache_owner_expansion: true,
                        ..Default::default()
                    },
                ),
                None,
                "local refusal must not publish the cold worker"
            );
        }
    }

    #[tokio::test]
    async fn guarded_cold_bootstrap_openai_stream_with_usage_dispatches_and_commits() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_url = format!("http://{}", listener.local_addr().unwrap());
        let stream_body = b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                            data: {\"choices\":[],\"usage\":{\"completion_tokens\":7}}\n\n\
                            data: [DONE]\n\n"
            .to_vec();
        let server_body = stream_body.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let response_head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_body.len()
            );
            socket.write_all(response_head.as_bytes()).await.unwrap();
            socket.write_all(&server_body).await.unwrap();
        });

        let ColdBootstrapTestFixture {
            router,
            cache_policy,
            workers,
            partition_headers,
            _loads_tx,
            _observed_loads_tx,
        } = cold_bootstrap_test_fixture(worker_url);
        let response = router
            .route_typed_request(
                Some(&partition_headers),
                &TenantRequestMeta::new(TenantKey::new("tenant-a")),
                &streaming_chat_request(Some(true)),
                "/v1/chat/completions",
                "test-model",
            )
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            stream_body
        );
        assert_eq!(workers[0].load(), 0);
        assert_eq!(
            cache_policy.select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("abcdefgh"),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            ),
            Some(0),
            "terminal OpenAI usage proof must publish the cold worker"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn guarded_cold_bootstrap_generate_stream_still_dispatches_and_commits() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_url = format!("http://{}", listener.local_addr().unwrap());
        let stream_body = b"data: {\"text\":\"ok\",\"meta_info\":{\"completion_tokens\":7,\"finish_reason\":{\"type\":\"stop\"}}}\n\n"
            .to_vec();
        let server_body = stream_body.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let response_head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_body.len()
            );
            socket.write_all(response_head.as_bytes()).await.unwrap();
            socket.write_all(&server_body).await.unwrap();
        });

        let ColdBootstrapTestFixture {
            router,
            cache_policy,
            workers,
            partition_headers,
            _loads_tx,
            _observed_loads_tx,
        } = cold_bootstrap_test_fixture(worker_url);
        let typed_req = TestGenerationRequest {
            stream: true,
            n: 1,
            max_tokens: 1,
            tools: Vec::new(),
            response_format: serde_json::Value::Null,
            reasoning_effort: String::new(),
        };
        let response = router
            .route_typed_request(
                Some(&partition_headers),
                &TenantRequestMeta::new(TenantKey::new("tenant-a")),
                &typed_req,
                "/generate",
                "test-model",
            )
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            stream_body
        );
        assert_eq!(workers[0].load(), 0);
        assert_eq!(
            cache_policy.select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some("abcdefgh"),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            ),
            Some(0),
            "native terminal usage proof must still publish the cold worker"
        );
        server.await.unwrap();
    }

    struct DistributionTestFixture {
        router: Router,
        cache_policy: Arc<dyn LoadBalancingPolicy>,
        workers: Vec<Arc<dyn Worker>>,
        partition_headers: HeaderMap,
        request_text: &'static str,
        // Keep the watch channels alive for the entire fixture. Dropping the
        // senders at helper return makes adaptive headroom disappear between
        // sequential requests and turns the second request into a spurious 429.
        _loads_tx: watch::Sender<HashMap<String, WorkerLoadResponse>>,
        _observed_loads_tx: watch::Sender<HashMap<String, ObservedWorkerLoad>>,
    }

    fn distribution_test_fixture(idle_url: String) -> DistributionTestFixture {
        let hot: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new("http://127.0.0.1:1")
                .model(ModelCard::new("test-model"))
                .worker_type(WorkerType::Regular)
                .connection_mode(ConnectionMode::Http)
                .label(ADMISSION_PARTITION_LABEL, "test-model")
                .health_config(no_health_check())
                .build(),
        );
        let idle: Arc<dyn Worker> = Arc::new(
            BasicWorkerBuilder::new(idle_url)
                .model(ModelCard::new("test-model"))
                .worker_type(WorkerType::Regular)
                .connection_mode(ConnectionMode::Http)
                .label(ADMISSION_PARTITION_LABEL, "test-model")
                .health_config(no_health_check())
                .build(),
        );
        let workers = vec![Arc::clone(&hot), Arc::clone(&idle)];
        let worker_registry = Arc::new(WorkerRegistry::new());
        worker_registry.register(Arc::clone(&hot)).unwrap();
        worker_registry.register(Arc::clone(&idle)).unwrap();

        let policy_registry = Arc::new(PolicyRegistry::new(PolicyConfig::CacheAware {
            cache_threshold: 0.5,
            balance_abs_threshold: 32,
            balance_rel_threshold: 1.1,
            eviction_interval_secs: 0,
            max_tree_size: 10_000,
            fallback_output_token_estimate: 2_048,
            block_size: 16,
            engine_load: true,
            balance_token_usage_threshold: 1.0,
            overload_token_usage_threshold: 1.0,
            max_cached_owners_per_prefix: 2,
            cache_owner_spill_cooldown_secs: 60,
        }));
        let cache_policy = policy_registry.get_default_policy();
        cache_policy
            .as_any()
            .downcast_ref::<CacheAwarePolicy>()
            .unwrap()
            .init_workers(&workers);

        let mut partition_headers = HeaderMap::new();
        partition_headers.insert(
            ADMISSION_PARTITION_HEADER,
            HeaderValue::from_static("test-model"),
        );
        let request_text = "abcdefgh";
        let idle_guard = WorkerLoadGuard::new(Arc::clone(&idle), None);
        assert_eq!(
            cache_policy.select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some(request_text),
                    headers: Some(&partition_headers),
                    ..Default::default()
                },
            ),
            Some(0),
            "the setup must prime only the hot worker as prefix owner"
        );
        drop(idle_guard);
        for _ in 0..5 {
            hot.record_outcome(503);
        }
        assert!(hot.is_healthy());
        assert!(!hot.is_available());

        let hot_load = distribution_test_load(37, 1, 38);
        let idle_load = distribution_test_load(0, 0, 38);
        let loads = HashMap::from([
            (hot.url().to_string(), hot_load.clone()),
            (idle.url().to_string(), idle_load.clone()),
        ]);
        cache_policy.update_loads(&loads);
        let observed_at = Instant::now();
        let observed_loads = HashMap::from([
            (
                hot.url().to_string(),
                ObservedWorkerLoad {
                    response: hot_load,
                    observed_at,
                    worker_generation_id: hot.generation_id(),
                    worker_revision: hot.revision(),
                    router_load_at_observation: hot.load(),
                    source: WorkerLoadSource::NativeLoads,
                    scheduler_counts_present: true,
                },
            ),
            (
                idle.url().to_string(),
                ObservedWorkerLoad {
                    response: idle_load,
                    observed_at,
                    worker_generation_id: idle.generation_id(),
                    worker_revision: idle.revision(),
                    router_load_at_observation: idle.load(),
                    source: WorkerLoadSource::NativeLoads,
                    scheduler_counts_present: true,
                },
            ),
        ]);
        let (_loads_tx, loads_rx) = watch::channel(loads);
        let (_observed_loads_tx, observed_loads_rx) = watch::channel(observed_loads);
        let controller = AdaptiveAdmissionController::new(
            AdaptiveAdmissionConfig {
                mode: AdaptiveAdmissionMode::Enforce,
                strategy: AdaptiveAdmissionStrategy::EngineFeedback,
                min_load_coverage: 1.0,
                feedback_max_waiting_requests_per_healthy_replica: 0,
                distribution_headroom_partitions: vec!["test-model".to_string()],
                distribution_headroom_max_inflight: 1,
                ..Default::default()
            },
            Arc::clone(&worker_registry),
        );
        controller.start_load_updates(loads_rx, observed_loads_rx);

        DistributionTestFixture {
            router: Router {
                worker_registry,
                policy_registry,
                client: Client::new(),
                no_redirect_client: Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                retry_config: RetryConfig::default(),
                realtime_registry: Arc::new(RealtimeRegistry::new()),
                webrtc_bind_addr: None,
                webrtc_stun_server: None,
                adaptive_admission: Some(controller),
            },
            cache_policy,
            workers,
            partition_headers,
            request_text,
            _loads_tx,
            _observed_loads_tx,
        }
    }

    fn streaming_chat_request(include_usage: Option<bool>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            messages: vec![ChatMessage::User {
                content: MessageContent::Text("abcdefgh".to_string()),
                name: None,
            }],
            model: "test-model".to_string(),
            max_completion_tokens: Some(1),
            stream: true,
            stream_options: include_usage.map(|include_usage| StreamOptions {
                include_usage: Some(include_usage),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn guarded_stream_contract_is_endpoint_specific_and_fail_closed() {
        let request = |body: serde_json::Value| {
            Client::new()
                .post("http://worker/v1/chat/completions")
                .json(&body)
                .build()
                .unwrap()
        };

        for route in ["/generate", "/v1/messages", "/v1/responses"] {
            assert!(validate_guarded_stream_usage_contract(
                route,
                &request(serde_json::json!({"stream": true})),
            )
            .is_ok());
        }

        for body in [
            serde_json::json!({"stream": true}),
            serde_json::json!({
                "stream": true,
                "stream_options": {"include_usage": false}
            }),
        ] {
            assert!(validate_guarded_stream_usage_contract(
                "/v1/chat/completions",
                &request(body.clone()),
            )
            .is_err());
            assert!(
                validate_guarded_stream_usage_contract("/v1/completions", &request(body),).is_err()
            );
        }

        let with_usage = request(serde_json::json!({
            "stream": true,
            "stream_options": {"include_usage": true}
        }));
        assert!(
            validate_guarded_stream_usage_contract("/v1/chat/completions", &with_usage,).is_ok()
        );
        assert!(validate_guarded_stream_usage_contract("/v1/completions", &with_usage,).is_ok());
        assert!(validate_guarded_stream_usage_contract("/future/stream", &with_usage,).is_err());
    }

    #[tokio::test]
    async fn distribution_openai_stream_without_usage_is_rejected_before_dispatch() {
        let DistributionTestFixture {
            router,
            cache_policy,
            workers,
            partition_headers,
            request_text,
            _loads_tx,
            _observed_loads_tx,
        } = distribution_test_fixture("http://127.0.0.1:9".to_string());

        for (request_id, include_usage) in
            [("request-absent", None), ("request-false", Some(false))]
        {
            let meta = distribution_test_meta(request_id);
            let response = router
                .route_typed_request(
                    Some(&partition_headers),
                    &meta,
                    &streaming_chat_request(include_usage),
                    "/v1/chat/completions",
                    "test-model",
                )
                .await;

            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(response
                .extensions()
                .get::<LocalAdaptiveRejection>()
                .is_some());
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("stream_options.include_usage=true"));
            assert!(
                meta.extension::<RedeemedCapacityCreditAuthorization>()
                    .unwrap()
                    .try_claim("test-model", "test-model"),
                "a locally rejected seed must leave its scheduler route claim unused"
            );
            assert_eq!(workers[1].load(), 0);
            assert_eq!(
                cache_policy.select_worker(
                    &workers,
                    &SelectWorkerInfo {
                        request_text: Some(request_text),
                        headers: Some(&partition_headers),
                        forbid_unleased_cache_owner_expansion: true,
                        ..Default::default()
                    },
                ),
                None,
                "a rejected stream must not publish the clean peer as an owner"
            );
        }
    }

    #[tokio::test]
    async fn distribution_openai_stream_with_usage_dispatches_and_commits() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let idle_url = format!("http://{}", listener.local_addr().unwrap());
        let stream_body = b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                            data: {\"choices\":[],\"usage\":{\"completion_tokens\":7}}\n\n\
                            data: [DONE]\n\n"
            .to_vec();
        let server_body = stream_body.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let response_head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_body.len()
            );
            socket.write_all(response_head.as_bytes()).await.unwrap();
            socket.write_all(&server_body).await.unwrap();
        });

        let DistributionTestFixture {
            router,
            cache_policy,
            workers,
            partition_headers,
            request_text,
            _loads_tx,
            _observed_loads_tx,
        } = distribution_test_fixture(idle_url);
        let meta = distribution_test_meta("request-with-usage");
        let response = router
            .route_typed_request(
                Some(&partition_headers),
                &meta,
                &streaming_chat_request(Some(true)),
                "/v1/chat/completions",
                "test-model",
            )
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .extensions()
            .get::<LocalAdaptiveRejection>()
            .is_none());
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            stream_body
        );
        assert!(
            !meta
                .extension::<RedeemedCapacityCreditAuthorization>()
                .unwrap()
                .try_claim("test-model", "test-model"),
            "a dispatched seed must consume its scheduler route claim"
        );
        let committed_idx = cache_policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some(request_text),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            )
            .expect("terminal usage proof must publish the clean peer");
        assert_eq!(workers[committed_idx].url(), workers[1].url());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn distribution_nonstream_commits_owner_only_after_terminal_usage_proof() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let idle_url = format!("http://{}", listener.local_addr().unwrap());
        let invalid_body =
            br#"{"text":"ok","meta_info":{"finish_reason":{"type":"stop"}}}"#.to_vec();
        let valid_body =
            br#"{"text":"ok","meta_info":{"completion_tokens":7,"finish_reason":{"type":"stop"}}}"#
                .to_vec();
        let server_bodies = [invalid_body.clone(), valid_body.clone()];
        let server = tokio::spawn(async move {
            for response_bytes in server_bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let response_head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_bytes.len()
                );
                socket.write_all(response_head.as_bytes()).await.unwrap();
                socket.write_all(&response_bytes).await.unwrap();
            }
        });

        let DistributionTestFixture {
            router,
            cache_policy,
            workers,
            partition_headers,
            request_text,
            _loads_tx,
            _observed_loads_tx,
        } = distribution_test_fixture(idle_url);
        let idle = Arc::clone(&workers[1]);
        let typed_req = TestGenerationRequest {
            stream: false,
            n: 1,
            max_tokens: 1,
            tools: Vec::new(),
            response_format: serde_json::Value::Null,
            reasoning_effort: String::new(),
        };

        let invalid_response = router
            .route_typed_request(
                Some(&partition_headers),
                &distribution_test_meta("request-invalid"),
                &typed_req,
                "/generate",
                "test-model",
            )
            .await;
        assert_eq!(invalid_response.status(), StatusCode::OK);
        assert!(invalid_response
            .extensions()
            .get::<LocalAdaptiveRejection>()
            .is_none());
        assert!(invalid_response
            .extensions()
            .get::<BufferedResponseBytes>()
            .is_some_and(|body| !buffered_completion_has_proof(&body.0)));
        assert_eq!(
            to_bytes(invalid_response.into_body(), usize::MAX)
                .await
                .unwrap(),
            invalid_body
        );
        assert_eq!(
            cache_policy.select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some(request_text),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            ),
            None,
            "a 2xx response without terminal usage proof must not publish the idle owner"
        );

        let valid_response = router
            .route_typed_request(
                Some(&partition_headers),
                &distribution_test_meta("request-valid"),
                &typed_req,
                "/generate",
                "test-model",
            )
            .await;
        assert_eq!(valid_response.status(), StatusCode::OK);
        assert!(valid_response
            .extensions()
            .get::<LocalAdaptiveRejection>()
            .is_none());
        assert!(valid_response
            .extensions()
            .get::<BufferedResponseBytes>()
            .is_some_and(|body| buffered_completion_has_proof(&body.0)));
        assert_eq!(
            to_bytes(valid_response.into_body(), usize::MAX)
                .await
                .unwrap(),
            valid_body
        );
        let committed_idx = cache_policy
            .select_worker(
                &workers,
                &SelectWorkerInfo {
                    request_text: Some(request_text),
                    headers: Some(&partition_headers),
                    forbid_unleased_cache_owner_expansion: true,
                    ..Default::default()
                },
            )
            .expect("terminal usage proof must publish the idle owner");
        assert_eq!(workers[committed_idx].url(), idle.url());

        server.await.unwrap();
    }

    fn create_test_regular_router() -> Router {
        // Create registries
        let worker_registry = Arc::new(WorkerRegistry::new());
        let policy_registry = Arc::new(PolicyRegistry::new(PolicyConfig::RoundRobin));

        // Register test workers
        let worker1 = BasicWorkerBuilder::new("http://worker1:8080")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        let worker2 = BasicWorkerBuilder::new("http://worker2:8080")
            .worker_type(WorkerType::Regular)
            .health_config(no_health_check())
            .build();
        worker_registry.register_or_replace(Arc::new(worker1));
        worker_registry.register_or_replace(Arc::new(worker2));

        Router {
            worker_registry,
            policy_registry,
            client: Client::new(),
            no_redirect_client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            retry_config: RetryConfig::default(),
            realtime_registry: Arc::new(RealtimeRegistry::new()),
            webrtc_bind_addr: None,
            webrtc_stun_server: None,
            adaptive_admission: None,
        }
    }

    fn create_test_unhealthy_router() -> Router {
        let router = create_test_regular_router();
        let workers = router.worker_registry.get_all();
        workers[0].set_status(openai_protocol::worker::WorkerStatus::NotReady);
        router
    }

    #[test]
    fn test_router_get_worker_urls_regular() {
        let router = create_test_regular_router();
        let workers = router.worker_registry.get_all();
        let urls: Vec<String> = workers.iter().map(|w| w.url().to_string()).collect();

        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"http://worker1:8080".to_string()));
        assert!(urls.contains(&"http://worker2:8080".to_string()));
    }

    #[test]
    fn test_select_first_worker_regular() {
        let router = create_test_regular_router();
        let result = router.select_first_worker();

        assert!(result.is_ok());
        let url = result.unwrap();
        // DashMap doesn't guarantee order, so just check we get one of the workers
        assert!(url == "http://worker1:8080" || url == "http://worker2:8080");
    }

    #[test]
    fn test_select_first_worker_with_unhealthy_worker() {
        let router = create_test_unhealthy_router();
        let result = router.select_first_worker();

        assert!(result.is_ok());
        let url = result.unwrap();

        let worker = router.worker_registry.get_by_url(&url).unwrap();
        assert!(worker.is_healthy());
    }
}
