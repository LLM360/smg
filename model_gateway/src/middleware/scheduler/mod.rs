//! Priority-aware admission scheduler.

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
pub use state::{AdmissionMode, SchedulerState, ADMISSION_PARTITION_HEADER};

/// In-process response marker set only by SMG's local adaptive-admission stage.
/// It is never serialized to clients or accepted from a backend response.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalAdaptiveRejection;
