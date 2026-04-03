//! Score pipeline stage for vLLM native gRPC `/v1/score` endpoint
//!
//! This stage handles native gRPC scoring by using the populated GrpcClient
//! to directly dispatch to the backend vLLM worker's `Score` RPC.

use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use openai_protocol::common::StringOrArray;
use tracing::{debug, error};

use crate::routers::{
    error,
    grpc::{
        common::stages::PipelineStage,
        context::{ClientState, RequestContext},
    },
};

/// Pipeline stage that executes `/v1/score` requests via gRPC
///
/// This stage leverages the `ClientAcquisitionStage` which populates the context
/// with the appropriate `GrpcClient`, and then directly uses native gRPC.
/// It short-circuits the pipeline by returning `Ok(Some(response))`
/// after forwarding, so no downstream stages are executed.
pub(crate) struct ScoreNativeStage;

impl ScoreNativeStage {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl PipelineStage for ScoreNativeStage {
    fn name(&self) -> &'static str {
        "ScoreNativeStage"
    }

    async fn execute(&self, ctx: &mut RequestContext) -> Result<Option<Response>, Response> {
        let score_req = ctx.score_request_arc();

        let mut client = match ctx.state.client.take() {
            Some(ClientState::Single(c)) => c,
            _ => {
                error!(stage = self.name(), "Missing client for ScoreNativeStage");
                return Err(error::internal_error(
                    "score_missing_client",
                    "Missing gRPC client",
                ));
            }
        };

        // Convert request into vLLM ScoreRequest proto
        let request_id = "score_id".to_string(); // or capture from elsewhere

        // text_2 is StringOrArray, normalize it to Vec<String>
        let text_2 = match &score_req.text_2 {
            StringOrArray::String(s) => vec![s.clone()],
            StringOrArray::Array(arr) => arr.clone(),
        };

        let proto_req =
            match client.build_score_request(request_id, score_req.text_1.clone(), text_2) {
                Ok(req) => req,
                Err(e) => {
                    error!(stage = self.name(), error = %e, "Backend unsupported for Score");
                    return Err(error::internal_error(
                        "score_backend_unsupported",
                        "Score is only supported on vLLM backend",
                    ));
                }
            };

        debug!(
            stage = self.name(),
            model = %score_req.model,
            "Forwarding Score via native gRPC"
        );

        let grpc_response = match client.score(proto_req).await {
            Ok(r) => r,
            Err(e) => {
                error!(stage = self.name(), error = %e, "native gRPC Score failed");
                return Err(crate::routers::grpc::utils::handle_grpc_error(e));
            }
        };

        // Put the client back into context so it drops with the right lifetime optionally
        ctx.state.client = Some(ClientState::Single(client));

        // Assemble protocol response
        let mut results = Vec::with_capacity(grpc_response.data.len());
        for res in grpc_response.data {
            results.push(openai_protocol::rerank::ScoreResult {
                index: res.index as usize,
                score: res.score,
                object: "score".to_string(),
            });
        }

        let resp = openai_protocol::rerank::ScoreResponse {
            id: format!("score-{}", uuid::Uuid::new_v4()),
            object: "list".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: score_req.model.clone(),
            data: results,
            usage: openai_protocol::rerank::ScoreUsage {
                prompt_tokens: grpc_response.prompt_tokens,
                total_tokens: grpc_response.total_tokens,
                completion_tokens: 0,
                prompt_tokens_details: None,
            },
        };

        let axum_response = (StatusCode::OK, axum::Json(resp)).into_response();
        Ok(Some(axum_response))
    }
}
