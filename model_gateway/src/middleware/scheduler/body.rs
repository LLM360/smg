//! The scheduler's permit as the edge's body duty.
//!
//! The edge's one body wrapper (`edge_body`) holds the permit of the
//! request for the lifetime of the response stream. [`SchedulerBodyPermit`]
//! adds what the fair-share ledger and the HTTP admission metrics need at the
//! end of the body:
//!
//! - **TTFT.** The first data frame marks time-to-first-byte through the
//!   permit's CAS. If the scheduler already won the preemption CAS, the
//!   wrapper ends the stream before the first byte.
//! - **Settlement.** When the body completes, the output tokens that the
//!   response reported replace the provisional fair-share charge. A body that
//!   fails, or that the client closes, keeps the provisional charge.
//! - **Admission outcome.** A completed body records its status in the
//!   `smg_http_admission_*` metrics. Any other end records an interruption.
//! - **Slot release.** The drop of the permit releases the slot.

use std::collections::VecDeque;

use axum::http::StatusCode;

use super::{engine::SchedulerPermit, output_tokens::observed_output_tokens, SettlementKind};
use crate::middleware::{admission_metrics::AdmissionActiveGuard, edge_body::Permit};

const FAIR_SHARE_RESPONSE_TAIL_LIMIT: usize = 64 * 1024;

/// A [`SchedulerPermit`] that rides on the response body.
pub(crate) struct SchedulerBodyPermit {
    permit: SchedulerPermit,
    active_guard: AdmissionActiveGuard,
    status_code: StatusCode,
    completed: bool,
    /// Tail of the response, kept only while a fair-share reservation exists,
    /// to read the terminal `usage` object.
    response_tail: Option<VecDeque<u8>>,
    fair_share_settled: bool,
}

impl SchedulerBodyPermit {
    pub(crate) fn new(
        permit: SchedulerPermit,
        active_guard: AdmissionActiveGuard,
        status_code: StatusCode,
    ) -> Self {
        let response_tail = permit
            .has_fair_share_reservation()
            .then(|| VecDeque::with_capacity(FAIR_SHARE_RESPONSE_TAIL_LIMIT));
        let fair_share_settled = response_tail.is_none();
        Self {
            permit,
            active_guard,
            status_code,
            completed: false,
            response_tail,
            fair_share_settled,
        }
    }

    fn append_response_tail(&mut self, data: &[u8]) {
        let Some(response_tail) = self.response_tail.as_mut() else {
            return;
        };
        if data.len() >= FAIR_SHARE_RESPONSE_TAIL_LIMIT {
            response_tail.clear();
            response_tail.extend(
                data[data.len().saturating_sub(FAIR_SHARE_RESPONSE_TAIL_LIMIT)..]
                    .iter()
                    .copied(),
            );
            return;
        }
        let overflow = response_tail
            .len()
            .saturating_add(data.len())
            .saturating_sub(FAIR_SHARE_RESPONSE_TAIL_LIMIT);
        for _ in 0..overflow {
            response_tail.pop_front();
        }
        response_tail.extend(data.iter().copied());
    }

    fn settle_completed_response(&mut self) {
        if self.fair_share_settled {
            return;
        }
        let observed = self
            .response_tail
            .as_mut()
            .and_then(|tail| observed_output_tokens(tail.make_contiguous()));
        let kind = if observed.is_some() {
            SettlementKind::Observed
        } else {
            SettlementKind::MissingUsage
        };
        self.permit.settle_output_tokens(observed, kind);
        self.fair_share_settled = true;
    }
}

impl Permit for SchedulerBodyPermit {
    fn first_data_frame(&mut self) -> bool {
        self.permit.try_mark_first_byte()
    }

    fn data_frame(&mut self, data: &[u8]) {
        self.append_response_tail(data);
    }

    fn body_finished(&mut self) {
        self.completed = true;
        self.settle_completed_response();
    }
}

impl Drop for SchedulerBodyPermit {
    fn drop(&mut self) {
        if self.completed {
            self.active_guard.record_outcome(self.status_code.as_u16());
        } else {
            self.active_guard.record_interrupted();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use axum::{body::Body, response::Response};
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use smg_auth::RequestId;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        middleware::{
            edge_body::{at_the_edge, BodyDuties},
            scheduler::{
                AdmitOutcome, Class, ClassConfig, FairShareConfig, GlobalFairShare,
                PriorityScheduler, PrioritySchedulerYaml, SchedulerSettings,
            },
        },
        tenant::TenantKey,
    };

    /// `body` with the permit as the admission layer leaves it, at the edge.
    fn guarded(inner: Body, permit: SchedulerPermit) -> Body {
        let mut response = Response::new(inner);
        BodyDuties::hand(&mut response, |duties| {
            duties.permit = Some(Box::new(SchedulerBodyPermit::new(
                permit,
                AdmissionActiveGuard::new(),
                StatusCode::OK,
            )));
        });
        at_the_edge(response).into_body()
    }

    fn fair_scheduler(
        default_output_tokens: u32,
    ) -> (Arc<PriorityScheduler>, Arc<GlobalFairShare>, TenantKey, u64) {
        let mut classes = HashMap::new();
        for class in Class::ALL {
            let mut config = ClassConfig::default_for(class);
            config.reserved_floor = 0;
            config.reserved_per_slot = 0.0;
            config.queue_size = 8;
            classes.insert(class, config);
        }
        let yaml = PrioritySchedulerYaml {
            classes,
            fair_share: Some(FairShareConfig {
                default_weight: 1.0,
                default_output_tokens,
                trust_output_token_estimate_header: false,
                trust_request_model_header: false,
                tenant_weights: HashMap::from([("header:alice".to_string(), 1.0)]),
                model_profiles: HashMap::new(),
            }),
            ..Default::default()
        };
        let settings =
            SchedulerSettings::from_cli_and_yaml(true, Class::Default, 32, Some(&yaml)).unwrap();
        let ledger = Arc::new(GlobalFairShare::from_settings(&settings).unwrap());
        let scheduler =
            PriorityScheduler::new_with_fair_share(&settings, 1, Some(Arc::clone(&ledger)))
                .unwrap();
        let scope_id = scheduler
            .fair_share_scope_for_test()
            .expect("fair-share scheduler has a scope");
        (scheduler, ledger, TenantKey::new("header:alice"), scope_id)
    }

    async fn fair_permit(
        scheduler: &Arc<PriorityScheduler>,
        tenant: &TenantKey,
        id: &str,
        estimate: u32,
    ) -> SchedulerPermit {
        let outcome = scheduler
            .admit_for_tenant(
                Class::Default,
                RequestId(id.to_string()),
                CancellationToken::new(),
                tenant.clone(),
                estimate,
            )
            .await;
        let AdmitOutcome::Admitted(permit) = outcome else {
            panic!("request should be admitted");
        };
        permit
    }

    #[tokio::test]
    async fn terminal_usage_settles_actual_output_tokens() {
        let (scheduler, ledger, tenant, scope_id) = fair_scheduler(100);
        let permit = fair_permit(&scheduler, &tenant, "usage", 100).await;
        guarded(Body::from(r#"{"usage":{"completion_tokens":37}}"#), permit)
            .collect()
            .await
            .unwrap();
        assert_eq!(ledger.snapshot(scope_id, &tenant).0, 37);
    }

    #[tokio::test]
    async fn split_sse_frames_are_reassembled_for_terminal_usage() {
        let (scheduler, ledger, tenant, scope_id) = fair_scheduler(100);
        let permit = fair_permit(&scheduler, &tenant, "split-sse", 100).await;
        let frames: Vec<Result<Bytes, std::io::Error>> = [
            "data: {\"choices\":[]",
            ",\"usage\":{\"completion_",
            "tokens\":23}}\n\n",
            "data: [DONE]\n\n",
        ]
        .into_iter()
        .map(|chunk| Ok(Bytes::from(chunk)))
        .collect();
        let body = Body::from_stream(tokio_stream::iter(frames));
        guarded(body, permit).collect().await.unwrap();
        assert_eq!(ledger.snapshot(scope_id, &tenant).0, 23);
    }

    #[tokio::test]
    async fn client_drop_before_usage_keeps_the_provisional_charge() {
        let (scheduler, ledger, tenant, scope_id) = fair_scheduler(100);
        let permit = fair_permit(&scheduler, &tenant, "client-drop", 100).await;
        let frames = vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"data: {\"choices\":[]}\n\n")),
            Ok(Bytes::from_static(b"data: more\n\n")),
        ];
        let mut body = guarded(Body::from_stream(tokio_stream::iter(frames)), permit);
        assert!(body.frame().await.unwrap().unwrap().is_data());
        drop(body);
        assert_eq!(ledger.snapshot(scope_id, &tenant).0, 100);
    }

    #[tokio::test]
    async fn backend_error_keeps_the_provisional_charge() {
        let (scheduler, ledger, tenant, scope_id) = fair_scheduler(100);
        let permit = fair_permit(&scheduler, &tenant, "backend-error", 100).await;
        let frames = vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"data: partial\n\n")),
            Err(std::io::Error::other("backend failed")),
        ];
        let mut body = guarded(Body::from_stream(tokio_stream::iter(frames)), permit);
        assert!(body.frame().await.unwrap().unwrap().is_data());
        assert!(body.frame().await.unwrap().is_err());
        drop(body);
        assert_eq!(ledger.snapshot(scope_id, &tenant).0, 100);
    }

    #[tokio::test]
    async fn many_small_frames_stay_bounded_and_parse_usage_after_ring_wrap() {
        let (scheduler, ledger, tenant, scope_id) = fair_scheduler(100);
        let permit = fair_permit(&scheduler, &tenant, "ring-wrap", 100).await;
        let mut frames: Vec<Result<Bytes, std::io::Error>> = (0..9_000)
            .map(|_| Ok(Bytes::from_static(b"data: filler\n\n")))
            .collect();
        for chunk in [
            "data: {\"choices\":[],\"usage\":{\"comple",
            "tion_tokens\":47}}\n\n",
            "data: [DONE]\n\n",
        ] {
            frames.push(Ok(Bytes::from(chunk)));
        }
        let mut body = guarded(Body::from_stream(tokio_stream::iter(frames)), permit);
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
        drop(body);
        assert_eq!(ledger.snapshot(scope_id, &tenant).0, 47);
    }
}
