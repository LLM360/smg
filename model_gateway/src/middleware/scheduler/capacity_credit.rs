//! Short-lived, single-use capacity credit registry.
//!
//! The registry is deliberately transport-agnostic and is not wired into the
//! request path. A follow-up can store an acquired [`super::SchedulerPermit`]
//! as the payload, then expose issue and redemption through authenticated
//! internal Comet-to-SMG endpoints.

use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use parking_lot::Mutex;
use rand::Rng;
use thiserror::Error;

use crate::tenant::TenantKey;

const TOKEN_BYTES: usize = 32;

/// Opaque bearer token for one capacity credit.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CapacityCreditToken([u8; TOKEN_BYTES]);

impl CapacityCreditToken {
    fn random() -> Self {
        let mut bytes = [0_u8; TOKEN_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Encode the token for an internal HTTP header.
    #[must_use]
    pub fn expose_secret(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// Parse an internal header value into a fixed-size token.
    pub fn parse(encoded: &str) -> Result<Self, CapacityCreditError> {
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| CapacityCreditError::InvalidToken)?;
        let bytes = decoded
            .try_into()
            .map_err(|_| CapacityCreditError::InvalidToken)?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for CapacityCreditToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapacityCreditToken([REDACTED])")
    }
}

/// Immutable identity and accounting scope of one credit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityCreditBinding {
    generation: Arc<str>,
    policy_epoch: u64,
    partition: Arc<str>,
    model: Arc<str>,
    tenant: TenantKey,
    request_id: Arc<str>,
    estimated_output_tokens: u32,
}

impl CapacityCreditBinding {
    /// Build a validated binding for one allocator-selected request attempt.
    pub fn new(
        generation: impl AsRef<str>,
        policy_epoch: u64,
        partition: impl AsRef<str>,
        model: impl AsRef<str>,
        tenant: TenantKey,
        request_id: impl AsRef<str>,
        estimated_output_tokens: u32,
    ) -> Result<Self, CapacityCreditError> {
        let generation = checked_label("generation", generation.as_ref())?;
        let partition = checked_label("partition", partition.as_ref())?;
        let model = checked_label("model", model.as_ref())?;
        let request_id = checked_label("request id", request_id.as_ref())?;
        if tenant.as_str().is_empty() || tenant.as_str().trim() != tenant.as_str() {
            return Err(CapacityCreditError::InvalidBinding("tenant"));
        }
        if estimated_output_tokens == 0 {
            return Err(CapacityCreditError::InvalidBinding(
                "estimated output tokens",
            ));
        }
        Ok(Self {
            generation,
            policy_epoch,
            partition,
            model,
            tenant,
            request_id,
            estimated_output_tokens,
        })
    }

    /// Allocator generation allowed to use this credit.
    #[must_use]
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// Logical request-attempt identifier used for idempotent issue.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

fn checked_label(name: &'static str, value: &str) -> Result<Arc<str>, CapacityCreditError> {
    if value.is_empty() || value.trim() != value {
        return Err(CapacityCreditError::InvalidBinding(name));
    }
    Ok(Arc::from(value))
}

/// Result of issuing a new or idempotently repeated active credit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedCapacityCredit {
    pub token: CapacityCreditToken,
    pub expires_at: Instant,
    pub newly_issued: bool,
}

/// Successfully redeemed credit and its caller-owned capacity payload.
#[derive(Debug)]
pub struct RedeemedCapacityCredit<P> {
    pub binding: CapacityCreditBinding,
    pub payload: P,
}

/// Capacity-credit protocol failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum CapacityCreditError {
    #[error("invalid capacity credit token")]
    InvalidToken,
    #[error("invalid capacity credit binding field: {0}")]
    InvalidBinding(&'static str),
    #[error("capacity credit belongs to another allocator generation")]
    WrongGeneration,
    #[error("request id already has a credit with different bindings")]
    RequestConflict,
    #[error("capacity credit is unknown")]
    Unknown,
    #[error("capacity credit binding does not match")]
    BindingMismatch,
    #[error("capacity credit expired")]
    Expired,
    #[error("capacity credit was already redeemed")]
    AlreadyRedeemed,
    #[error("capacity credit was cancelled")]
    Cancelled,
    #[error("capacity credit registry state is inconsistent")]
    InternalState,
}

enum CreditState<P> {
    Active(P),
    Redeemed,
    Cancelled,
    Expired,
}

struct CreditRecord<P> {
    binding: CapacityCreditBinding,
    expires_at: Instant,
    terminal_until: Option<Instant>,
    state: CreditState<P>,
}

struct RegistryInner<P> {
    by_token: HashMap<CapacityCreditToken, CreditRecord<P>>,
    by_request: HashMap<Arc<str>, CapacityCreditToken>,
}

/// Process-local registry for one active SMG generation.
pub struct CapacityCreditRegistry<P> {
    generation: Arc<str>,
    ttl: Duration,
    terminal_retention: Duration,
    inner: Mutex<RegistryInner<P>>,
}

impl<P> CapacityCreditRegistry<P> {
    /// Create a registry with a nonzero active TTL and replay-tombstone TTL.
    pub fn new(
        generation: impl AsRef<str>,
        ttl: Duration,
        terminal_retention: Duration,
    ) -> Result<Self, CapacityCreditError> {
        let generation = checked_label("generation", generation.as_ref())?;
        if ttl.is_zero() {
            return Err(CapacityCreditError::InvalidBinding("credit ttl"));
        }
        if terminal_retention.is_zero() {
            return Err(CapacityCreditError::InvalidBinding("terminal retention"));
        }
        Ok(Self {
            generation,
            ttl,
            terminal_retention,
            inner: Mutex::new(RegistryInner {
                by_token: HashMap::new(),
                by_request: HashMap::new(),
            }),
        })
    }

    /// Issue a credit, or return the same active token for an exact retry.
    pub fn issue(
        &self,
        binding: CapacityCreditBinding,
        payload: P,
    ) -> Result<IssuedCapacityCredit, CapacityCreditError> {
        self.issue_at(binding, payload, Instant::now())
    }

    fn issue_at(
        &self,
        binding: CapacityCreditBinding,
        payload: P,
        now: Instant,
    ) -> Result<IssuedCapacityCredit, CapacityCreditError> {
        if binding.generation.as_ref() != self.generation.as_ref() {
            return Err(CapacityCreditError::WrongGeneration);
        }
        self.maintain_at(now);

        let mut unused_payload = Some(payload);
        let result = {
            let mut inner = self.inner.lock();
            if let Some(token) = inner.by_request.get(&binding.request_id).cloned() {
                let Some(record) = inner.by_token.get(&token) else {
                    return Err(CapacityCreditError::InternalState);
                };
                if record.binding == binding {
                    match record.state {
                        CreditState::Active(_) => Ok(IssuedCapacityCredit {
                            token,
                            expires_at: record.expires_at,
                            newly_issued: false,
                        }),
                        CreditState::Redeemed => Err(CapacityCreditError::AlreadyRedeemed),
                        CreditState::Cancelled => Err(CapacityCreditError::Cancelled),
                        CreditState::Expired => Err(CapacityCreditError::Expired),
                    }
                } else {
                    Err(CapacityCreditError::RequestConflict)
                }
            } else {
                let token = loop {
                    let candidate = CapacityCreditToken::random();
                    if !inner.by_token.contains_key(&candidate) {
                        break candidate;
                    }
                };
                let expires_at = now + self.ttl;
                inner
                    .by_request
                    .insert(Arc::clone(&binding.request_id), token.clone());
                inner.by_token.insert(
                    token.clone(),
                    CreditRecord {
                        binding,
                        expires_at,
                        terminal_until: None,
                        state: CreditState::Active(
                            unused_payload
                                .take()
                                .ok_or(CapacityCreditError::InternalState)?,
                        ),
                    },
                );
                Ok(IssuedCapacityCredit {
                    token,
                    expires_at,
                    newly_issued: true,
                })
            }
        };
        drop(unused_payload);
        result
    }

    /// Atomically consume a matching active credit at most once.
    pub fn redeem(
        &self,
        token: &CapacityCreditToken,
        presented: &CapacityCreditBinding,
    ) -> Result<RedeemedCapacityCredit<P>, CapacityCreditError> {
        self.redeem_at(token, presented, Instant::now())
    }

    fn redeem_at(
        &self,
        token: &CapacityCreditToken,
        presented: &CapacityCreditBinding,
        now: Instant,
    ) -> Result<RedeemedCapacityCredit<P>, CapacityCreditError> {
        let (result, expired_payload) = {
            let mut inner = self.inner.lock();
            let Some(record) = inner.by_token.get_mut(token) else {
                return Err(CapacityCreditError::Unknown);
            };
            if now >= record.expires_at {
                let payload = transition_terminal(
                    record,
                    CreditState::Expired,
                    now + self.terminal_retention,
                );
                (Err(CapacityCreditError::Expired), payload)
            } else if record.binding != *presented {
                (Err(CapacityCreditError::BindingMismatch), None)
            } else {
                match std::mem::replace(&mut record.state, CreditState::Redeemed) {
                    CreditState::Active(payload) => {
                        record.terminal_until = Some(now + self.terminal_retention);
                        (
                            Ok(RedeemedCapacityCredit {
                                binding: record.binding.clone(),
                                payload,
                            }),
                            None,
                        )
                    }
                    CreditState::Redeemed => {
                        record.state = CreditState::Redeemed;
                        (Err(CapacityCreditError::AlreadyRedeemed), None)
                    }
                    CreditState::Cancelled => {
                        record.state = CreditState::Cancelled;
                        (Err(CapacityCreditError::Cancelled), None)
                    }
                    CreditState::Expired => {
                        record.state = CreditState::Expired;
                        (Err(CapacityCreditError::Expired), None)
                    }
                }
            }
        };
        drop(expired_payload);
        result
    }

    /// Cancel an active credit. Returns `true` only for the first transition.
    pub fn cancel(&self, token: &CapacityCreditToken) -> bool {
        self.cancel_at(token, Instant::now())
    }

    fn cancel_at(&self, token: &CapacityCreditToken, now: Instant) -> bool {
        let payload = {
            let mut inner = self.inner.lock();
            let Some(record) = inner.by_token.get_mut(token) else {
                return false;
            };
            transition_terminal(
                record,
                CreditState::Cancelled,
                now + self.terminal_retention,
            )
        };
        let changed = payload.is_some();
        drop(payload);
        changed
    }

    /// Expire active credits and purge elapsed replay tombstones.
    pub fn maintain(&self) -> usize {
        self.maintain_at(Instant::now())
    }

    fn maintain_at(&self, now: Instant) -> usize {
        let mut dropped_payloads = Vec::new();
        let changed = {
            let mut inner = self.inner.lock();
            let mut expired = 0;
            for record in inner.by_token.values_mut() {
                if matches!(record.state, CreditState::Active(_)) && now >= record.expires_at {
                    if let Some(payload) = transition_terminal(
                        record,
                        CreditState::Expired,
                        now + self.terminal_retention,
                    ) {
                        dropped_payloads.push(payload);
                        expired += 1;
                    }
                }
            }

            let purge: Vec<_> = inner
                .by_token
                .iter()
                .filter_map(|(token, record)| {
                    record
                        .terminal_until
                        .filter(|until| now >= *until)
                        .map(|_| (token.clone(), Arc::clone(&record.binding.request_id)))
                })
                .collect();
            for (token, request_id) in &purge {
                inner.by_token.remove(token);
                inner.by_request.remove(request_id);
            }
            expired + purge.len()
        };
        drop(dropped_payloads);
        changed
    }

    /// Number of active payload-holding credits.
    #[must_use]
    pub fn active_len(&self) -> usize {
        self.inner
            .lock()
            .by_token
            .values()
            .filter(|record| matches!(record.state, CreditState::Active(_)))
            .count()
    }
}

fn transition_terminal<P>(
    record: &mut CreditRecord<P>,
    terminal: CreditState<P>,
    terminal_until: Instant,
) -> Option<P> {
    if !matches!(record.state, CreditState::Active(_)) {
        return None;
    }
    record.terminal_until = Some(terminal_until);
    match std::mem::replace(&mut record.state, terminal) {
        CreditState::Active(payload) => Some(payload),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Weak,
    };

    use super::*;

    fn binding(request_id: &str) -> CapacityCreditBinding {
        CapacityCreditBinding::new(
            "green-1",
            7,
            "kimi-k3",
            "kimi-k3",
            TenantKey::new("auth:junu"),
            request_id,
            1_024,
        )
        .unwrap()
    }

    fn registry<P>() -> CapacityCreditRegistry<P> {
        CapacityCreditRegistry::new("green-1", Duration::from_secs(2), Duration::from_secs(3))
            .unwrap()
    }

    #[test]
    fn token_round_trip_is_url_safe_and_debug_redacted() {
        let token = CapacityCreditToken::random();
        let encoded = token.expose_secret();
        assert_eq!(CapacityCreditToken::parse(&encoded).unwrap(), token);
        assert!(!encoded.contains('='));
        assert_eq!(format!("{token:?}"), "CapacityCreditToken([REDACTED])");
        assert_eq!(
            CapacityCreditToken::parse("not a token"),
            Err(CapacityCreditError::InvalidToken)
        );
    }

    #[test]
    fn validates_binding_generation_and_ttls() {
        assert_eq!(
            CapacityCreditBinding::new(" green", 1, "p", "m", TenantKey::new("auth:a"), "r", 1,),
            Err(CapacityCreditError::InvalidBinding("generation"))
        );
        assert!(matches!(
            CapacityCreditRegistry::<()>::new("green", Duration::ZERO, Duration::from_secs(1)),
            Err(CapacityCreditError::InvalidBinding("credit ttl"))
        ));
    }

    #[test]
    fn duplicate_issue_is_idempotent_and_conflict_is_rejected() {
        let registry = registry();
        let first = registry.issue(binding("request-1"), 1).unwrap();
        let repeated = registry.issue(binding("request-1"), 2).unwrap();
        assert!(first.newly_issued);
        assert!(!repeated.newly_issued);
        assert_eq!(first.token, repeated.token);
        assert_eq!(registry.active_len(), 1);

        let mut conflict = binding("request-1");
        conflict.estimated_output_tokens = 2_048;
        assert_eq!(
            registry.issue(conflict, 3),
            Err(CapacityCreditError::RequestConflict)
        );
    }

    #[test]
    fn mismatch_does_not_consume_and_redemption_is_single_use() {
        let registry = registry();
        let expected = binding("request-1");
        let issued = registry.issue(expected.clone(), "permit").unwrap();
        let mismatch = binding("request-2");
        assert_eq!(
            registry.redeem(&issued.token, &mismatch).unwrap_err(),
            CapacityCreditError::BindingMismatch
        );
        assert_eq!(registry.active_len(), 1);

        let redeemed = registry.redeem(&issued.token, &expected).unwrap();
        assert_eq!(redeemed.payload, "permit");
        assert_eq!(registry.active_len(), 0);
        assert_eq!(
            registry.redeem(&issued.token, &expected).unwrap_err(),
            CapacityCreditError::AlreadyRedeemed
        );
        assert_eq!(
            registry.issue(expected, "another"),
            Err(CapacityCreditError::AlreadyRedeemed)
        );
    }

    #[test]
    fn concurrent_redemption_has_exactly_one_winner() {
        let registry = Arc::new(registry());
        let expected = binding("request-1");
        let issued = registry.issue(expected.clone(), "permit").unwrap();
        let winners = (0..16)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let token = issued.token.clone();
                let expected = expected.clone();
                std::thread::spawn(move || registry.redeem(&token, &expected).is_ok())
            })
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1);
    }

    struct DropProbe {
        registry: Weak<CapacityCreditRegistry<DropProbe>>,
        dropped_unlocked: Arc<AtomicBool>,
        drop_count: Arc<AtomicUsize>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            if let Some(registry) = self.registry.upgrade() {
                self.dropped_unlocked
                    .store(registry.inner.try_lock().is_some(), Ordering::Release);
            }
            self.drop_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn drop_probe(
        registry: &Arc<CapacityCreditRegistry<DropProbe>>,
        dropped_unlocked: &Arc<AtomicBool>,
        drop_count: &Arc<AtomicUsize>,
    ) -> DropProbe {
        DropProbe {
            registry: Arc::downgrade(registry),
            dropped_unlocked: Arc::clone(dropped_unlocked),
            drop_count: Arc::clone(drop_count),
        }
    }

    #[test]
    fn cancel_and_expiry_drop_payloads_outside_registry_lock() {
        let registry = Arc::new(registry());
        let unlocked = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let first = registry
            .issue_at(
                binding("cancel"),
                drop_probe(&registry, &unlocked, &drops),
                start,
            )
            .unwrap();
        assert!(registry.cancel_at(&first.token, start));
        assert!(unlocked.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        unlocked.store(false, Ordering::Release);
        registry
            .issue_at(
                binding("expire"),
                drop_probe(&registry, &unlocked, &drops),
                start,
            )
            .unwrap();
        assert_eq!(registry.maintain_at(start + Duration::from_secs(2)), 1);
        assert!(unlocked.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn terminal_tombstone_blocks_replay_then_purges() {
        let registry = registry();
        let start = Instant::now();
        let expected = binding("request-1");
        let issued = registry.issue_at(expected.clone(), (), start).unwrap();
        registry
            .redeem_at(&issued.token, &expected, start + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            registry.issue_at(expected.clone(), (), start + Duration::from_secs(2)),
            Err(CapacityCreditError::AlreadyRedeemed)
        );
        assert_eq!(registry.maintain_at(start + Duration::from_secs(4)), 1);
        assert!(
            registry
                .issue_at(expected, (), start + Duration::from_secs(4))
                .unwrap()
                .newly_issued
        );
    }
}
