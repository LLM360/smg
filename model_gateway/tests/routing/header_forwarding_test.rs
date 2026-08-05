//! Header forwarding integration tests
//!
//! Tests for header propagation through the router to workers.

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{header::CONTENT_TYPE, StatusCode},
};
use serde_json::json;
use smg::config::RouterConfig;
use tower::ServiceExt;

use crate::common::{
    mock_worker::{HealthStatus, MockWorkerConfig, WorkerType},
    AppTestContext, TestRouterConfig, TestWorkerConfig,
};

#[cfg(test)]
mod header_forwarding_tests {
    use super::*;

    const POOL_HEADER: &str = "x-comet-pool-component-id";

    async fn set_pool_identity(app: &axum::Router, worker_url: &str, value: &str) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/workers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let worker_id = body["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["url"] == worker_url)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let update = json!({"labels": {"comet_pool_component_id": value}});
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/workers/{worker_id}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(update.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        for _ in 0..40 {
            let worker = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/workers/{worker_id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(worker.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if body["labels"]["comet_pool_component_id"] == value {
                return;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        }
        panic!("worker label update did not finish");
    }

    async fn generate(app: &axum::Router, stream: bool) -> axum::response::Response {
        let payload = json!({"text": "serving identity", "stream": stream});
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/generate")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_buffered_and_streaming_responses_report_selected_pool() {
        let worker = TestWorkerConfig::healthy(19410);
        let ctx = AppTestContext::new(vec![worker]).await;
        let app = ctx.create_app();
        let pool_id = "a".repeat(64);
        set_pool_identity(&app, "http://127.0.0.1:19410", &pool_id).await;

        for stream in [false, true] {
            let response = generate(&app, stream).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[POOL_HEADER], pool_id);
        }

        ctx.shutdown().await;
    }

    #[tokio::test]
    async fn test_retry_reports_the_worker_that_succeeds() {
        let config = TestRouterConfig::round_robin_with_retry(
            3110,
            smg::config::RetryConfig {
                max_retries: 2,
                initial_backoff_ms: 1,
                max_backoff_ms: 1,
                jitter_factor: 0.0,
                ..Default::default()
            },
        );
        let ctx = AppTestContext::new_with_config(
            config,
            vec![
                TestWorkerConfig::flaky(19411, 1.0),
                TestWorkerConfig::healthy(19412),
            ],
        )
        .await;
        let app = ctx.create_app();
        set_pool_identity(&app, "http://127.0.0.1:19411", &"a".repeat(64)).await;
        set_pool_identity(&app, "http://127.0.0.1:19412", &"b".repeat(64)).await;

        let response = generate(&app, false).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[POOL_HEADER], "b".repeat(64));
        ctx.shutdown().await;
    }

    #[tokio::test]
    async fn test_missing_or_malformed_pool_identity_is_omitted() {
        let ctx = AppTestContext::new(vec![TestWorkerConfig::healthy(19413)]).await;
        let app = ctx.create_app();
        assert!(!generate(&app, false)
            .await
            .headers()
            .contains_key(POOL_HEADER));

        for value in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            set_pool_identity(&app, "http://127.0.0.1:19413", &value).await;
            assert!(!generate(&app, false)
                .await
                .headers()
                .contains_key(POOL_HEADER));
        }

        ctx.shutdown().await;
    }

    /// Test that X-Request-Id header is forwarded
    #[tokio::test]
    async fn test_request_id_forwarding() {
        let ctx = AppTestContext::new(vec![MockWorkerConfig {
            port: 19400,
            worker_type: WorkerType::Regular,
            health_status: HealthStatus::Healthy,
            response_delay_ms: 0,
            fail_rate: 0.0,
        }])
        .await;

        let app = ctx.create_app();

        let custom_request_id = "test-request-id-12345";
        let payload = json!({
            "text": "Test header forwarding",
            "stream": false
        });

        let req = Request::builder()
            .method("POST")
            .uri("/generate")
            .header(CONTENT_TYPE, "application/json")
            .header("x-request-id", custom_request_id)
            .body(Body::from(serde_json::to_string(&payload).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Response should have the same request ID
        let response_id = resp.headers().get("x-request-id");
        assert!(response_id.is_some(), "Response should have x-request-id");
        assert_eq!(
            response_id.unwrap().to_str().unwrap(),
            custom_request_id,
            "Request ID should be preserved"
        );

        ctx.shutdown().await;
    }

    /// Test custom request ID headers
    #[tokio::test]
    async fn test_custom_request_id_headers() {
        let mut config = RouterConfig::builder()
            .regular_mode(vec![])
            .random_policy()
            .host("127.0.0.1")
            .port(3500)
            .max_payload_size(256 * 1024 * 1024)
            .request_timeout_secs(600)
            .worker_startup_timeout_secs(5)
            .worker_startup_check_interval_secs(1)
            .max_concurrent_requests(64)
            .queue_timeout_secs(60)
            .request_id_headers(vec![
                "custom-trace-id".to_string(),
                "x-correlation-id".to_string(),
            ])
            .build_unchecked();
        config.health_check.disable_health_check = true;

        let ctx = AppTestContext::new_with_config(
            config,
            vec![MockWorkerConfig {
                port: 19401,
                worker_type: WorkerType::Regular,
                health_status: HealthStatus::Healthy,
                response_delay_ms: 0,
                fail_rate: 0.0,
            }],
        )
        .await;

        let app = ctx.create_app();

        let custom_trace_id = "my-custom-trace-123";
        let payload = json!({
            "text": "Test custom headers",
            "stream": false
        });

        let req = Request::builder()
            .method("POST")
            .uri("/generate")
            .header(CONTENT_TYPE, "application/json")
            .header("custom-trace-id", custom_trace_id)
            .body(Body::from(serde_json::to_string(&payload).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Response should use the custom header as request ID
        let response_id = resp.headers().get("x-request-id");
        assert!(response_id.is_some());
        assert_eq!(
            response_id.unwrap().to_str().unwrap(),
            custom_trace_id,
            "Custom header should be used as request ID"
        );

        ctx.shutdown().await;
    }

    /// Test correlation ID header forwarding
    #[tokio::test]
    async fn test_correlation_id_forwarding() {
        let ctx = AppTestContext::new(vec![MockWorkerConfig {
            port: 19402,
            worker_type: WorkerType::Regular,
            health_status: HealthStatus::Healthy,
            response_delay_ms: 0,
            fail_rate: 0.0,
        }])
        .await;

        let app = ctx.create_app();

        let correlation_id = "correlation-abc-789";
        let payload = json!({
            "text": "Test correlation ID",
            "stream": false
        });

        let req = Request::builder()
            .method("POST")
            .uri("/generate")
            .header(CONTENT_TYPE, "application/json")
            .header("x-correlation-id", correlation_id)
            .body(Body::from(serde_json::to_string(&payload).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Response should preserve the correlation ID as request ID
        let response_id = resp.headers().get("x-request-id");
        assert!(response_id.is_some());
        assert_eq!(
            response_id.unwrap().to_str().unwrap(),
            correlation_id,
            "Correlation ID should be preserved"
        );

        ctx.shutdown().await;
    }

    /// Test that request ID is generated when not provided
    #[tokio::test]
    async fn test_auto_generated_request_id() {
        let ctx = AppTestContext::new(vec![MockWorkerConfig {
            port: 19403,
            worker_type: WorkerType::Regular,
            health_status: HealthStatus::Healthy,
            response_delay_ms: 0,
            fail_rate: 0.0,
        }])
        .await;

        let app = ctx.create_app();

        let payload = json!({
            "text": "Test auto-generated ID",
            "stream": false
        });

        let req = Request::builder()
            .method("POST")
            .uri("/generate")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&payload).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Response should have an auto-generated request ID
        let response_id = resp.headers().get("x-request-id");
        assert!(
            response_id.is_some(),
            "Response should have auto-generated x-request-id"
        );

        let id_value = response_id.unwrap().to_str().unwrap();
        assert!(!id_value.is_empty(), "Request ID should not be empty");
        // For generate endpoint, ID should have 'gnt-' prefix
        assert!(
            id_value.starts_with("gnt-"),
            "Generate endpoint should have gnt- prefix, got: {id_value}"
        );

        ctx.shutdown().await;
    }

    /// Test chat completions request ID format
    #[tokio::test]
    async fn test_chat_completions_request_id_format() {
        let ctx = AppTestContext::new(vec![MockWorkerConfig {
            port: 19404,
            worker_type: WorkerType::Regular,
            health_status: HealthStatus::Healthy,
            response_delay_ms: 0,
            fail_rate: 0.0,
        }])
        .await;

        let app = ctx.create_app();

        let payload = json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        });

        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&payload).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Response should have chatcmpl- prefix for chat completions
        let response_id = resp.headers().get("x-request-id");
        assert!(response_id.is_some());

        let id_value = response_id.unwrap().to_str().unwrap();
        assert!(
            id_value.starts_with("chatcmpl-"),
            "Chat completions should have chatcmpl- prefix, got: {id_value}"
        );

        ctx.shutdown().await;
    }

    /// Test multiple header priorities
    #[tokio::test]
    async fn test_header_priority() {
        let ctx = AppTestContext::new(vec![MockWorkerConfig {
            port: 19405,
            worker_type: WorkerType::Regular,
            health_status: HealthStatus::Healthy,
            response_delay_ms: 0,
            fail_rate: 0.0,
        }])
        .await;

        let app = ctx.create_app();

        let primary_id = "primary-request-id";
        let fallback_id = "fallback-correlation-id";
        let payload = json!({
            "text": "Test header priority",
            "stream": false
        });

        // When x-request-id is provided, it should take priority
        let req = Request::builder()
            .method("POST")
            .uri("/generate")
            .header(CONTENT_TYPE, "application/json")
            .header("x-request-id", primary_id)
            .header("x-correlation-id", fallback_id)
            .body(Body::from(serde_json::to_string(&payload).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let response_id = resp.headers().get("x-request-id");
        assert!(response_id.is_some());
        assert_eq!(
            response_id.unwrap().to_str().unwrap(),
            primary_id,
            "x-request-id should take priority"
        );

        ctx.shutdown().await;
    }
}
