//! Adaptive predicted-work admission after request tokenization.

use async_trait::async_trait;
use axum::{
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    response::Response,
};

use super::PipelineStage;
use crate::{
    middleware::scheduler::ADMISSION_PARTITION_HEADER,
    routers::{
        error,
        grpc::{
            adaptive_admission::{
                PredictionFeatures, FLAG_MULTIPLE_COMPLETIONS, FLAG_REASONING, FLAG_STREAMING,
                FLAG_STRUCTURED_OUTPUT, FLAG_TOOLS,
            },
            context::{RequestContext, RequestType},
        },
    },
};

const COMET_USER_HEADER: &str = "x-comet-user";
const COMET_WORKLOAD_TYPE_HEADER: &str = "x-comet-workload-type";

pub(crate) struct AdaptiveAdmissionStage;

#[derive(Clone, Copy)]
struct GenerationShape {
    endpoint: &'static str,
    max_output_tokens: Option<u32>,
    flags: u16,
}

impl GenerationShape {
    fn for_request(request: &RequestType) -> Option<Self> {
        match request {
            RequestType::Chat(request) => {
                #[expect(
                    deprecated,
                    reason = "max_tokens remains an OpenAI compatibility fallback"
                )]
                let per_completion_limit = request.max_completion_tokens.or(request.max_tokens);
                let multiplicity = request.n.unwrap_or(1).max(1);
                let mut flags = 0;
                if multiplicity > 1 {
                    flags |= FLAG_MULTIPLE_COMPLETIONS;
                }
                if request
                    .tools
                    .as_ref()
                    .is_some_and(|tools| !tools.is_empty())
                {
                    flags |= FLAG_TOOLS;
                }
                if request.response_format.is_some()
                    || request.regex.is_some()
                    || request.ebnf.is_some()
                {
                    flags |= FLAG_STRUCTURED_OUTPUT;
                }
                if request.reasoning_effort.is_some() {
                    flags |= FLAG_REASONING;
                }
                if request.stream {
                    flags |= FLAG_STREAMING;
                }
                Some(Self {
                    endpoint: "chat",
                    max_output_tokens: multiplied_limit(per_completion_limit, multiplicity),
                    flags,
                })
            }
            RequestType::Generate(request) => {
                let params = request.sampling_params.as_ref();
                let multiplicity = params.and_then(|p| p.n).unwrap_or(1).max(1);
                let mut flags = 0;
                if multiplicity > 1 {
                    flags |= FLAG_MULTIPLE_COMPLETIONS;
                }
                if params.is_some_and(|p| {
                    p.json_schema.is_some() || p.regex.is_some() || p.ebnf.is_some()
                }) {
                    flags |= FLAG_STRUCTURED_OUTPUT;
                }
                if request.stream {
                    flags |= FLAG_STREAMING;
                }
                Some(Self {
                    endpoint: "generate",
                    max_output_tokens: multiplied_limit(
                        params.and_then(|p| p.max_new_tokens),
                        multiplicity,
                    ),
                    flags,
                })
            }
            RequestType::Completion(request) => {
                let returned = request.n.unwrap_or(1).max(1);
                let generated = request.best_of.unwrap_or(returned).max(returned);
                let mut flags = 0;
                if generated > 1 {
                    flags |= FLAG_MULTIPLE_COMPLETIONS;
                }
                if request.stream {
                    flags |= FLAG_STREAMING;
                }
                Some(Self {
                    endpoint: "completion",
                    max_output_tokens: multiplied_limit(request.max_tokens, generated),
                    flags,
                })
            }
            RequestType::Messages(request) => {
                let mut flags = 0;
                if request
                    .tools
                    .as_ref()
                    .is_some_and(|tools| !tools.is_empty())
                {
                    flags |= FLAG_TOOLS;
                }
                if request.thinking.is_some() {
                    flags |= FLAG_REASONING;
                }
                if request.is_stream() {
                    flags |= FLAG_STREAMING;
                }
                Some(Self {
                    endpoint: "messages",
                    max_output_tokens: Some(request.max_tokens),
                    flags,
                })
            }
            RequestType::Responses(request) => {
                let mut flags = 0;
                if request
                    .tools
                    .as_ref()
                    .is_some_and(|tools| !tools.is_empty())
                {
                    flags |= FLAG_TOOLS;
                }
                if request.text.is_some() {
                    flags |= FLAG_STRUCTURED_OUTPUT;
                }
                if request.reasoning.is_some() {
                    flags |= FLAG_REASONING;
                }
                if request.stream.unwrap_or(false) {
                    flags |= FLAG_STREAMING;
                }
                Some(Self {
                    endpoint: "responses",
                    max_output_tokens: request.max_output_tokens,
                    flags,
                })
            }
            RequestType::Embedding(_) | RequestType::Classify(_) => None,
        }
    }
}

fn multiplied_limit(per_completion: Option<u32>, multiplicity: u32) -> Option<u32> {
    per_completion.map(|limit| limit.saturating_mul(multiplicity))
}

fn trusted_header(ctx: &RequestContext, name: &str) -> Option<String> {
    ctx.input
        .headers
        .as_ref()
        .and_then(|headers| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn rejection_response(retry_after_secs: u32) -> Response {
    let mut response = error::create_error(
        StatusCode::TOO_MANY_REQUESTS,
        "adaptive_admission_saturated",
        "model fleet is temporarily saturated",
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.max(1).to_string()) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

#[async_trait]
impl PipelineStage for AdaptiveAdmissionStage {
    async fn execute(&self, ctx: &mut RequestContext) -> Result<Option<Response>, Response> {
        let Some(controller) = ctx.components.adaptive_admission.clone() else {
            return Ok(None);
        };
        let Some(shape) = GenerationShape::for_request(&ctx.input.request_type) else {
            return Ok(None);
        };
        let prompt_tokens = ctx
            .state
            .preparation
            .as_ref()
            .map_or(0, |preparation| preparation.total_token_count());
        let partition = trusted_header(ctx, ADMISSION_PARTITION_HEADER)
            .unwrap_or_else(|| ctx.input.model_id.clone());
        let user = trusted_header(ctx, COMET_USER_HEADER)
            .or_else(|| {
                ctx.input
                    .tenant_request_meta
                    .as_ref()
                    .map(|meta| meta.tenant_key().as_str().to_string())
            })
            .unwrap_or_else(|| "anonymous".to_string());
        let workload_type = trusted_header(ctx, COMET_WORKLOAD_TYPE_HEADER)
            .unwrap_or_else(|| "unclassified".to_string());
        let tracker = controller.begin(
            partition,
            PredictionFeatures {
                model: ctx.input.model_id.clone(),
                user,
                workload_type,
                endpoint: shape.endpoint,
                prompt_tokens,
                max_output_tokens: shape.max_output_tokens,
                generation_flags: shape.flags,
            },
        );
        if tracker.should_reject() {
            return Err(rejection_response(tracker.retry_after_secs()));
        }
        ctx.state.adaptive_request = Some(tracker);
        Ok(None)
    }

    fn name(&self) -> &'static str {
        "AdaptiveAdmission"
    }
}
