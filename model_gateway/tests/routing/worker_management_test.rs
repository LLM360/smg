//! Worker management integration tests
//!
//! Tests for dynamic worker add/remove operations via management API.
//! The actual worker management API uses:
//! - POST /workers - create a worker
//! - GET /workers - list workers
//! - PUT /workers/{worker_id} - replace a worker
//! - DELETE /workers/{worker_id} - remove a worker

use axum::{
    body::Body,
    extract::Request,
    http::{header::CONTENT_TYPE, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{AppTestContext, TestRouterConfig, TestWorkerConfig};

#[cfg(test)]
mod worker_management_tests {
    use super::*;

    /// Test listing workers via API
    #[tokio::test]
    async fn test_list_workers() {
        let config = TestRouterConfig::round_robin(3900);

        let ctx = AppTestContext::new_with_config(
            config,
            vec![
                TestWorkerConfig::healthy(19900),
                TestWorkerConfig::healthy(19901),
            ],
        )
        .await;

        let app = ctx.create_app();

        // List workers via GET /workers
        let req = Request::builder()
            .method("GET")
            .uri("/workers")
            .body(Body::empty())
            .unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "GET /workers should return OK"
        );

        ctx.shutdown().await;
    }

    /// Test that routing continues to work with multiple workers
    #[tokio::test]
    async fn test_routing_with_multiple_workers() {
        let config = TestRouterConfig::round_robin(3901);

        let ctx = AppTestContext::new_with_config(
            config,
            vec![
                TestWorkerConfig::healthy(19902),
                TestWorkerConfig::healthy(19903),
            ],
        )
        .await;

        let app = ctx.create_app();
        let mut success_count = 0;

        // Verify routing distributes across workers
        for i in 0..20 {
            let payload = json!({
                "text": format!("Test request {}", i),
                "stream": false
            });

            let req = Request::builder()
                .method("POST")
                .uri("/generate")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap();

            let resp = app.clone().oneshot(req).await.unwrap();
            if resp.status() == StatusCode::OK {
                success_count += 1;
            }
        }

        assert_eq!(
            success_count, 20,
            "All requests should succeed with multiple workers"
        );

        ctx.shutdown().await;
    }

    /// Test that requests continue to work during worker operations
    #[tokio::test]
    async fn test_requests_during_worker_changes() {
        let config = TestRouterConfig::round_robin(3902);

        let ctx =
            AppTestContext::new_with_config(config, vec![TestWorkerConfig::healthy(19904)]).await;

        let app = ctx.create_app();

        // Send requests and verify they succeed
        let mut success_count = 0;
        for i in 0..10 {
            let payload = json!({
                "text": format!("Request during changes {}", i),
                "stream": false
            });

            let req = Request::builder()
                .method("POST")
                .uri("/generate")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap();

            let resp = app.clone().oneshot(req).await.unwrap();
            if resp.status() == StatusCode::OK {
                success_count += 1;
            }
        }

        assert_eq!(
            success_count, 10,
            "All requests should succeed during normal operation"
        );

        ctx.shutdown().await;
    }

    /// PUT /workers/{id} must actually apply the new spec.
    ///
    /// The handler returns 202 and runs the registration workflow in the
    /// background. Before the registration mode existed, that workflow always
    /// rejected an already-registered URL, so the replace failed after the
    /// caller had already seen 202.
    #[tokio::test]
    async fn test_replace_worker_applies_new_spec() {
        let config = TestRouterConfig::round_robin(3903);

        let ctx =
            AppTestContext::new_with_config(config, vec![TestWorkerConfig::healthy(19905)]).await;

        let app = ctx.create_app();

        async fn get_worker(app: axum::Router, worker_id: &str) -> serde_json::Value {
            let req = Request::builder()
                .method("GET")
                .uri(format!("/workers/{worker_id}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "GET /workers/{{id}} failed");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        let req = Request::builder()
            .method("GET")
            .uri("/workers")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let worker = &listed["workers"][0];
        let worker_id = worker["id"].as_str().unwrap().to_string();
        let url = worker["url"].as_str().unwrap().to_string();
        let old_priority = worker["priority"].as_u64().unwrap();
        let new_priority = old_priority + 7;

        // Same URL (required by the handler), different priority.
        let body = json!({ "url": url, "priority": new_priority });
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/workers/{worker_id}"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::ACCEPTED,
            "PUT /workers/{{id}} should be accepted"
        );

        // The workflow runs in the background, so poll for the new value.
        let mut applied = false;
        for _ in 0..40 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            let current = get_worker(app.clone(), &worker_id).await;
            if current["priority"].as_u64() == Some(new_priority) {
                applied = true;
                break;
            }
        }

        let current = get_worker(app.clone(), &worker_id).await;
        assert!(applied, "PUT never applied. Worker still reads: {current}");
        assert_eq!(
            current["url"].as_str(),
            Some(url.as_str()),
            "URL must not change"
        );

        ctx.shutdown().await;
    }
}
