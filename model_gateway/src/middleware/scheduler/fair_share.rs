//! Process-wide output-token accounting and weighted service with local queues.
//!
//! Every priority-scheduler partition receives the same [`Arc<GlobalFairShare>`].
//! Lifetime charged-token accounting and canonical tenant virtual finish span
//! that process. Each partition stores only local queued/reservation membership.
//! Queued work is registered before a partition asks for its next local waiter,
//! and the ledger chooses the globally least-served tenant among candidates
//! eligible for that partition. This keeps every model pool work-conserving:
//! unrelated or non-fungible capacity is never idled to repay another tenant's
//! debt.
//!
//! The ledger is process-local. It spans all model/admission partitions and
//! all workload types that resolve to the same tenant key in one SMG process;
//! it does not coordinate separate gateways or overlapping blue/green
//! processes. Because model pools are non-fungible, it also cannot guarantee
//! exact aggregate percentages when users target disjoint pools. It enforces
//! the configured ratios whenever weighted tenants contend for substitutable
//! eligible capacity. Service and contention debt can follow a tenant across
//! pools, but cannot force a disjoint pool to idle.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;

use super::{Class, FairShareConfig, SchedulerSettings};
use crate::tenant::TenantKey;

/// Trusted estimate injected by the authenticated Comet proxy.
pub const OUTPUT_TOKEN_ESTIMATE_HEADER: &str = "x-smg-output-token-estimate";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementKind {
    Observed,
    MissingUsage,
    Interrupted,
}

#[derive(Debug, Default)]
struct TenantAccounting {
    charged_output_tokens: u64,
    reserved_output_tokens: u64,
}

#[derive(Debug, Default)]
struct TenantService {
    active_reservations: u64,
    queued: usize,
    virtual_finish: f64,
}

#[derive(Debug, Default)]
struct ScopeTenant {
    active_reservations: u64,
    queued: [usize; 4],
}

#[derive(Debug, Default)]
struct ScopeLedger {
    tenants: HashMap<TenantKey, ScopeTenant>,
}

#[derive(Debug, Default)]
struct LedgerState {
    accounting: HashMap<TenantKey, TenantAccounting>,
    service: HashMap<TenantKey, TenantService>,
    scopes: HashMap<u64, ScopeLedger>,
    system_virtual_time: f64,
}

/// One local candidate offered by a partition queue.
pub(crate) struct FairShareCandidate<'a> {
    pub index: usize,
    pub tenant: &'a TenantKey,
    pub estimated_output_tokens: u32,
}

/// The candidate selected by the shared ledger, plus its provisional charge.
pub(crate) struct FairShareSelection {
    pub index: usize,
    pub reservation: FairShareReservation,
}

/// Shared weighted-service ledger for every scheduler partition in one router.
pub struct GlobalFairShare {
    default_weight: f64,
    default_output_tokens: u32,
    trust_output_token_estimate_header: bool,
    tenant_weights: HashMap<TenantKey, f64>,
    metric_tenants: HashSet<TenantKey>,
    state: Mutex<LedgerState>,
    next_scope: AtomicU64,
}

impl std::fmt::Debug for GlobalFairShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlobalFairShare")
            .field("default_weight", &self.default_weight)
            .field("default_output_tokens", &self.default_output_tokens)
            .field(
                "trust_output_token_estimate_header",
                &self.trust_output_token_estimate_header,
            )
            .field("tenant_weights", &self.tenant_weights)
            .finish_non_exhaustive()
    }
}

impl GlobalFairShare {
    #[must_use]
    pub fn from_settings(settings: &SchedulerSettings) -> Option<Self> {
        settings.fair_share_config().map(|config| {
            let mut ledger = Self::from_config(config);
            let mut metric_tenants: Vec<_> = ledger.tenant_weights.keys().cloned().collect();
            metric_tenants.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            metric_tenants.truncate(settings.tenant_metric_top_n as usize);
            ledger.metric_tenants = metric_tenants.into_iter().collect();
            ledger
        })
    }

    #[must_use]
    pub fn from_config(config: &FairShareConfig) -> Self {
        let tenant_weights: HashMap<_, _> = config
            .tenant_weights
            .iter()
            .map(|(tenant, weight)| (TenantKey::new(tenant), *weight))
            .collect();
        let metric_tenants = tenant_weights.keys().cloned().collect();
        Self {
            default_weight: config.default_weight,
            default_output_tokens: config.default_output_tokens,
            trust_output_token_estimate_header: config.trust_output_token_estimate_header,
            tenant_weights,
            metric_tenants,
            state: Mutex::new(LedgerState::default()),
            next_scope: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn default_output_tokens(&self) -> u32 {
        self.default_output_tokens
    }

    #[must_use]
    pub fn trusts_output_token_estimate_header(&self) -> bool {
        self.trust_output_token_estimate_header
    }

    pub(crate) fn new_scope(&self) -> u64 {
        self.next_scope.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn record_queue_wait(&self, tenant: &TenantKey, class: Class, wait: Duration) {
        super::metrics::record_fair_share_queue_wait(self.metric_tenant(tenant), class, wait);
    }

    pub(crate) fn register_waiter(&self, scope_id: u64, tenant: &TenantKey, class: Class) {
        let unknown = !self.tenant_weights.contains_key(tenant);
        let mut state = self.state.lock();
        let system_virtual_time = state.system_virtual_time;
        let service = state.service.entry(tenant.clone()).or_default();
        if !Self::is_active(service) {
            // Start-time fair queueing: idle tenants join the current epoch.
            // They neither retain stale low credit nor inherit solo service as
            // debt against users that were not backlogged at the time.
            service.virtual_finish = service.virtual_finish.max(system_virtual_time);
        }
        service.queued = service.queued.saturating_add(1);
        state
            .scopes
            .entry(scope_id)
            .or_default()
            .tenants
            .entry(tenant.clone())
            .or_default()
            .queued[class as usize] += 1;
        Self::advance_system_virtual_time(&mut state);
        drop(state);
        if unknown {
            super::metrics::record_fair_share_unknown_tenant(self.metric_tenant(tenant));
        }
    }

    pub(crate) fn remove_waiter(&self, scope_id: u64, tenant: &TenantKey, class: Class) {
        let mut state = self.state.lock();
        {
            let Some(scope) = state.scopes.get_mut(&scope_id) else {
                return;
            };
            let Some(membership) = scope.tenants.get_mut(tenant) else {
                return;
            };
            let queued = &mut membership.queued[class as usize];
            debug_assert!(*queued > 0, "fair-share waiter removed below zero");
            if *queued == 0 {
                return;
            }
            *queued -= 1;
        }
        if let Some(service) = state.service.get_mut(tenant) {
            service.queued = service.queued.saturating_sub(1);
        }
        Self::advance_system_virtual_time(&mut state);
        drop(state);
    }

    /// Reserve the least-served candidate eligible for this local partition.
    ///
    /// Selection never consults tenants that are queued only in another
    /// partition. That is the work-conserving boundary for non-fungible model
    /// pools: a local slot is never idled for work that cannot use it.
    pub(crate) fn reserve_local_candidate(
        self: &Arc<Self>,
        scope_id: u64,
        class: Class,
        candidates: &[FairShareCandidate<'_>],
    ) -> Option<FairShareSelection> {
        let mut state = self.state.lock();
        let class_index = class as usize;
        let selected = {
            let scope = state.scopes.get(&scope_id)?;
            candidates
                .iter()
                .filter_map(|candidate| {
                    let membership = scope.tenants.get(candidate.tenant)?;
                    if membership.queued[class_index] == 0 {
                        return None;
                    }
                    let service = state.service.get(candidate.tenant)?;
                    Some((candidate, service.virtual_finish))
                })
                .min_by(|(left, left_service), (right, right_service)| {
                    left_service
                        .total_cmp(right_service)
                        .then_with(|| left.tenant.as_str().cmp(right.tenant.as_str()))
                        .then_with(|| left.index.cmp(&right.index))
                })?
                .0
        };

        let tenant = selected.tenant.clone();
        let estimated_output_tokens = selected.estimated_output_tokens.max(1);
        let membership = state.scopes.get_mut(&scope_id)?.tenants.get_mut(&tenant)?;
        debug_assert!(membership.queued[class_index] > 0);
        membership.queued[class_index] = membership.queued[class_index].saturating_sub(1);
        membership.active_reservations = membership.active_reservations.saturating_add(1);
        let service = state.service.get_mut(&tenant)?;
        service.queued = service.queued.saturating_sub(1);
        service.active_reservations = service.active_reservations.saturating_add(1);
        service.virtual_finish += f64::from(estimated_output_tokens) / self.weight(&tenant);
        let virtual_service = service.virtual_finish;
        Self::advance_system_virtual_time(&mut state);
        let accounting = state.accounting.entry(tenant.clone()).or_default();
        accounting.reserved_output_tokens = accounting
            .reserved_output_tokens
            .saturating_add(u64::from(estimated_output_tokens));
        let reserved_output_tokens = accounting.reserved_output_tokens;
        drop(state);

        super::metrics::set_fair_share_virtual_finish(self.metric_tenant(&tenant), virtual_service);
        super::metrics::set_fair_share_reserved_output_tokens(
            self.metric_tenant(&tenant),
            reserved_output_tokens,
        );

        Some(FairShareSelection {
            index: selected.index,
            reservation: FairShareReservation {
                ledger: Arc::clone(self),
                tenant,
                scope_id,
                estimated_output_tokens,
                settled: false,
            },
        })
    }

    fn weight(&self, tenant: &TenantKey) -> f64 {
        self.tenant_weights
            .get(tenant)
            .copied()
            .unwrap_or(self.default_weight)
    }

    fn is_active(service: &TenantService) -> bool {
        service.active_reservations > 0 || service.queued > 0
    }

    fn advance_system_virtual_time(state: &mut LedgerState) {
        if let Some(active_minimum) = state
            .service
            .values()
            .filter(|service| Self::is_active(service))
            .map(|service| service.virtual_finish)
            .min_by(f64::total_cmp)
        {
            state.system_virtual_time = state.system_virtual_time.max(active_minimum);
        }
    }

    fn metric_tenant<'a>(&'a self, tenant: &'a TenantKey) -> &'a str {
        if self.metric_tenants.contains(tenant) {
            tenant.as_str()
        } else {
            "other"
        }
    }

    fn cancel_reservation(&self, scope_id: u64, tenant: &TenantKey, estimated_output_tokens: u32) {
        let mut state = self.state.lock();
        {
            let Some(membership) = state
                .scopes
                .get_mut(&scope_id)
                .and_then(|scope| scope.tenants.get_mut(tenant))
            else {
                return;
            };
            if membership.active_reservations == 0 {
                return;
            }
            membership.active_reservations -= 1;
        }
        let system_virtual_time = state.system_virtual_time;
        let Some(service) = state.service.get_mut(tenant) else {
            return;
        };
        service.active_reservations = service.active_reservations.saturating_sub(1);
        service.virtual_finish = (service.virtual_finish
            - f64::from(estimated_output_tokens) / self.weight(tenant))
        .max(system_virtual_time);
        let virtual_service = service.virtual_finish;
        Self::advance_system_virtual_time(&mut state);
        let accounting = state.accounting.entry(tenant.clone()).or_default();
        accounting.reserved_output_tokens = accounting
            .reserved_output_tokens
            .saturating_sub(u64::from(estimated_output_tokens));
        let reserved_output_tokens = accounting.reserved_output_tokens;
        drop(state);
        super::metrics::set_fair_share_virtual_finish(self.metric_tenant(tenant), virtual_service);
        super::metrics::set_fair_share_reserved_output_tokens(
            self.metric_tenant(tenant),
            reserved_output_tokens,
        );
    }

    fn settle_reservation(
        &self,
        scope_id: u64,
        tenant: &TenantKey,
        estimated_output_tokens: u32,
        observed_output_tokens: Option<u32>,
        kind: SettlementKind,
    ) {
        let charged = observed_output_tokens.unwrap_or(estimated_output_tokens);
        let mut state = self.state.lock();
        {
            let membership = state
                .scopes
                .entry(scope_id)
                .or_default()
                .tenants
                .entry(tenant.clone())
                .or_default();
            membership.active_reservations = membership.active_reservations.saturating_sub(1);
        }
        let system_virtual_time = state.system_virtual_time;
        let service = state.service.entry(tenant.clone()).or_default();
        service.active_reservations = service.active_reservations.saturating_sub(1);
        let correction =
            (f64::from(charged) - f64::from(estimated_output_tokens)) / self.weight(tenant);
        service.virtual_finish = (service.virtual_finish + correction).max(system_virtual_time);
        let virtual_service = service.virtual_finish;
        Self::advance_system_virtual_time(&mut state);
        let accounting = state.accounting.entry(tenant.clone()).or_default();
        accounting.reserved_output_tokens = accounting
            .reserved_output_tokens
            .saturating_sub(u64::from(estimated_output_tokens));
        accounting.charged_output_tokens = accounting
            .charged_output_tokens
            .saturating_add(u64::from(charged));
        let reserved_output_tokens = accounting.reserved_output_tokens;
        drop(state);

        let metric_tenant = self.metric_tenant(tenant);
        super::metrics::record_fair_share_charged_output_tokens(metric_tenant, charged);
        super::metrics::set_fair_share_virtual_finish(metric_tenant, virtual_service);
        super::metrics::set_fair_share_reserved_output_tokens(
            metric_tenant,
            reserved_output_tokens,
        );
        if kind != SettlementKind::Observed {
            super::metrics::record_fair_share_fallback(kind.as_str());
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot(
        &self,
        scope_id: u64,
        tenant: &TenantKey,
    ) -> (u64, u64, u64, [usize; 4]) {
        let state = self.state.lock();
        let accounting = state
            .accounting
            .get(tenant)
            .expect("tenant accounting exists");
        let membership = state
            .scopes
            .get(&scope_id)
            .and_then(|scope| scope.tenants.get(tenant))
            .expect("tenant scope membership exists");
        (
            accounting.charged_output_tokens,
            accounting.reserved_output_tokens,
            membership.active_reservations,
            membership.queued,
        )
    }

    #[cfg(test)]
    fn virtual_snapshot(&self, scope_id: u64, tenant: &TenantKey) -> (f64, f64) {
        let state = self.state.lock();
        state
            .scopes
            .get(&scope_id)
            .and_then(|scope| scope.tenants.get(tenant))
            .expect("tenant scope membership exists");
        (
            state
                .service
                .get(tenant)
                .expect("tenant service exists")
                .virtual_finish,
            state.system_virtual_time,
        )
    }
}

impl SettlementKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::MissingUsage => "missing_usage",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Provisional output-token charge attached to an admitted request.
///
/// Explicit settlement replaces the estimate with observed terminal usage.
/// Dropping without terminal usage keeps the estimate charged, preventing a
/// disconnect from becoming a way to escape fair-share accounting.
pub struct FairShareReservation {
    ledger: Arc<GlobalFairShare>,
    tenant: TenantKey,
    scope_id: u64,
    estimated_output_tokens: u32,
    settled: bool,
}

impl std::fmt::Debug for FairShareReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FairShareReservation")
            .field("tenant", &self.tenant)
            .field("scope_id", &self.scope_id)
            .field("estimated_output_tokens", &self.estimated_output_tokens)
            .field("settled", &self.settled)
            .finish()
    }
}

impl FairShareReservation {
    pub fn settle(mut self, observed_output_tokens: Option<u32>, kind: SettlementKind) {
        self.ledger.settle_reservation(
            self.scope_id,
            &self.tenant,
            self.estimated_output_tokens,
            observed_output_tokens,
            kind,
        );
        self.settled = true;
    }

    pub fn cancel(mut self) {
        self.ledger
            .cancel_reservation(self.scope_id, &self.tenant, self.estimated_output_tokens);
        self.settled = true;
    }
}

impl Drop for FairShareReservation {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.ledger.settle_reservation(
            self.scope_id,
            &self.tenant,
            self.estimated_output_tokens,
            None,
            SettlementKind::Interrupted,
        );
        self.settled = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(weights: &[(&str, f64)]) -> FairShareConfig {
        FairShareConfig {
            default_weight: 1.0,
            default_output_tokens: 10,
            trust_output_token_estimate_header: false,
            tenant_weights: weights
                .iter()
                .map(|(tenant, weight)| ((*tenant).to_string(), *weight))
                .collect(),
        }
    }

    fn reserve_one(
        ledger: &Arc<GlobalFairShare>,
        scope_id: u64,
        class: Class,
        tenant: &TenantKey,
        estimated_output_tokens: u32,
    ) -> Option<FairShareReservation> {
        ledger.register_waiter(scope_id, tenant, class);
        ledger
            .reserve_local_candidate(
                scope_id,
                class,
                &[FairShareCandidate {
                    index: 0,
                    tenant,
                    estimated_output_tokens,
                }],
            )
            .map(|selection| selection.reservation)
    }

    #[test]
    fn observed_tokens_replace_the_provisional_estimate_once() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[("header:a", 1.0)])));
        let scope_id = ledger.new_scope();
        let tenant = TenantKey::new("header:a");
        let reservation = reserve_one(&ledger, scope_id, Class::Default, &tenant, 100).unwrap();
        assert_eq!(ledger.snapshot(scope_id, &tenant), (0, 100, 1, [0; 4]));

        reservation.settle(Some(37), SettlementKind::Observed);
        assert_eq!(ledger.snapshot(scope_id, &tenant), (37, 0, 0, [0; 4]));
    }

    #[test]
    fn cancelled_delivery_reverts_the_provisional_charge() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[("header:a", 1.0)])));
        let scope_id = ledger.new_scope();
        let tenant = TenantKey::new("header:a");
        let reservation = reserve_one(&ledger, scope_id, Class::Default, &tenant, 100).unwrap();
        reservation.cancel();
        assert_eq!(ledger.snapshot(scope_id, &tenant), (0, 0, 0, [0; 4]));
    }

    #[test]
    fn missing_terminal_usage_keeps_the_estimate_charged() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[("header:a", 1.0)])));
        let scope_id = ledger.new_scope();
        let tenant = TenantKey::new("header:a");
        let reservation = reserve_one(&ledger, scope_id, Class::Default, &tenant, 100).unwrap();
        drop(reservation);
        assert_eq!(ledger.snapshot(scope_id, &tenant), (100, 0, 0, [0; 4]));
    }

    #[test]
    fn weighted_share_converges_when_users_contend_for_one_pool() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 2.0),
            ("header:b", 1.0),
        ])));
        let scope_id = ledger.new_scope();
        let a = TenantKey::new("header:a");
        let b = TenantKey::new("header:b");
        let mut admitted_a = 0;
        let mut admitted_b = 0;

        // Keep both tenants continuously backlogged for every selection.
        for _ in 0..30 {
            ledger.register_waiter(scope_id, &a, Class::Default);
            ledger.register_waiter(scope_id, &b, Class::Default);
        }
        for _ in 0..30 {
            let candidates = [
                FairShareCandidate {
                    index: 0,
                    tenant: &a,
                    estimated_output_tokens: 10,
                },
                FairShareCandidate {
                    index: 1,
                    tenant: &b,
                    estimated_output_tokens: 10,
                },
            ];
            let selection = ledger
                .reserve_local_candidate(scope_id, Class::Default, &candidates)
                .expect("a local contender must be selected");
            if selection.index == 0 {
                admitted_a += 1;
                selection
                    .reservation
                    .settle(Some(10), SettlementKind::Observed);
            } else {
                admitted_b += 1;
                selection
                    .reservation
                    .settle(Some(10), SettlementKind::Observed);
            }
        }

        assert_eq!((admitted_a, admitted_b), (20, 10));
    }

    #[test]
    fn lone_backlogged_user_borrows_all_available_capacity() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 1.0),
            ("header:b", 1.0),
        ])));
        let scope_id = ledger.new_scope();
        let a = TenantKey::new("header:a");

        for _ in 0..4 {
            reserve_one(&ledger, scope_id, Class::Default, &a, 10)
                .expect("a lone eligible user must never be idled")
                .settle(Some(10), SettlementKind::Observed);
        }
        assert_eq!(ledger.snapshot(scope_id, &a).0, 40);
    }

    #[test]
    fn underserved_eligible_user_wins_under_simultaneous_contention() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 1.0),
            ("header:b", 1.0),
        ])));
        let scope_id = ledger.new_scope();
        let a = TenantKey::new("header:a");
        let b = TenantKey::new("header:b");

        // b is already backlogged while a receives service, so this is real
        // debt on a shared constrained resource rather than idle-time credit.
        ledger.register_waiter(scope_id, &a, Class::Default);
        ledger.register_waiter(scope_id, &b, Class::Default);
        let candidates = [
            FairShareCandidate {
                index: 0,
                tenant: &a,
                estimated_output_tokens: 10,
            },
            FairShareCandidate {
                index: 1,
                tenant: &b,
                estimated_output_tokens: 10,
            },
        ];
        let first = ledger
            .reserve_local_candidate(scope_id, Class::Default, &candidates)
            .unwrap();
        assert_eq!(first.index, 0, "stable tie-break selects a first");
        first.reservation.settle(Some(50), SettlementKind::Observed);
        ledger.register_waiter(scope_id, &a, Class::Default);

        let selected = ledger
            .reserve_local_candidate(scope_id, Class::Default, &candidates)
            .unwrap();
        assert_eq!(selected.index, 1, "the underserved eligible user wins");
        selected.reservation.cancel();
    }

    #[test]
    fn solo_service_does_not_create_debt_against_an_idle_tenant() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 1.0),
            ("header:b", 1.0),
        ])));
        let scope_id = ledger.new_scope();
        let a = TenantKey::new("header:a");
        let b = TenantKey::new("header:b");

        for _ in 0..100 {
            reserve_one(&ledger, scope_id, Class::Default, &a, 10)
                .unwrap()
                .settle(Some(10), SettlementKind::Observed);
        }
        ledger.register_waiter(scope_id, &a, Class::Default);
        ledger.register_waiter(scope_id, &b, Class::Default);

        let (a_finish, system_time) = ledger.virtual_snapshot(scope_id, &a);
        let (b_finish, _) = ledger.virtual_snapshot(scope_id, &b);
        assert_eq!(a_finish, system_time);
        assert_eq!(b_finish, system_time);

        let candidates = [
            FairShareCandidate {
                index: 0,
                tenant: &a,
                estimated_output_tokens: 10,
            },
            FairShareCandidate {
                index: 1,
                tenant: &b,
                estimated_output_tokens: 10,
            },
        ];
        let selected = ledger
            .reserve_local_candidate(scope_id, Class::Default, &candidates)
            .unwrap();
        assert_eq!(
            selected.index, 0,
            "new b must not monopolize service to match a's lifetime total"
        );
        selected.reservation.cancel();
    }

    #[test]
    fn idle_returning_tenant_is_prompt_without_unbounded_catch_up() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 1.0),
            ("header:b", 1.0),
        ])));
        let scope_id = ledger.new_scope();
        let a = TenantKey::new("header:a");
        let b = TenantKey::new("header:b");

        for _ in 0..20 {
            reserve_one(&ledger, scope_id, Class::Default, &a, 10)
                .unwrap()
                .settle(Some(10), SettlementKind::Observed);
        }
        for _ in 0..4 {
            ledger.register_waiter(scope_id, &a, Class::Default);
            ledger.register_waiter(scope_id, &b, Class::Default);
        }
        let candidates = [
            FairShareCandidate {
                index: 0,
                tenant: &a,
                estimated_output_tokens: 10,
            },
            FairShareCandidate {
                index: 1,
                tenant: &b,
                estimated_output_tokens: 10,
            },
        ];
        let mut order = Vec::new();
        for _ in 0..4 {
            let selected = ledger
                .reserve_local_candidate(scope_id, Class::Default, &candidates)
                .unwrap();
            order.push(selected.index);
            selected
                .reservation
                .settle(Some(10), SettlementKind::Observed);
        }
        assert_eq!(order, vec![0, 1, 0, 1]);
    }

    #[test]
    fn unrelated_model_backlog_never_blocks_local_capacity() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 1.0),
            ("header:b", 1.0),
        ])));
        let local_scope = ledger.new_scope();
        let other_scope = ledger.new_scope();
        let a = TenantKey::new("header:a");
        let b = TenantKey::new("header:b");

        // b is queued only for another model. It cannot consume this scope's
        // slot or cause local idling.
        ledger.register_waiter(other_scope, &b, Class::Default);
        ledger.register_waiter(local_scope, &a, Class::Default);
        let selected = ledger
            .reserve_local_candidate(
                local_scope,
                Class::Default,
                &[FairShareCandidate {
                    index: 0,
                    tenant: &a,
                    estimated_output_tokens: 10,
                }],
            )
            .expect("an eligible local request must keep the pool work-conserving");
        selected.reservation.cancel();
    }

    #[test]
    fn contended_service_in_one_pool_affects_ordering_in_another_pool() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 1.0),
            ("header:b", 1.0),
        ])));
        let first_pool = ledger.new_scope();
        let second_pool = ledger.new_scope();
        let a = TenantKey::new("header:a");
        let b = TenantKey::new("header:b");

        // b is backlogged while a receives service in the first pool, so a's
        // canonical process-global virtual finish moves ahead of b's.
        ledger.register_waiter(first_pool, &b, Class::Default);
        reserve_one(&ledger, first_pool, Class::Default, &a, 50)
            .unwrap()
            .settle(Some(50), SettlementKind::Observed);

        ledger.register_waiter(second_pool, &a, Class::Default);
        ledger.register_waiter(second_pool, &b, Class::Default);
        let candidates = [
            FairShareCandidate {
                index: 0,
                tenant: &a,
                estimated_output_tokens: 10,
            },
            FairShareCandidate {
                index: 1,
                tenant: &b,
                estimated_output_tokens: 10,
            },
        ];
        let selected = ledger
            .reserve_local_candidate(second_pool, Class::Default, &candidates)
            .unwrap();
        assert_eq!(
            selected.index, 1,
            "contended service debt must follow a tenant across model pools"
        );
        selected.reservation.cancel();
        assert_eq!(ledger.snapshot(first_pool, &a).0, 50);
    }

    #[test]
    fn batch_and_interactive_share_one_partition_ledger() {
        let ledger = Arc::new(GlobalFairShare::from_config(&config(&[
            ("header:a", 1.0),
            ("header:b", 1.0),
        ])));
        let scope_id = ledger.new_scope();
        let a = TenantKey::new("header:a");
        let b = TenantKey::new("header:b");

        // b is backlogged in the partition while a receives bulk service.
        // Class priority remains the outer policy, but the tenant's virtual
        // finish is shared when a later interactive contention is resolved.
        ledger.register_waiter(scope_id, &b, Class::Default);
        let bulk = reserve_one(&ledger, scope_id, Class::Bulk, &a, 50).unwrap();
        bulk.settle(Some(50), SettlementKind::Observed);

        ledger.register_waiter(scope_id, &a, Class::Interactive);
        ledger.register_waiter(scope_id, &b, Class::Interactive);
        let candidates = [
            FairShareCandidate {
                index: 0,
                tenant: &a,
                estimated_output_tokens: 10,
            },
            FairShareCandidate {
                index: 1,
                tenant: &b,
                estimated_output_tokens: 10,
            },
        ];
        let selected = ledger
            .reserve_local_candidate(scope_id, Class::Interactive, &candidates)
            .unwrap();
        assert_eq!(
            selected.index, 1,
            "bulk output remains visible to later interactive contention"
        );
        selected.reservation.cancel();
        assert_eq!(ledger.snapshot(scope_id, &a).0, 50);
    }
}
