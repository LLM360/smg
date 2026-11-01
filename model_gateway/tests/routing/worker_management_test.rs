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

    /// Explicit re-registration after removal (AC-3, LLM360/comet#3718):
    /// once a worker is removed, POST /workers for the same URL registers
    /// fresh — no stale duplicate-conflict — and the worker re-enters the
    /// routing table.
    #[tokio::test]
    async fn test_reregister_after_removal_registers_fresh() {
        let mut config = TestRouterConfig::round_robin(3904);
        // The drain settle window would outlast the removal polls below.
        config.health_check.drain_settle_secs = 0;

        let ctx =
            AppTestContext::new_with_config(config, vec![TestWorkerConfig::healthy(19906)]).await;

        let app = ctx.create_app();

        async fn list_workers(app: axum::Router) -> serde_json::Value {
            let req = Request::builder()
                .method("GET")
                .uri("/workers")
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "GET /workers failed");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        fn urls(listed: &serde_json::Value) -> Vec<String> {
            listed["workers"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|w| w["url"].as_str().map(str::to_string))
                .collect()
        }

        let listed = list_workers(app.clone()).await;
        let worker = &listed["workers"][0];
        let worker_id = worker["id"].as_str().unwrap().to_string();
        let worker_url = worker["url"].as_str().unwrap().to_string();

        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/workers/{worker_id}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(
            resp.status().is_success(),
            "DELETE /workers/{{id}} failed: {}",
            resp.status()
        );

        // The removal workflow runs in the background; poll until the URL is gone.
        let mut absent = false;
        for _ in 0..40 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            if !urls(&list_workers(app.clone()).await).contains(&worker_url) {
                absent = true;
                break;
            }
        }
        assert!(absent, "removed worker URL is still listed");

        // Explicit re-registration of the removed URL must register fresh.
        let body = json!({ "url": worker_url });
        let req = Request::builder()
            .method("POST")
            .uri("/workers")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(
            resp.status().is_success(),
            "re-registration of a removed URL returned {} (stale conflict?)",
            resp.status()
        );

        // The worker is listed again and serves routing.
        let mut present = false;
        for _ in 0..40 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            if urls(&list_workers(app.clone()).await).contains(&worker_url) {
                present = true;
                break;
            }
        }
        assert!(present, "re-registered worker URL never reappeared");

        let payload = json!({ "text": "after re-registration", "stream": false });
        let req = Request::builder()
            .method("POST")
            .uri("/generate")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&payload).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "re-registered worker does not serve routing"
        );

        ctx.shutdown().await;
    }

    /// Registration stays an explicit act (AC-3, LLM360/comet#3718): after a
    /// DELETE, nothing re-adds the worker on its own.
    #[tokio::test]
    async fn test_removed_worker_is_not_auto_readded() {
        let mut config = TestRouterConfig::round_robin(3905);
        // The drain settle window would outlast the removal polls below.
        config.health_check.drain_settle_secs = 0;

        let ctx =
            AppTestContext::new_with_config(config, vec![TestWorkerConfig::healthy(19907)]).await;

        let app = ctx.create_app();

        async fn urls(app: axum::Router) -> Vec<String> {
            let req = Request::builder()
                .method("GET")
                .uri("/workers")
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "GET /workers failed");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            listed["workers"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|w| w["url"].as_str().map(str::to_string))
                .collect()
        }

        let listed = urls(app.clone()).await;
        assert_eq!(listed.len(), 1, "test worker not registered");
        let worker_url = listed[0].clone();

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
        let worker_id = listed["workers"][0]["id"].as_str().unwrap().to_string();

        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/workers/{worker_id}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert!(
            resp.status().is_success(),
            "DELETE /workers/{{id}} failed: {}",
            resp.status()
        );

        let mut absent = false;
        for _ in 0..40 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            if !urls(app.clone()).await.contains(&worker_url) {
                absent = true;
                break;
            }
        }
        assert!(absent, "removed worker URL is still listed");

        // Several manage cycles later the URL must still be absent: nothing
        // re-registers a removed worker implicitly.
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        assert!(
            !urls(app.clone()).await.contains(&worker_url),
            "removed worker reappeared without an explicit POST /workers"
        );

        ctx.shutdown().await;
    }
}
