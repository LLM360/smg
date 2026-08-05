//! Priority-aware admission scheduler.

pub mod admission;
pub mod body;
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
pub use class::{Class, PRIORITY_HEADER};
pub use config::{
    AdmissionPartitionConfig, ClassConfig, ClassRuntimeConfig, FairShareConfig,
    PrioritySchedulerYaml, SchedulerSettings, SettingsValidationError, TenantPolicyConfig,
};
pub use engine::{
    AdmitOutcome, PriorityScheduler, RejectionReason, SchedulerInitError, SchedulerPermit,
};
pub use error::{SchedulerError, HEADER_X_SMG_PREEMPTED};
pub use extract::PreemptionGuard;
pub use fair_share::{GlobalFairShare, SettlementKind, OUTPUT_TOKEN_ESTIMATE_HEADER};
pub use policy::{StaticTenantPolicyResolver, TenantPolicy, TenantPolicyResolver};
pub use state::{AdmissionMode, SchedulerState, ADMISSION_PARTITION_HEADER};
