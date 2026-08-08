//! Priority-aware admission scheduler.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub mod admission;
pub mod body;
pub mod capacity_credit;
pub mod capacity_credit_api;
pub mod class;
pub mod config;
pub mod engine;
pub mod error;
pub mod extract;
pub mod fair_share;
pub mod inflight;
pub mod metrics;
mod output_tokens;
pub mod policy;
pub mod queue;
pub mod slots;
pub mod state;

pub use admission::priority_admission_middleware;
pub use body::SchedulerGuardBody;
pub(crate) use capacity_credit_api::{cancel_capacity_credit, issue_capacity_credit};
pub use capacity_credit_api::{
    CAPACITY_CREDIT_GENERATION_HEADER, CAPACITY_CREDIT_HEADER, CAPACITY_CREDIT_POLICY_EPOCH_HEADER,
    CAPACITY_CREDIT_REQUEST_ID_HEADER,
};
pub use class::{Class, PRIORITY_HEADER};
pub use config::{
    AdmissionPartitionConfig, ClassConfig, ClassRuntimeConfig, FairShareConfig,
    ModelFairShareConfig, PrioritySchedulerYaml, SchedulerSettings, SettingsValidationError,
    TenantPolicyConfig,
};
pub use engine::{
    AdmitOutcome, PriorityScheduler, RejectionReason, SchedulerInitError, SchedulerPermit,
};
pub use error::{SchedulerError, HEADER_X_SMG_PREEMPTED};
pub use extract::PreemptionGuard;
pub use fair_share::{
    GlobalFairShare, SettlementKind, OUTPUT_TOKEN_ESTIMATE_HEADER, REQUEST_MODEL_HEADER,
};
pub use policy::{StaticTenantPolicyResolver, TenantPolicy, TenantPolicyResolver};
pub use state::{
    AdmissionMode, SchedulerState, ADMISSION_PARTITION_HEADER, ADMISSION_PARTITION_LABEL,
};

/// In-process response marker set only by SMG's local adaptive-admission stage.
/// It is never serialized to clients or accepted from a backend response.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalAdaptiveRejection;

/// In-process proof that the request redeemed one allocator-issued capacity
/// credit. The binding is immutable and the route authorization can be claimed
/// at most once across cloned request metadata.
///
/// This is deliberately transport and routing-policy agnostic. It does not
/// select a worker or grant engine capacity; those checks remain with adaptive
/// admission and the active routing policy.
#[derive(Clone, Debug)]
pub(crate) struct RedeemedCapacityCreditAuthorization {
    binding: capacity_credit::CapacityCreditBinding,
    route_claimed: Arc<AtomicBool>,
}

impl RedeemedCapacityCreditAuthorization {
    pub(crate) fn new(binding: capacity_credit::CapacityCreditBinding) -> Self {
        Self {
            binding,
            route_claimed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn matches(&self, partition: &str, model: &str) -> bool {
        self.binding.partition() == partition && self.binding.model() == model
    }

    pub(crate) fn try_claim(&self, partition: &str, model: &str) -> bool {
        self.matches(partition, model)
            && self
                .route_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::TenantKey;

    fn authorization() -> RedeemedCapacityCreditAuthorization {
        let binding = capacity_credit::CapacityCreditBinding::new(
            "generation-1",
            7,
            "k3",
            "kimi-k3",
            TenantKey::new("tenant-a"),
            "request-1",
            1_000,
        )
        .unwrap();
        RedeemedCapacityCreditAuthorization::new(binding)
    }

    #[test]
    fn redeemed_credit_route_claim_is_exact_and_shared_across_clones() {
        let authorization = authorization();
        let clone = authorization.clone();

        assert!(!authorization.matches("other", "kimi-k3"));
        assert!(!authorization.try_claim("other", "kimi-k3"));
        assert!(authorization.matches("k3", "kimi-k3"));
        assert!(authorization.try_claim("k3", "kimi-k3"));
        assert!(!clone.try_claim("k3", "kimi-k3"));
    }
}
