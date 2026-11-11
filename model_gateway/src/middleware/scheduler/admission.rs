//! `priority_admission_middleware`: the axum layer that runs the priority
//! scheduler on protected routes when it is enabled.
//!
//! Pipeline position (route_layer order): runs after tenant resolution
//! (so `RouteRequestMeta.tenant_key` is in extensions for the clamp) and
//! before the handler. On admission it inserts the permit's cancel token
//! into request extensions (so long-running handlers can `select!` against
//! it for preemption) and hands the permit to the edge's one body wrapper
//! (TTFT marking on the first data frame, slot release when the body ends).

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use smg_auth::RequestId;
use tokio_util::sync::CancellationToken;
use tracing::trace;

use super::{
    capacity_credit::CapacityCreditError,
    capacity_credit_api::{
        capacity_credit_error_response, capacity_credit_required_response,
        presented_capacity_credit,
    },
    fair_share::FairShareProfile,
    body::SchedulerBodyPermit,
    metrics as sched_metrics,
    state::SchedulerState,
    AdmitOutcome, Class, GlobalFairShare, RejectionReason, SchedulerError,
    SchedulerPermit, HEADER_X_SMG_PREEMPTED, OUTPUT_TOKEN_ESTIMATE_HEADER,
    PRIORITY_HEADER,
};
use crate::{
    middleware::{
        admission_metrics::{AdmissionActiveGuard, AdmissionPendingGuard},
        edge_body::BodyDuties,
        tenant_resolution::restamp_accepted_at,
        RouteRequestMeta,
    },
    observability::metrics::{metrics_labels, Metrics},
    tenant::TenantKey,
};

/// Monotonic source of registry keys for admitted requests. Each admission
/// gets a unique key, so the inflight registry can never be clobbered by a
/// duplicate client-supplied `x-request-id` (the collision hazard flagged
/// in earlier review). The client request id is still used for logging.
static NEXT_ADMISSION_ID: AtomicU64 = AtomicU64::new(0);

fn next_registry_id() -> RequestId {
    let n = NEXT_ADMISSION_ID.fetch_add(1, Ordering::Relaxed);
    RequestId(format!("sched-{n}"))
}

/// The class a request resolves to, plus what it asked for.
struct ResolvedPriority {
    /// Post-clamp class the request is admitted under.
    effective: Class,
    /// Class parsed from the header before the tenant clamp.
    requested: Class,
    /// Header carried a non-empty value that didn't name a known class.
    unknown: bool,
}

/// Resolve the effective class: parse the priority header, then clamp it
/// down to the tenant's configured `max_class` (a low-tier tenant cannot
/// self-promote by setting the header). `min` is the clamp because of the
/// `Ord` derive on `Class`.
fn resolve_priority(
    req: &Request<Body>,
    state: &SchedulerState,
    tenant: &TenantKey,
) -> ResolvedPriority {
    let raw = req
        .headers()
        .get(PRIORITY_HEADER)
        .and_then(|h| h.to_str().ok());
    let requested = raw.map(Class::parse_header).unwrap_or(Class::Default);
    // Unknown = present, non-empty, not "default", yet still parsed to
    // Default (i.e. an unrecognized value silently downgraded).
    let unknown = raw.map(str::trim).is_some_and(|v| {
        !v.is_empty() && !v.eq_ignore_ascii_case("default") && requested == Class::Default
    });
    let max_class = state.resolver.policy(tenant).max_class;
    ResolvedPriority {
        effective: requested.min(max_class),
        requested,
        unknown,
    }
}

/// Map an admit rejection to an `admit_total` outcome label.
fn rejection_outcome(reason: RejectionReason) -> &'static str {
    match reason {
        RejectionReason::QueueFull => sched_metrics::outcome::REJECTED_QUEUE_FULL,
        RejectionReason::QueueTimeout => sched_metrics::outcome::REJECTED_QUEUE_TIMEOUT,
        RejectionReason::Preempted => sched_metrics::outcome::PREEMPTED,
        RejectionReason::ClientCancelled => sched_metrics::outcome::CLIENT_CANCELLED,
    }
}

fn output_token_estimate(headers: &HeaderMap, ledger: &GlobalFairShare) -> u32 {
    let configured_default = ledger.default_output_tokens();
    let header = headers.get(OUTPUT_TOKEN_ESTIMATE_HEADER);
    if !ledger.trusts_output_token_estimate_header() {
        if header.is_some() {
            sched_metrics::record_fair_share_fallback("untrusted_estimate");
        }
        return configured_default;
    }
    match header {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or_else(|| {
                sched_metrics::record_fair_share_fallback("invalid_estimate");
                configured_default
            }),
        None => {
            sched_metrics::record_fair_share_fallback("missing_estimate");
            configured_default
        }
    }
}

async fn run_admitted_request(
    mut req: Request<Body>,
    next: Next,
    mut permit: SchedulerPermit,
    class: Class,
    requested_class: Class,
    tenant: &TenantKey,
    partition: &super::state::SchedulerPartition,
) -> Response {
    Metrics::record_http_admission_admitted();
    let active_guard = AdmissionActiveGuard::new();
    req.extensions_mut().insert(permit.cancel_token());
    if let Some(proof) = permit.fair_share_admission_proof() {
        let Some(meta) = req.extensions().get::<RouteRequestMeta>().cloned() else {
            permit.cancel_fair_share_reservation();
            Metrics::record_http_admission_rejected();
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            return response;
        };
        req.extensions_mut().insert(meta.with_extension(proof));
    }
    // The class's queue wait was the scheduler's, bounded by its own timeout;
    // the bound ahead of worker selection counts from here.
    restamp_accepted_at(req.extensions_mut());
    let mut response = next.run(req).await;
    let outcome = if response.headers().contains_key(HEADER_X_SMG_PREEMPTED) {
        sched_metrics::outcome::PREEMPTED
    } else {
        sched_metrics::outcome::ADMITTED
    };
    sched_metrics::record_admit(class, outcome);
    sched_metrics::record_partition_admit(&partition.name, class, outcome);
    trace!(
        scheduler.class = class.as_str(),
        scheduler.requested_class = requested_class.as_str(),
        scheduler.tenant = %tenant,
        scheduler.partition = %partition.name,
        scheduler.admit_outcome = outcome,
        "scheduler admission decision"
    );
    carry_permit(&mut response, permit, active_guard);
    response
}

pub async fn priority_admission_middleware(
    State(state): State<Arc<SchedulerState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    Metrics::record_http_admission_received();
    let mut pending_guard = AdmissionPendingGuard::new();
    let tenant = req
        .extensions()
        .get::<RouteRequestMeta>()
        .map(|m| m.tenant_key().clone())
        .unwrap_or_else(|| TenantKey::new("anonymous"));
    let route_request_id = req
        .extensions()
        .get::<RouteRequestMeta>()
        .map(RouteRequestMeta::request_charge_id);

    // A credit already owns a scheduler slot. Redeem it before the sibling
    // RPS check so any local rejection can drop the held permit immediately
    // instead of stranding capacity until the credit TTL expires.
    let held_credit = match presented_capacity_credit(req.headers(), &tenant, &state) {
        Ok(Some(presented)) => {
            let Some(route_request_id) = route_request_id else {
                Metrics::record_http_admission_rejected();
                pending_guard.resolve();
                return capacity_credit_error_response(CapacityCreditError::InternalState, None);
            };
            let Some(registry) = state.capacity_credits() else {
                Metrics::record_http_admission_rejected();
                pending_guard.resolve();
                return capacity_credit_error_response(CapacityCreditError::Unknown, None);
            };
            match registry.redeem(&presented.token, &presented.binding) {
                Ok(mut redeemed) => {
                    if !redeemed
                        .payload
                        .bind_redeemed_capacity_credit(&redeemed.binding, route_request_id)
                    {
                        Metrics::record_http_admission_rejected();
                        pending_guard.resolve();
                        return capacity_credit_error_response(
                            CapacityCreditError::InternalState,
                            None,
                        );
                    }
                    Some((redeemed.payload, presented.partition))
                }
                Err(error) => {
                    Metrics::record_http_admission_rejected();
                    pending_guard.resolve();
                    return capacity_credit_error_response(error, None);
                }
            }
        }
        Ok(None) if state.capacity_credit_required() => {
            Metrics::record_http_admission_rejected();
            pending_guard.resolve();
            return capacity_credit_required_response();
        }
        Ok(None) => None,
        Err(error) => {
            Metrics::record_http_admission_rejected();
            pending_guard.resolve();
            return capacity_credit_error_response(error, None);
        }
    };

    // RPS sibling check (only set when an explicit per-second limit is
    // configured). Checked before admission so a rejected request never
    // acquires a new slot. A redeemed credit already owns its slot and is
    // dropped here on rejection. Tokens are not returned; refill is time-based.
    // Tracked by smg_http_rate_limit_total, so not double-counted here.
    if let Some(bucket) = &state.rate_limiter {
        if bucket.try_take(1.0).is_err() {
            drop(held_credit);
            Metrics::record_http_rate_limit(metrics_labels::RATE_LIMIT_REJECTED);
            Metrics::record_http_admission_rejected();
            pending_guard.resolve();
            return crate::rate_limit::rejection_response(bucket.retry_after_secs(1.0));
        }
    }

    if let Some((held_permit, partition)) = held_credit {
        pending_guard.resolve();
        return run_admitted_request(
            req,
            next,
            held_permit.into_permit(),
            Class::Default,
            Class::Default,
            &tenant,
            &partition,
        )
        .await;
    }

    let resolved = resolve_priority(&req, &state, &tenant);
    let class = resolved.effective;

    if resolved.unknown {
        sched_metrics::record_unknown_priority(state.tenant_labels.intern(tenant.as_str()));
    }
    if class < resolved.requested {
        sched_metrics::record_clamp(
            resolved.requested,
            class,
            state.tenant_labels.intern(tenant.as_str()),
        );
    }

    let partition = state.partition_for(req.headers());
    let request_id = next_registry_id();

    // NOTE: client-disconnect detection during the queue wait is not yet
    // wired (axum does not surface it to a middleware pre-`next.run`), so we
    // pass a fresh token; queued waits are bounded by `queue_timeout`. A
    // real disconnect drops the response future: while queued, the scheduler
    // records that exit as `client_cancelled` and reaps the waiter; once
    // admitted, the permit rides on the response body and is dropped with
    // it, releasing the slot.
    let cancel = CancellationToken::new();

    let (estimated_output_tokens, fair_share_profile) = match partition.scheduler.fair_share() {
        Some(ledger) => (
            output_token_estimate(req.headers(), ledger),
            state.fair_share_profile_for(req.headers(), ledger),
        ),
        None => (1, FairShareProfile::Global),
    };

    match partition
        .scheduler
        .admit_for_tenant_profile(
            class,
            request_id,
            cancel,
            tenant.clone(),
            fair_share_profile,
            estimated_output_tokens,
        )
        .await
    {
        AdmitOutcome::Admitted(permit) => {
            pending_guard.resolve();
            run_admitted_request(
                req,
                next,
                permit,
                class,
                resolved.requested,
                &tenant,
                &partition,
            )
            .await
        }
        AdmitOutcome::Rejected(reason) => {
            Metrics::record_http_admission_rejected();
            pending_guard.resolve();
            let outcome = rejection_outcome(reason);
            sched_metrics::record_admit(class, outcome);
            sched_metrics::record_partition_admit(&partition.name, class, outcome);
            trace!(
                scheduler.class = class.as_str(),
                scheduler.requested_class = resolved.requested.as_str(),
                scheduler.tenant = %tenant,
                scheduler.partition = %partition.name,
                scheduler.admit_outcome = outcome,
                "scheduler admission decision"
            );
            SchedulerError::from(reason)
                .into_response_with_retry_after(partition.scheduler.retry_after_secs(class))
        }
    }
}

/// The permit rides on the response body, held until the body ends or is
/// dropped by the edge's one body wrapper, which marks TTFT on the first data
/// frame; unless the response is the preemption 503 the handler's guard
/// wrote. That request is over: the permit goes now, the slot with it, and the
/// 503's body goes out whole. The wrapper's first-frame check could not tell
/// that 503 from a 200 whose first byte a preemption beat (the scheduler won
/// the preempt CAS either way), and a layer between here and the edge that
/// replaces the body while keeping the extensions (the Messages error
/// envelope) would have it end the rebuilt body before its first byte.
///
/// The header alone does not say so: a worker's response carries its own
/// headers through the relays, this one included (a gateway one tier down
/// writes it). The scheduler's own mark does: nothing has asked the permit
/// about a first frame yet, so a handle that is no longer preemptible was
/// preempted by this scheduler, which no worker can fake. A relayed header
/// on an admitted stream leaves the permit on the body, the slot held and
/// TTFT marked, as before.
fn carry_permit(
    response: &mut Response,
    permit: SchedulerPermit,
    mut active_guard: AdmissionActiveGuard,
) {
    let status_code = response.status();
    if response.headers().contains_key(HEADER_X_SMG_PREEMPTED) && !permit.handle().is_preemptible()
    {
        // The response is whole with its head: it counts as its status.
        active_guard.record_outcome(status_code.as_u16());
        drop(permit);
        return;
    }
    BodyDuties::hand(response, |duties| {
        duties.permit = Some(Box::new(SchedulerBodyPermit::new(
            permit,
            active_guard,
            status_code,
        )));
    });
}

#[cfg(test)]
mod permit_tests {
    //! The scheduler's permit on the edge's one body wrapper: TTFT on the
    //! first data frame, the slot released with the body.
    use std::sync::Arc;

    use axum::{body::Body, http::StatusCode, response::Response};
    use http_body::Body as HttpBody;
    use http_body_util::BodyExt;
    use smg_auth::RequestId;

    use super::*;
    use crate::middleware::{
        edge_body::at_the_edge,
        scheduler::{PriorityScheduler, SchedulerPermit, SchedulerSettings},
    };

    fn scheduler() -> Arc<PriorityScheduler> {
        let settings =
            SchedulerSettings::from_cli_and_yaml(true, Class::Default, 32, None).unwrap();
        // Default reservations sum to 160 (Interactive 128 + System 32),
        // so capacity must be >= 160 for new() to accept them.
        PriorityScheduler::new(&settings, 256).unwrap()
    }

    fn permit(sched: &Arc<PriorityScheduler>, id: &str) -> SchedulerPermit {
        sched
            .acquire_inflight(Class::Default, RequestId(id.to_string()))
            .expect("slot available")
    }

    /// `body` with `permit` on it, as the admission layer leaves it, at the
    /// edge.
    fn guarded(body: Body, permit: SchedulerPermit) -> Body {
        let mut response = Response::new(body);
        BodyDuties::hand(&mut response, |duties| {
            duties.permit = Some(Box::new(permit));
        });
        at_the_edge(response).into_body()
    }

    #[tokio::test]
    async fn the_first_data_frame_marks_ttft() {
        let sched = scheduler();
        let p = permit(&sched, "req-ttft");
        let handle = Arc::clone(p.handle());
        let mut body = guarded(Body::from("hello world"), p);

        assert!(handle.is_preemptible(), "pre-TTFT before first frame");
        let frame = body.frame().await.expect("a frame").expect("ok frame");
        assert!(frame.is_data());
        assert!(
            !handle.is_preemptible(),
            "first data frame must mark TTFT (no longer preemptible)"
        );
        // A request that has emitted its first byte must reject preemption.
        assert!(!handle.try_mark_preempted());
    }

    #[tokio::test]
    async fn a_request_preempted_before_its_first_byte_ends_the_stream_without_it() {
        let sched = scheduler();
        let p = permit(&sched, "req-preempt");
        let handle = Arc::clone(p.handle());
        // Scheduler wins the preempt CAS before any byte is polled.
        assert!(handle.try_mark_preempted());

        let mut body = guarded(Body::from("should not surface"), p);
        // The first poll must end the stream rather than yield the data.
        assert!(
            body.frame().await.is_none(),
            "preempted body must end the stream on first frame"
        );
        // is_end_stream must agree with the forced None, and a re-poll must
        // stay ended.
        assert!(
            HttpBody::is_end_stream(&body),
            "force-ended stream must report is_end_stream() == true"
        );
        assert!(
            body.frame().await.is_none(),
            "re-polling a force-ended stream must stay ended"
        );
        assert_eq!(
            sched.inflight_for_test(Class::Default),
            0,
            "the slot is released with the ended body"
        );
    }

    #[tokio::test]
    async fn an_empty_body_never_marks_ttft() {
        let sched = scheduler();
        let p = permit(&sched, "req-empty");
        let handle = Arc::clone(p.handle());
        // Empty body: no data frames at all.
        let body = guarded(Body::empty(), p);
        let _ = body.collect().await;
        assert!(
            handle.is_preemptible(),
            "no data frame emitted -> TTFT never marked"
        );
    }

    #[tokio::test]
    async fn dropping_the_body_releases_the_slot() {
        let sched = scheduler();
        assert_eq!(sched.inflight_for_test(Class::Default), 0);
        let p = permit(&sched, "req-drop");
        assert_eq!(sched.inflight_for_test(Class::Default), 1);
        let body = guarded(Body::from("x"), p);
        drop(body);
        assert_eq!(
            sched.inflight_for_test(Class::Default),
            0,
            "dropping the guarded body releases the slot"
        );
    }

    /// The preemption 503 the handler's guard wrote carries no permit: the
    /// request is over, the slot goes with the response head, and the body
    /// goes out whole, since no first-frame check rides on it.
    #[tokio::test]
    async fn a_preemption_503_carries_no_permit_and_goes_out_whole() {
        let sched = scheduler();
        let p = permit(&sched, "req-503");
        let handle = Arc::clone(p.handle());
        assert!(handle.try_mark_preempted(), "the scheduler won the CAS");
        let mut response = Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(HEADER_X_SMG_PREEMPTED, "true")
            .body(Body::from("{\"error\":\"overloaded\"}"))
            .expect("response");
        carry_permit(&mut response, p, AdmissionActiveGuard::new());
        assert_eq!(
            sched.inflight_for_test(Class::Default),
            0,
            "the slot goes with the 503, not with its body"
        );
        let body = at_the_edge(response)
            .into_body()
            .collect()
            .await
            .expect("whole")
            .to_bytes();
        assert_eq!(&body[..], b"{\"error\":\"overloaded\"}");
    }

    /// The preemption header a worker's response carries through the relays
    /// is not this scheduler's word: an admitted stream keeps its permit,
    /// the slot stays held and the first frame marks TTFT.
    #[tokio::test]
    async fn a_relayed_preemption_header_on_an_admitted_stream_keeps_the_permit() {
        let sched = scheduler();
        let p = permit(&sched, "req-relayed");
        let handle = Arc::clone(p.handle());
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header(HEADER_X_SMG_PREEMPTED, "true")
            .body(Body::from("data: x\n\n"))
            .expect("response");
        carry_permit(&mut response, p, AdmissionActiveGuard::new());
        assert_eq!(
            sched.inflight_for_test(Class::Default),
            1,
            "the slot stays held: the header is the worker's, not the scheduler's"
        );
        let mut body = at_the_edge(response).into_body();
        let frame = body.frame().await.expect("a frame").expect("frame ok");
        assert!(frame.is_data());
        assert!(!handle.is_preemptible(), "the first frame marked TTFT");
        drop(body);
        assert_eq!(sched.inflight_for_test(Class::Default), 0);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::middleware::scheduler::FairShareConfig;

    fn ledger(trust_header: bool) -> GlobalFairShare {
        GlobalFairShare::from_config(&FairShareConfig {
            default_weight: 1.0,
            default_output_tokens: 256,
            trust_output_token_estimate_header: trust_header,
            trust_request_model_header: false,
            tenant_weights: HashMap::new(),
            model_profiles: HashMap::new(),
        })
    }

    #[test]
    fn untrusted_client_estimate_cannot_reduce_reservation() {
        let mut headers = HeaderMap::new();
        headers.insert(OUTPUT_TOKEN_ESTIMATE_HEADER, "1".parse().unwrap());
        assert_eq!(output_token_estimate(&headers, &ledger(false)), 256);
    }

    #[test]
    fn trusted_proxy_estimate_is_honored() {
        let mut headers = HeaderMap::new();
        headers.insert(OUTPUT_TOKEN_ESTIMATE_HEADER, "128".parse().unwrap());
        assert_eq!(output_token_estimate(&headers, &ledger(true)), 128);
    }
}
