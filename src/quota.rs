//! Backend-neutral mutable quota state.
//!
//! Policy definitions are resolved and compiled before this layer is called.
//! Backends receive only immutable [`QuotaAction`] values and never query the
//! rate-limit, budget, API-key, usage-pool, connection, or model tables.
//!
//! **Two sources of truth, deliberately kept apart.** The policy — the
//! limit value, algorithm, and window — lives in `budget.rs`/`ratelimit.rs`'s
//! Postgres tables and is read once per request to build a [`QuotaAction`].
//! The *mutable* enforcement counter (the running balance or window usage
//! that a request actually debits) lives entirely in whichever
//! [`QuotaBackend`] is configured via `ROUTER_QUOTA_BACKEND`, and the two
//! backends offer very different consistency guarantees for that counter:
//! [`MemoryQuota`] is per-process, in-memory, and lost on restart — every
//! replica enforces against its own independent view, so it is a
//! development/test convenience, never a multi-replica source of truth.
//! [`PostgresQuota`] persists the counter in the database and is the only
//! backend that is consistent across replicas and restarts. Switching
//! `ROUTER_QUOTA_BACKEND` therefore changes not just where state lives but
//! whether "the quota" means one thing cluster-wide or one thing per process.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::QuotaBackendKind;

pub const LEASE_TTL: Duration = Duration::from_secs(90);
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
const RESERVATION_TTL: Duration = Duration::from_secs(10 * 60);
/// Tokens each token policy holds, beyond the visible input and output, for
/// a request whose provider reads input the router never sees: hosted tool
/// output, stored history, or opaque content such as images and encrypted
/// reasoning. Holding them at admission throttles parallel soft-cap requests,
/// and an unknown outcome keeps them as the charge.
pub const SOFT_CAP_RESERVE_TOKENS: u64 = 8_192;
/// Batch size for the incidental, whole-runtime sweep `acquire_many` performs
/// on every call. Kept small so an acquisition never pays for scanning every
/// policy's stale state; `expire_for_policies_locked` handles the policies
/// actually being evaluated, unbounded, since that set is always small.
const ACQUIRE_CLEANUP_BATCH: usize = 64;
/// Minimum gap between the incidental, whole-runtime `cleanup_locked` sweeps
/// `acquire_many` performs. The per-acquisition `expire_for_policies_locked`
/// pass already keeps a request's own policies accurate on every call, so
/// throttling the unrelated-backlog sweep to once per interval avoids paying
/// its (bounded, but nonzero) cost on every request under high volume.
const GLOBAL_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    RateLimit,
    BudgetLimit,
}

impl QuotaSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::BudgetLimit => "budget_limit",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaAlgorithm {
    TokenBucket,
    FixedWindow,
    Concurrency,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaMeter {
    Requests,
    Tokens,
    ConcurrentRequests,
}

impl QuotaMeter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requests => "requests",
            Self::Tokens => "tokens",
            Self::ConcurrentRequests => "concurrent_requests",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaAction {
    pub policy_id: String,
    pub environment_id: String,
    pub source: QuotaSource,
    pub algorithm: QuotaAlgorithm,
    pub meter: QuotaMeter,
    pub limit_value: u64,
    pub burst: Option<u64>,
    pub amount: u64,
    pub window_start: Option<DateTime<Utc>>,
    pub window_end: Option<DateTime<Utc>>,
}

/// A read-only projection of a fixed-window enforcement counter. This is
/// deliberately runtime state, not a historical usage ledger: it includes
/// conservative reservations until they are settled or expire.
#[derive(Debug, Clone)]
pub struct FixedWindowUsageQuery {
    pub policy_id: String,
    pub window_start: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaGrant {
    pub permit_id: uuid::Uuid,
    pub actions: Vec<QuotaAction>,
    pub has_concurrency: bool,
    /// Amount retained for this acquisition when an upstream dispatch fails.
    /// `None` retains the full conservative reservation.
    pub failed_amount: Option<u64>,
    pub background_owner: Option<BackgroundOwner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundOwner {
    pub environment_id: String,
    pub event_id: uuid::Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaExceeded {
    pub source: QuotaSource,
    pub meter: QuotaMeter,
    pub limit_value: u64,
    pub retry_after: Duration,
}

#[derive(Debug, Clone)]
pub enum QuotaDecision {
    Allowed(Option<QuotaGrant>),
    Limited(QuotaExceeded),
}

/// The request charge survives a route denial or route-backend failure.
/// Keep the phases separate so fallback and refunds retain their semantics.
#[derive(Debug, Clone)]
pub enum QuotaAdmission {
    RequestLimited(QuotaExceeded),
    RequestAllowed {
        request_grant: Option<QuotaGrant>,
        route: Result<QuotaDecision, QuotaBackendError>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaOutcome {
    Actual(u64),
    Unknown,
    Failed,
}

/// Whether token-denominated policies apply to one quota admission.
///
/// `Unknown` is distinct from `NotApplicable`: a token-metered operation with
/// no conservative estimate must be rejected when a matching token policy
/// exists, while a request-only operation simply omits token policies.
/// `SoftCap` is for requests whose provider adds input beyond the visible
/// bound: the visible bound plus a reserve is held, and usage beyond it is
/// charged as debt at settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenReservation {
    Known(u64),
    SoftCap(u64),
    Unknown,
    NotApplicable,
}

impl TokenReservation {
    pub fn from_option(value: Option<u64>) -> Self {
        value.map_or(Self::Unknown, Self::Known)
    }

    /// Mark a visible bound as a soft cap (see [`SOFT_CAP_RESERVE_TOKENS`]).
    pub fn with_soft_cap(self, soft_cap: bool) -> Self {
        match self {
            Self::Known(visible) if soft_cap => Self::SoftCap(visible),
            other => other,
        }
    }

    /// The amount one token policy with `capacity` (its limit or burst) holds.
    /// The soft-cap reserve is clamped to capacity so it never makes a request
    /// inadmissible on its own.
    pub fn amount_for(self, capacity: u64) -> Option<u64> {
        match self {
            Self::Known(amount) => Some(amount),
            Self::SoftCap(visible) => Some(
                visible
                    .saturating_add(SOFT_CAP_RESERVE_TOKENS)
                    .min(capacity)
                    .max(visible),
            ),
            Self::Unknown | Self::NotApplicable => None,
        }
    }

    /// Return the conservative upper bound across every route that may serve
    /// one logical request. One unbounded candidate makes the whole request
    /// unbounded; silently skipping it could under-reserve before fallback.
    pub fn conservative_max(reservations: impl IntoIterator<Item = Option<u64>>) -> Self {
        let mut maximum = 0;
        for reservation in reservations {
            let Some(reservation) = reservation else {
                return Self::Unknown;
            };
            maximum = maximum.max(reservation);
        }
        Self::Known(maximum)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaBackendErrorKind {
    Unavailable,
    /// No pool connection became available. Further admissions for the same
    /// request would queue behind the same exhausted pool.
    PoolExhausted,
    /// Admission gave up waiting for another request's quota row lock. The
    /// policy is too contended to evaluate, not an unavailable backend.
    Contended,
}

#[derive(Debug, Clone)]
pub struct QuotaBackendError {
    message: String,
    kind: QuotaBackendErrorKind,
}

impl QuotaBackendError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: QuotaBackendErrorKind::Unavailable,
        }
    }

    pub fn pool_exhausted(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: QuotaBackendErrorKind::PoolExhausted,
        }
    }

    pub fn contended(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: QuotaBackendErrorKind::Contended,
        }
    }

    pub fn is_pool_exhausted(&self) -> bool {
        self.kind == QuotaBackendErrorKind::PoolExhausted
    }

    pub fn is_contended(&self) -> bool {
        self.kind == QuotaBackendErrorKind::Contended
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::unavailable(message)
    }

    fn database(error: sqlx::Error) -> Self {
        if crate::db_error::is_pool_exhausted(&error) {
            tracing::error!(%error, "quota backend pool exhausted");
            return Self::pool_exhausted("database pool exhausted");
        }
        if crate::db_error::is_lock_timeout(&error) {
            tracing::warn!(%error, "quota admission lock wait timed out");
            return Self::contended("quota state lock wait timed out");
        }
        tracing::error!(%error, "quota backend database error");
        Self::unavailable("database error")
    }
}

impl std::fmt::Display for QuotaBackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for QuotaBackendError {}

#[async_trait]
pub trait QuotaBackend: Send + Sync {
    fn enabled(&self) -> bool;

    /// Opt in to atomic foreground accounting with a usage tracker sharing
    /// this exact pool. Other backends keep the ordinary settlement path.
    fn postgres_pool(&self) -> Option<&sqlx::PgPool> {
        None
    }

    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError>;

    /// Admit the logical request, then its first foreground route. Backends
    /// without a combined primitive retain the same ordered two-phase behavior.
    async fn acquire_request_and_route(
        &self,
        request_actions: &[QuotaAction],
        route_actions: &[QuotaAction],
        route_failed_amount: Option<u64>,
    ) -> Result<QuotaAdmission, QuotaBackendError> {
        match self.acquire_many(request_actions, None).await? {
            QuotaDecision::Limited(exceeded) => Ok(QuotaAdmission::RequestLimited(exceeded)),
            QuotaDecision::Allowed(request_grant) => Ok(QuotaAdmission::RequestAllowed {
                request_grant,
                route: self.acquire_many(route_actions, route_failed_amount).await,
            }),
        }
    }

    async fn acquire_background(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
        _owner: &BackgroundOwner,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        if self.enabled() {
            return Err(QuotaBackendError::invalid(
                "quota backend does not support background ownership",
            ));
        }
        self.acquire_many(actions, failed_amount).await
    }

    async fn finish_background(
        &self,
        _owner: &BackgroundOwner,
        _outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        Ok(())
    }

    async fn finish_background_in_transaction(
        &self,
        _connection: &mut sqlx::PgConnection,
        _owner: &BackgroundOwner,
        _outcome: QuotaOutcome,
    ) -> Result<bool, QuotaBackendError> {
        Ok(false)
    }

    async fn finish(
        &self,
        grant: &QuotaGrant,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError>;

    async fn renew(&self, permit_id: uuid::Uuid) -> Result<(), QuotaBackendError>;

    /// Bounded maintenance for cold policies. Backends with native expiry may
    /// implement this as a no-op or an atomic repair pass.
    async fn cleanup(&self, batch_size: u64) -> Result<(u64, u64), QuotaBackendError>;

    /// Return admitted usage for each requested current fixed window. `None`
    /// means the backend has no enforcement state (for example, disabled).
    async fn fixed_window_usage(
        &self,
        queries: &[FixedWindowUsageQuery],
    ) -> Result<Option<HashMap<String, u64>>, QuotaBackendError> {
        let _ = queries;
        Ok(None)
    }
}

struct PermitPart {
    backend: Arc<dyn QuotaBackend>,
    grant: QuotaGrant,
    stop_renewal: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Owns one or more acquisitions for one client request. Request-level quota
/// admission and successful-dispatch rate admission are separate atomic
/// acquisitions, then travel together so response handling reconciles both
/// exactly once.
pub struct QuotaPermit {
    parts: Vec<PermitPart>,
}

impl QuotaPermit {
    pub(crate) fn can_account_with(&self, pool: &sqlx::PgPool, environment_id: &str) -> bool {
        self.parts.iter().all(|part| {
            part.backend.postgres_pool().is_some_and(|backend_pool| {
                std::ptr::eq(pool.options(), backend_pool.options())
                    && part.grant.background_owner.is_none()
                    && part
                        .grant
                        .actions
                        .iter()
                        .all(|action| action.environment_id == environment_id)
            })
        })
    }

    pub(crate) fn accounting_grants(&self) -> Vec<&QuotaGrant> {
        self.parts.iter().map(|part| &part.grant).collect()
    }

    /// Called only after acknowledgement of the atomic accounting statement.
    /// Until then, this permit still owns renewal and cancellation cleanup.
    pub(crate) fn accounting_committed(mut self) {
        self.parts.clear();
    }

    pub fn new(backend: Arc<dyn QuotaBackend>, grant: QuotaGrant) -> Self {
        // Long requests keep their token reservations alive as well as their
        // leases, so settlement still finds what admission held.
        let renews = grant.has_concurrency
            || grant
                .actions
                .iter()
                .any(|action| action.meter == QuotaMeter::Tokens);
        let stop_renewal = if renews && grant.background_owner.is_none() {
            let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
            let renew_backend = backend.clone();
            let permit_id = grant.permit_id;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(LEASE_RENEW_INTERVAL);
                interval.tick().await;
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        _ = interval.tick() => {
                            if let Err(error) = renew_backend.renew(permit_id).await {
                                tracing::error!(%permit_id, %error, "quota lease renewal failed");
                            }
                        }
                    }
                }
            });
            Some(stop_tx)
        } else {
            None
        };
        Self {
            parts: vec![PermitPart {
                backend,
                grant,
                stop_renewal,
            }],
        }
    }

    pub fn merge(first: Option<Self>, second: Option<Self>) -> Option<Self> {
        match (first, second) {
            (Some(mut first), Some(mut second)) => {
                first.parts.append(&mut second.parts);
                Some(first)
            }
            (Some(first), None) => Some(first),
            (None, Some(second)) => Some(second),
            (None, None) => None,
        }
    }

    async fn complete(mut self, outcome_for: impl Fn(&QuotaGrant) -> QuotaOutcome) {
        let parts = std::mem::take(&mut self.parts);
        for mut part in parts {
            part.stop_renewal.take();
            let outcome = outcome_for(&part.grant);
            if let Err(error) = part.backend.finish(&part.grant, outcome).await {
                tracing::error!(permit_id = %part.grant.permit_id, %error, "quota reconciliation failed");
            }
        }
    }

    pub async fn finish(self, actual_amount: Option<u64>) {
        self.complete(|_| actual_amount.map_or(QuotaOutcome::Unknown, QuotaOutcome::Actual))
            .await;
    }

    pub async fn finish_failed(self) {
        self.complete(|grant| {
            grant
                .failed_amount
                .map_or(QuotaOutcome::Unknown, QuotaOutcome::Actual)
        })
        .await;
    }

    pub async fn refund(self) {
        self.complete(|_| QuotaOutcome::Actual(0)).await;
    }
}

impl Drop for QuotaPermit {
    fn drop(&mut self) {
        for mut part in std::mem::take(&mut self.parts) {
            if part.grant.background_owner.is_some() {
                continue;
            }
            part.stop_renewal.take();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(error) = part
                        .backend
                        .finish(&part.grant, QuotaOutcome::Unknown)
                        .await
                    {
                        tracing::error!(permit_id = %part.grant.permit_id, %error, "dropped quota permit cleanup failed");
                    }
                });
            }
        }
    }
}

pub struct DisabledQuota;

#[async_trait]
impl QuotaBackend for DisabledQuota {
    fn enabled(&self) -> bool {
        false
    }

    async fn acquire_many(
        &self,
        _actions: &[QuotaAction],
        _failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        Ok(QuotaDecision::Allowed(None))
    }

    async fn finish(
        &self,
        _grant: &QuotaGrant,
        _outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        Ok(())
    }

    async fn renew(&self, _permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        Ok(())
    }

    async fn cleanup(&self, _batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        Ok((0, 0))
    }

    async fn fixed_window_usage(
        &self,
        _queries: &[FixedWindowUsageQuery],
    ) -> Result<Option<HashMap<String, u64>>, QuotaBackendError> {
        Ok(None)
    }
}

struct ReservedState {
    amount: u64,
    window_start: Option<DateTime<Utc>>,
    expires_at: Instant,
    background_owner: Option<BackgroundOwner>,
    settlement: Option<ReservationSettlement>,
}

struct ReservationSettlement {
    algorithm: QuotaAlgorithm,
    limit_value: u64,
    burst: Option<u64>,
    failed_amount: Option<u64>,
    window_end: Option<DateTime<Utc>>,
}

impl ReservationSettlement {
    fn from_action(action: &QuotaAction, failed_amount: Option<u64>) -> Self {
        Self {
            algorithm: action.algorithm,
            limit_value: action.limit_value,
            burst: action.burst,
            failed_amount,
            window_end: action.window_end,
        }
    }

    /// Postgres background reservations do not store the window end, so the
    /// memory backend forgets it too and both settle rollovers identically.
    fn for_background(action: &QuotaAction, failed_amount: Option<u64>) -> Self {
        Self {
            window_end: None,
            ..Self::from_action(action, failed_amount)
        }
    }
}

struct LeaseState {
    expires_at: Instant,
    background_owner: Option<BackgroundOwner>,
}

struct MemoryPolicyState {
    balance: Option<f64>,
    updated_at: Option<Instant>,
    window_start: Option<DateTime<Utc>>,
    used_amount: u64,
    active_count: u64,
}

#[derive(Default)]
struct MemoryRuntime {
    states: HashMap<String, MemoryPolicyState>,
    reservations: HashMap<(String, uuid::Uuid), ReservedState>,
    reservations_by_permit: HashMap<uuid::Uuid, Vec<String>>,
    leases: HashMap<(String, uuid::Uuid), LeaseState>,
    /// permit_id -> policy_ids holding a concurrency lease under that permit.
    /// Lets `renew` update a permit's lease(s) directly instead of scanning
    /// every lease in the runtime.
    leases_by_permit: HashMap<uuid::Uuid, Vec<String>>,
    /// policy_id -> permit_ids holding a concurrency lease for that policy.
    /// The concurrency retry-after computation needs the minimum lease
    /// expiry *within one policy*, which `leases_by_permit` cannot answer, so
    /// it is kept as its own index rather than folded into that one.
    leases_by_policy: HashMap<String, HashSet<uuid::Uuid>>,
    /// policy_id -> permit_ids holding a token/fixed-window reservation for
    /// that policy. Lets `expire_for_policies_locked` reap a fresh
    /// acquisition's own stale reservations in `O(policies being evaluated)`
    /// instead of scanning every reservation in the runtime.
    reservations_by_policy: HashMap<String, HashSet<uuid::Uuid>>,
    /// Earliest time `acquire_many` may next run the bounded, whole-runtime
    /// `cleanup_locked` sweep; `None` means it has never run. Throttles that
    /// incidental sweep to at most once per `GLOBAL_CLEANUP_INTERVAL`
    /// regardless of request volume.
    next_global_cleanup: Option<Instant>,
}

#[derive(Default)]
pub struct MemoryQuota {
    runtime: Mutex<MemoryRuntime>,
}

impl MemoryQuota {
    pub fn new() -> Self {
        Self::default()
    }

    fn validate_actions(actions: &[QuotaAction]) -> Result<(), QuotaBackendError> {
        let mut ids = HashSet::new();
        let environment = actions.first().map(|action| action.environment_id.as_str());
        for action in actions {
            if action.environment_id.is_empty() {
                return Err(QuotaBackendError::invalid(
                    "quota actions require a non-empty environment id",
                ));
            }
            if action.policy_id.is_empty() || !ids.insert(action.policy_id.as_str()) {
                return Err(QuotaBackendError::invalid(
                    "quota actions require unique non-empty policy ids",
                ));
            }
            if Some(action.environment_id.as_str()) != environment {
                return Err(QuotaBackendError::invalid(
                    "one acquisition cannot span environments",
                ));
            }
            if action.limit_value == 0 || action.limit_value > i64::MAX as u64 {
                return Err(QuotaBackendError::invalid("invalid quota limit"));
            }
            if action.amount > i64::MAX as u64 {
                return Err(QuotaBackendError::invalid("invalid quota amount"));
            }
            match action.algorithm {
                QuotaAlgorithm::TokenBucket if action.burst.is_none_or(|burst| burst == 0) => {
                    return Err(QuotaBackendError::invalid(
                        "token buckets require a positive burst",
                    ));
                }
                QuotaAlgorithm::FixedWindow if !matches!((action.window_start, action.window_end), (Some(start), Some(end)) if end > start) =>
                {
                    return Err(QuotaBackendError::invalid(
                        "fixed windows require valid boundaries",
                    ));
                }
                QuotaAlgorithm::Concurrency
                    if action.amount != 1
                        || action.burst.is_some()
                        || action.meter != QuotaMeter::ConcurrentRequests =>
                {
                    return Err(QuotaBackendError::invalid(
                        "concurrency actions require amount one, no burst, and the concurrency meter",
                    ));
                }
                _ => {}
            }
            if action.source == QuotaSource::BudgetLimit
                && (action.algorithm != QuotaAlgorithm::FixedWindow
                    || action.meter != QuotaMeter::Tokens)
            {
                return Err(QuotaBackendError::invalid(
                    "budget actions must be fixed-window token quotas",
                ));
            }
        }
        Ok(())
    }

    /// Insert a concurrency lease, keeping `leases_by_permit` and
    /// `leases_by_policy` in sync with `leases`.
    fn insert_lease_locked(
        runtime: &mut MemoryRuntime,
        policy_id: &str,
        permit_id: uuid::Uuid,
        expiry: Instant,
        background_owner: Option<BackgroundOwner>,
    ) {
        runtime.leases.insert(
            (policy_id.to_string(), permit_id),
            LeaseState {
                expires_at: expiry,
                background_owner,
            },
        );
        runtime
            .leases_by_permit
            .entry(permit_id)
            .or_default()
            .push(policy_id.to_string());
        runtime
            .leases_by_policy
            .entry(policy_id.to_string())
            .or_default()
            .insert(permit_id);
    }

    /// Remove a lease by its `(policy_id, permit_id)` key, decrementing the
    /// policy's active count and keeping the secondary indices in sync.
    /// Returns whether a lease was actually removed.
    fn remove_lease_locked(runtime: &mut MemoryRuntime, key: &(String, uuid::Uuid)) -> bool {
        let (policy_id, permit_id) = key;
        if runtime.leases.remove(key).is_none() {
            return false;
        }
        if let Some(state) = runtime.states.get_mut(policy_id) {
            state.active_count = state.active_count.saturating_sub(1);
        }
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            runtime.leases_by_permit.entry(*permit_id)
        {
            entry.get_mut().retain(|id| id != policy_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            runtime.leases_by_policy.entry(policy_id.clone())
        {
            entry.get_mut().remove(permit_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
        true
    }

    /// Insert a reservation, keeping `reservations_by_policy` in sync with
    /// `reservations`.
    fn insert_reservation_locked(
        runtime: &mut MemoryRuntime,
        policy_id: &str,
        permit_id: uuid::Uuid,
        reservation: ReservedState,
    ) {
        runtime
            .reservations
            .insert((policy_id.to_string(), permit_id), reservation);
        runtime
            .reservations_by_permit
            .entry(permit_id)
            .or_default()
            .push(policy_id.to_string());
        runtime
            .reservations_by_policy
            .entry(policy_id.to_string())
            .or_default()
            .insert(permit_id);
    }

    /// Remove a reservation by its `(policy_id, permit_id)` key, keeping
    /// `reservations_by_policy` in sync. Returns the removed reservation, if
    /// any.
    fn remove_reservation_locked(
        runtime: &mut MemoryRuntime,
        key: &(String, uuid::Uuid),
    ) -> Option<ReservedState> {
        let removed = runtime.reservations.remove(key);
        if removed.is_some() {
            let (policy_id, permit_id) = key;
            if let std::collections::hash_map::Entry::Occupied(mut entry) =
                runtime.reservations_by_permit.entry(*permit_id)
            {
                entry.get_mut().retain(|id| id != policy_id);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
            if let std::collections::hash_map::Entry::Occupied(mut entry) =
                runtime.reservations_by_policy.entry(policy_id.clone())
            {
                entry.get_mut().remove(permit_id);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
        removed
    }

    /// Expire reservations and leases belonging only to `policy_ids`,
    /// unbounded. Called before evaluating a fresh acquisition so its
    /// decision is never skewed by its *own* policies' stale entries,
    /// without paying to scan every other policy's state (that incidental,
    /// bounded drain is `cleanup_locked`'s job).
    fn expire_for_policies_locked(
        runtime: &mut MemoryRuntime,
        now: Instant,
        policy_ids: &[String],
    ) -> (u64, u64) {
        let mut expired_reservations = 0u64;
        for policy_id in policy_ids {
            let expired: Vec<uuid::Uuid> = runtime
                .reservations_by_policy
                .get(policy_id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|permit| {
                    runtime
                        .reservations
                        .get(&(policy_id.clone(), *permit))
                        .is_some_and(|reservation| {
                            reservation.background_owner.is_none() && reservation.expires_at <= now
                        })
                })
                .collect();
            for permit in expired {
                Self::remove_reservation_locked(runtime, &(policy_id.clone(), permit));
                expired_reservations += 1;
            }
        }

        let mut expired_leases = 0u64;
        for policy_id in policy_ids {
            let expired: Vec<uuid::Uuid> = runtime
                .leases_by_policy
                .get(policy_id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|permit| {
                    runtime
                        .leases
                        .get(&(policy_id.clone(), *permit))
                        .is_some_and(|lease| {
                            lease.background_owner.is_none() && lease.expires_at <= now
                        })
                })
                .collect();
            for permit in expired {
                Self::remove_lease_locked(runtime, &(policy_id.clone(), permit));
                expired_leases += 1;
            }
        }

        (expired_reservations, expired_leases)
    }

    fn cleanup_locked(runtime: &mut MemoryRuntime, now: Instant, batch_size: usize) -> (u64, u64) {
        let expired_reservations: Vec<_> = runtime
            .reservations
            .iter()
            .filter(|(_, reservation)| {
                reservation.expires_at <= now && reservation.background_owner.is_none()
            })
            .take(batch_size)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired_reservations {
            Self::remove_reservation_locked(runtime, key);
        }

        let expired_leases: Vec<_> = runtime
            .leases
            .iter()
            .filter(|(_, lease)| lease.expires_at <= now && lease.background_owner.is_none())
            .take(batch_size)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired_leases {
            Self::remove_lease_locked(runtime, key);
        }
        (
            expired_reservations.len() as u64,
            expired_leases.len() as u64,
        )
    }

    fn finish_locked(
        runtime: &mut MemoryRuntime,
        permit_id: uuid::Uuid,
        outcome: QuotaOutcome,
        settlements: &HashMap<String, ReservationSettlement>,
    ) {
        let now = Instant::now();
        let policy_ids = runtime
            .reservations_by_permit
            .get(&permit_id)
            .cloned()
            .unwrap_or_default();
        for policy_id in policy_ids {
            let Some(reservation) =
                Self::remove_reservation_locked(runtime, &(policy_id.clone(), permit_id))
            else {
                continue;
            };
            let Some(settlement) = reservation
                .settlement
                .as_ref()
                .or_else(|| settlements.get(&policy_id))
            else {
                continue;
            };
            let actual = match outcome {
                QuotaOutcome::Actual(actual) => Some(actual),
                QuotaOutcome::Unknown => None,
                QuotaOutcome::Failed => settlement.failed_amount,
            };
            if let (Some(actual), Some(state)) = (actual, runtime.states.get_mut(&policy_id)) {
                match settlement.algorithm {
                    QuotaAlgorithm::TokenBucket => {
                        let elapsed = now
                            .saturating_duration_since(state.updated_at.unwrap_or(now))
                            .as_secs_f64();
                        let burst = settlement.burst.unwrap_or_default() as f64;
                        let refilled = (state.balance.unwrap_or(burst)
                            + elapsed * settlement.limit_value as f64 / 60.0)
                            .min(burst);
                        state.balance =
                            Some((refilled + reservation.amount as f64 - actual as f64).min(burst));
                        state.updated_at = Some(now);
                    }
                    QuotaAlgorithm::FixedWindow => {
                        let excess = actual.saturating_sub(reservation.amount);
                        if state.window_start == reservation.window_start {
                            if settlement.window_end.is_some_and(|end| end <= Utc::now()) {
                                // The admitting window closed and no admission
                                // has rolled the counter yet. Open the next
                                // window with the excess so the reset keeps it.
                                state.window_start = settlement.window_end;
                                state.used_amount = excess;
                            } else {
                                state.used_amount = state
                                    .used_amount
                                    .saturating_sub(reservation.amount)
                                    .saturating_add(actual);
                            }
                        } else if state.window_start > reservation.window_start {
                            // A later admission already opened a newer window.
                            state.used_amount = state.used_amount.saturating_add(excess);
                        }
                    }
                    QuotaAlgorithm::Concurrency => {}
                }
            }
        }
        let policy_ids = runtime
            .leases_by_permit
            .get(&permit_id)
            .cloned()
            .unwrap_or_default();
        for policy_id in policy_ids {
            Self::remove_lease_locked(runtime, &(policy_id, permit_id));
        }
    }
}

impl MemoryQuota {
    async fn acquire_owned(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
        owner: Option<&BackgroundOwner>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        Self::validate_actions(actions)?;
        if actions.is_empty() {
            return Ok(QuotaDecision::Allowed(None));
        }
        let mut actions = actions.to_vec();
        actions.sort_by(|left, right| left.policy_id.cmp(&right.policy_id));
        let now = Instant::now();
        let now_utc = Utc::now();
        let mut runtime = self.runtime.lock().expect("quota runtime lock");
        // Expire this acquisition's own policies fully so their stale state
        // never skews the decision below, then, at most once per
        // `GLOBAL_CLEANUP_INTERVAL`, drain a small, constant batch of the
        // rest of the runtime so other policies' expired entries still get
        // reclaimed gradually without every acquisition paying to scan the
        // whole runtime.
        let policy_ids: Vec<String> = actions.iter().map(|a| a.policy_id.clone()).collect();
        Self::expire_for_policies_locked(&mut runtime, now, &policy_ids);
        if runtime.next_global_cleanup.is_none_or(|next| now >= next) {
            Self::cleanup_locked(&mut runtime, now, ACQUIRE_CLEANUP_BATCH);
            runtime.next_global_cleanup = Some(now + GLOBAL_CLEANUP_INTERVAL);
        }

        let mut rejection: Option<QuotaExceeded> = None;
        for action in &actions {
            let state = runtime
                .states
                .entry(action.policy_id.clone())
                .or_insert_with(|| MemoryPolicyState {
                    balance: (action.algorithm == QuotaAlgorithm::TokenBucket)
                        .then(|| action.burst.unwrap_or_default() as f64),
                    updated_at: (action.algorithm == QuotaAlgorithm::TokenBucket).then_some(now),
                    window_start: action.window_start,
                    used_amount: 0,
                    active_count: 0,
                });

            let retry_after = match action.algorithm {
                QuotaAlgorithm::TokenBucket => {
                    let elapsed = now
                        .saturating_duration_since(state.updated_at.unwrap_or(now))
                        .as_secs_f64();
                    let burst = action.burst.unwrap_or_default() as f64;
                    let refilled = (state.balance.unwrap_or(burst)
                        + elapsed * action.limit_value as f64 / 60.0)
                        .min(burst);
                    state.balance = Some(refilled);
                    state.updated_at = Some(now);
                    (refilled < action.amount as f64).then(|| {
                        Duration::from_secs_f64(
                            ((action.amount as f64 - refilled) * 60.0 / action.limit_value as f64)
                                .max(0.001),
                        )
                    })
                }
                QuotaAlgorithm::FixedWindow => {
                    if state.window_start != action.window_start {
                        state.window_start = action.window_start;
                        state.used_amount = 0;
                    }
                    action
                        .amount
                        .checked_add(state.used_amount)
                        .filter(|total| *total <= action.limit_value)
                        .is_none()
                        .then(|| {
                            action
                                .window_end
                                .unwrap_or(now_utc)
                                .signed_duration_since(now_utc)
                                .to_std()
                                .unwrap_or(Duration::from_millis(1))
                                .max(Duration::from_millis(1))
                        })
                }
                QuotaAlgorithm::Concurrency => {
                    (state.active_count >= action.limit_value).then(|| {
                        runtime
                            .leases_by_policy
                            .get(&action.policy_id)
                            .into_iter()
                            .flatten()
                            .filter_map(|permit| {
                                runtime.leases.get(&(action.policy_id.clone(), *permit))
                            })
                            .map(|lease| lease.expires_at.saturating_duration_since(now))
                            .min()
                            .unwrap_or(Duration::from_secs(1))
                    })
                }
            };

            if let Some(retry_after) = retry_after {
                let exceeded = QuotaExceeded {
                    source: action.source,
                    meter: action.meter,
                    limit_value: action.limit_value,
                    retry_after,
                };
                if rejection
                    .as_ref()
                    .is_none_or(|current| retry_after >= current.retry_after)
                {
                    rejection = Some(exceeded);
                }
            }
        }

        if let Some(exceeded) = rejection {
            return Ok(QuotaDecision::Limited(exceeded));
        }

        let permit_id = uuid::Uuid::new_v4();
        let mut needs_finish = false;
        let mut has_concurrency = false;
        for action in &actions {
            let state = runtime
                .states
                .get_mut(&action.policy_id)
                .expect("quota state inserted");
            match action.algorithm {
                QuotaAlgorithm::TokenBucket => {
                    state.balance = Some(state.balance.unwrap_or_default() - action.amount as f64);
                }
                QuotaAlgorithm::FixedWindow => {
                    state.used_amount = state.used_amount.saturating_add(action.amount);
                }
                QuotaAlgorithm::Concurrency => {
                    state.active_count = state.active_count.saturating_add(1);
                    Self::insert_lease_locked(
                        &mut runtime,
                        &action.policy_id,
                        permit_id,
                        now + LEASE_TTL,
                        owner.cloned(),
                    );
                    needs_finish = true;
                    has_concurrency = true;
                }
            }
            if action.meter == QuotaMeter::Tokens {
                let expiry = if action.algorithm == QuotaAlgorithm::FixedWindow {
                    action
                        .window_end
                        .and_then(|end| end.signed_duration_since(now_utc).to_std().ok())
                        .map(|until_end| now + until_end.min(RESERVATION_TTL))
                        .unwrap_or(now + RESERVATION_TTL)
                } else {
                    now + RESERVATION_TTL
                };
                Self::insert_reservation_locked(
                    &mut runtime,
                    &action.policy_id,
                    permit_id,
                    ReservedState {
                        amount: action.amount,
                        window_start: action.window_start,
                        expires_at: expiry,
                        background_owner: owner.cloned(),
                        settlement: owner
                            .map(|_| ReservationSettlement::for_background(action, failed_amount)),
                    },
                );
                needs_finish = true;
            }
        }

        let grant = needs_finish.then(|| QuotaGrant {
            permit_id,
            actions,
            has_concurrency,
            failed_amount,
            background_owner: owner.cloned(),
        });
        Ok(QuotaDecision::Allowed(grant))
    }
}

#[async_trait]
impl QuotaBackend for MemoryQuota {
    fn enabled(&self) -> bool {
        true
    }

    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        self.acquire_owned(actions, failed_amount, None).await
    }

    async fn acquire_background(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
        owner: &BackgroundOwner,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        if actions
            .iter()
            .any(|action| action.environment_id != owner.environment_id)
        {
            return Err(QuotaBackendError::invalid(
                "background ownership crossed an environment boundary",
            ));
        }
        self.acquire_owned(actions, failed_amount, Some(owner))
            .await
    }

    async fn finish_background(
        &self,
        owner: &BackgroundOwner,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        let mut runtime = self.runtime.lock().expect("quota runtime lock");
        let permits: HashSet<_> = runtime
            .reservations
            .iter()
            .filter(|(_, reservation)| reservation.background_owner.as_ref() == Some(owner))
            .map(|(key, _)| key.1)
            .chain(
                runtime
                    .leases
                    .iter()
                    .filter(|(_, lease)| lease.background_owner.as_ref() == Some(owner))
                    .map(|(key, _)| key.1),
            )
            .collect();
        for permit_id in permits {
            Self::finish_locked(&mut runtime, permit_id, outcome, &HashMap::new());
        }
        Ok(())
    }

    async fn finish(
        &self,
        grant: &QuotaGrant,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        let settlements = grant
            .actions
            .iter()
            .filter(|action| action.meter == QuotaMeter::Tokens)
            .map(|action| {
                (
                    action.policy_id.clone(),
                    ReservationSettlement::from_action(action, grant.failed_amount),
                )
            })
            .collect();
        let mut runtime = self.runtime.lock().expect("quota runtime lock");
        Self::finish_locked(&mut runtime, grant.permit_id, outcome, &settlements);
        Ok(())
    }

    async fn renew(&self, permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        let now = Instant::now();
        let mut runtime = self.runtime.lock().expect("quota runtime lock");
        if let Some(policy_ids) = runtime.leases_by_permit.get(&permit_id).cloned() {
            for policy_id in policy_ids {
                if let Some(current) = runtime.leases.get_mut(&(policy_id, permit_id)) {
                    current.expires_at = now + LEASE_TTL;
                }
            }
        }
        if let Some(policy_ids) = runtime.reservations_by_permit.get(&permit_id).cloned() {
            for policy_id in policy_ids {
                if let Some(current) = runtime.reservations.get_mut(&(policy_id, permit_id)) {
                    if current.background_owner.is_none() {
                        current.expires_at = now + RESERVATION_TTL;
                    }
                }
            }
        }
        Ok(())
    }

    async fn cleanup(&self, batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        let mut runtime = self.runtime.lock().expect("quota runtime lock");
        Ok(Self::cleanup_locked(
            &mut runtime,
            Instant::now(),
            usize::try_from(batch_size).unwrap_or(usize::MAX),
        ))
    }

    async fn fixed_window_usage(
        &self,
        queries: &[FixedWindowUsageQuery],
    ) -> Result<Option<HashMap<String, u64>>, QuotaBackendError> {
        let runtime = self.runtime.lock().expect("quota runtime lock");
        Ok(Some(
            queries
                .iter()
                .map(|query| {
                    let used = runtime
                        .states
                        .get(&query.policy_id)
                        .filter(|state| state.window_start == Some(query.window_start))
                        .map_or(0, |state| state.used_amount);
                    (query.policy_id.clone(), used)
                })
                .collect(),
        ))
    }
}

pub struct PostgresQuota {
    pool: sqlx::PgPool,
}

#[derive(Deserialize)]
struct PostgresAdmissionDecision {
    allowed: bool,
    needs_finish: bool,
    has_concurrency: bool,
    retry_after_ms: i64,
    rejected_source: Option<String>,
    rejected_meter: Option<String>,
    rejected_limit: Option<i64>,
}

impl PostgresAdmissionDecision {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            allowed: row.try_get("allowed")?,
            needs_finish: row.try_get("needs_finish")?,
            has_concurrency: row.try_get("has_concurrency")?,
            retry_after_ms: row.try_get("retry_after_ms")?,
            rejected_source: row.try_get("rejected_source")?,
            rejected_meter: row.try_get("rejected_meter")?,
            rejected_limit: row.try_get("rejected_limit")?,
        })
    }

    fn decision(
        self,
        permit_id: uuid::Uuid,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
        owner: Option<&BackgroundOwner>,
    ) -> QuotaDecision {
        if !self.allowed {
            return QuotaDecision::Limited(QuotaExceeded {
                source: if self.rejected_source.as_deref() == Some("budget_limit") {
                    QuotaSource::BudgetLimit
                } else {
                    QuotaSource::RateLimit
                },
                meter: match self.rejected_meter.as_deref() {
                    Some("tokens") => QuotaMeter::Tokens,
                    Some("concurrent_requests") => QuotaMeter::ConcurrentRequests,
                    _ => QuotaMeter::Requests,
                },
                limit_value: self.rejected_limit.unwrap_or_default().max(0) as u64,
                retry_after: Duration::from_millis(self.retry_after_ms.max(1) as u64),
            });
        }
        QuotaDecision::Allowed(self.needs_finish.then(|| QuotaGrant {
            permit_id,
            actions: actions.to_vec(),
            has_concurrency: self.has_concurrency,
            failed_amount,
            background_owner: owner.cloned(),
        }))
    }
}

impl PostgresQuota {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl PostgresQuota {
    async fn acquire_request_only(
        &self,
        actions: &[QuotaAction],
        route: Result<QuotaDecision, QuotaBackendError>,
    ) -> Result<QuotaAdmission, QuotaBackendError> {
        Ok(match self.acquire_many(actions, None).await? {
            QuotaDecision::Limited(exceeded) => QuotaAdmission::RequestLimited(exceeded),
            QuotaDecision::Allowed(request_grant) => QuotaAdmission::RequestAllowed {
                request_grant,
                route,
            },
        })
    }

    async fn acquire_owned(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
        owner: Option<&BackgroundOwner>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        MemoryQuota::validate_actions(actions)?;
        if actions.is_empty() {
            return Ok(QuotaDecision::Allowed(None));
        }
        let permit_id = uuid::Uuid::new_v4();
        let row = sqlx::query("SELECT * FROM chalk_router.acquire_quota($1,$2,$3,$4,$5,$6,$7)")
            .bind(sqlx::types::Json(actions))
            .bind(permit_id)
            .bind(LEASE_TTL.as_secs() as i64)
            .bind(RESERVATION_TTL.as_secs() as i64)
            .bind(owner.map(|owner| owner.environment_id.as_str()))
            .bind(owner.map(|owner| owner.event_id))
            .bind(failed_amount.map(|amount| i64::try_from(amount).unwrap_or(i64::MAX)))
            .fetch_one(&self.pool)
            .await
            .map_err(QuotaBackendError::database)?;

        let decision =
            PostgresAdmissionDecision::from_row(&row).map_err(QuotaBackendError::database)?;
        Ok(decision.decision(permit_id, actions, failed_amount, owner))
    }
}

#[async_trait]
impl QuotaBackend for PostgresQuota {
    fn enabled(&self) -> bool {
        true
    }

    fn postgres_pool(&self) -> Option<&sqlx::PgPool> {
        Some(&self.pool)
    }

    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        self.acquire_owned(actions, failed_amount, None).await
    }

    async fn acquire_request_and_route(
        &self,
        request_actions: &[QuotaAction],
        route_actions: &[QuotaAction],
        route_failed_amount: Option<u64>,
    ) -> Result<QuotaAdmission, QuotaBackendError> {
        MemoryQuota::validate_actions(request_actions)?;
        if let Err(error) = MemoryQuota::validate_actions(route_actions) {
            return self.acquire_request_only(request_actions, Err(error)).await;
        }
        if route_actions.is_empty() {
            return self
                .acquire_request_only(request_actions, Ok(QuotaDecision::Allowed(None)))
                .await;
        }
        if request_actions.is_empty() {
            return Ok(QuotaAdmission::RequestAllowed {
                request_grant: None,
                route: self.acquire_many(route_actions, route_failed_amount).await,
            });
        }
        use sqlx::Row;
        let request_id = uuid::Uuid::new_v4();
        let route_id = uuid::Uuid::new_v4();
        let row = sqlx::query(
            "SELECT * FROM chalk_router.acquire_request_and_route_quota($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(sqlx::types::Json(request_actions))
        .bind(sqlx::types::Json(route_actions))
        .bind(request_id)
        .bind(route_id)
        .bind(LEASE_TTL.as_secs() as i64)
        .bind(RESERVATION_TTL.as_secs() as i64)
        .bind(route_failed_amount.map(|amount| i64::try_from(amount).unwrap_or(i64::MAX)))
        .fetch_one(&self.pool)
        .await
        .map_err(QuotaBackendError::database)?;
        let request: sqlx::types::Json<PostgresAdmissionDecision> = row
            .try_get("request_decision")
            .map_err(QuotaBackendError::database)?;
        match request.0.decision(request_id, request_actions, None, None) {
            QuotaDecision::Limited(exceeded) => Ok(QuotaAdmission::RequestLimited(exceeded)),
            QuotaDecision::Allowed(request_grant) => {
                // A route decoding failure must not discard the successfully
                // decoded request grant (which still needs renewal/settlement).
                let route = (|| {
                    if let Some(sqlstate) = row
                        .try_get::<Option<String>, _>("route_error")
                        .map_err(QuotaBackendError::database)?
                    {
                        return Err(QuotaBackendError::unavailable(format!(
                            "route quota acquisition failed (SQLSTATE {sqlstate})"
                        )));
                    }
                    let route: sqlx::types::Json<PostgresAdmissionDecision> = row
                        .try_get("route_decision")
                        .map_err(QuotaBackendError::database)?;
                    Ok(route
                        .0
                        .decision(route_id, route_actions, route_failed_amount, None))
                })();
                Ok(QuotaAdmission::RequestAllowed {
                    request_grant,
                    route,
                })
            }
        }
    }

    async fn acquire_background(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
        owner: &BackgroundOwner,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        self.acquire_owned(actions, failed_amount, Some(owner))
            .await
    }

    async fn finish_background(
        &self,
        owner: &BackgroundOwner,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(QuotaBackendError::database)?;
        self.finish_background_in_transaction(&mut transaction, owner, outcome)
            .await?;
        transaction
            .commit()
            .await
            .map_err(QuotaBackendError::database)?;
        Ok(())
    }

    async fn finish_background_in_transaction(
        &self,
        connection: &mut sqlx::PgConnection,
        owner: &BackgroundOwner,
        outcome: QuotaOutcome,
    ) -> Result<bool, QuotaBackendError> {
        let actual = match outcome {
            QuotaOutcome::Actual(amount) => Some(i64::try_from(amount).unwrap_or(i64::MAX)),
            QuotaOutcome::Unknown | QuotaOutcome::Failed => None,
        };
        sqlx::query("SELECT chalk_router.finish_background_response_quota($1,$2,$3,$4)")
            .bind(&owner.environment_id)
            .bind(owner.event_id)
            .bind(actual)
            .bind(outcome == QuotaOutcome::Failed)
            .execute(connection)
            .await
            .map_err(QuotaBackendError::database)?;
        Ok(true)
    }

    async fn finish(
        &self,
        grant: &QuotaGrant,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        let actual = match outcome {
            QuotaOutcome::Actual(amount) => Some(i64::try_from(amount).unwrap_or(i64::MAX)),
            QuotaOutcome::Unknown => None,
            QuotaOutcome::Failed => grant
                .failed_amount
                .map(|amount| i64::try_from(amount).unwrap_or(i64::MAX)),
        };
        if let Some(owner) = &grant.background_owner {
            // Background ownership and settlement must still commit together.
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(QuotaBackendError::database)?;
            sqlx::query("SELECT event_id FROM chalk_router.background_response_jobs WHERE environment_id=$1 AND event_id=$2 FOR UPDATE")
                .bind(&owner.environment_id).bind(owner.event_id).fetch_optional(&mut *transaction).await.map_err(QuotaBackendError::database)?;
            sqlx::query("SELECT chalk_router.finish_quota($1, $2, $3, $4)")
                .bind(sqlx::types::Json(&grant.actions))
                .bind(grant.permit_id)
                .bind(actual)
                .bind(outcome == QuotaOutcome::Failed)
                .execute(&mut *transaction)
                .await
                .map_err(QuotaBackendError::database)?;
            return transaction
                .commit()
                .await
                .map_err(QuotaBackendError::database);
        }
        // The function call is already atomic in an autocommit transaction.
        sqlx::query("SELECT chalk_router.finish_quota($1, $2, $3, $4)")
            .bind(sqlx::types::Json(&grant.actions))
            .bind(grant.permit_id)
            .bind(actual)
            .bind(outcome == QuotaOutcome::Failed)
            .execute(&self.pool)
            .await
            .map_err(QuotaBackendError::database)?;
        Ok(())
    }

    async fn renew(&self, permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        sqlx::query("SELECT chalk_router.renew_quota($1, $2, $3)")
            .bind(permit_id)
            .bind(LEASE_TTL.as_secs() as i64)
            .bind(RESERVATION_TTL.as_secs() as i64)
            .execute(&self.pool)
            .await
            .map_err(QuotaBackendError::database)?;
        Ok(())
    }

    async fn cleanup(&self, batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        use sqlx::Row;
        let row = sqlx::query("SELECT * FROM chalk_router.cleanup_quota_runtime($1)")
            .bind(i64::try_from(batch_size).unwrap_or(i64::MAX))
            .fetch_one(&self.pool)
            .await
            .map_err(QuotaBackendError::database)?;
        Ok((
            row.get::<i64, _>("reservations_deleted").max(0) as u64,
            row.get::<i64, _>("leases_deleted").max(0) as u64,
        ))
    }

    async fn fixed_window_usage(
        &self,
        queries: &[FixedWindowUsageQuery],
    ) -> Result<Option<HashMap<String, u64>>, QuotaBackendError> {
        use sqlx::Row;
        if queries.is_empty() {
            return Ok(Some(HashMap::new()));
        }
        let policy_ids: Vec<_> = queries
            .iter()
            .map(|query| query.policy_id.clone())
            .collect();
        let window_starts: Vec<_> = queries.iter().map(|query| query.window_start).collect();
        let rows = sqlx::query(
            "SELECT requested.policy_id, COALESCE(state.used_amount, 0) AS used_amount \
             FROM UNNEST($1::TEXT[], $2::TIMESTAMPTZ[]) AS requested(policy_id, window_start) \
             LEFT JOIN chalk_router.quota_states AS state \
               ON state.policy_id = requested.policy_id \
              AND state.window_start = requested.window_start",
        )
        .bind(policy_ids)
        .bind(window_starts)
        .fetch_all(&self.pool)
        .await
        .map_err(QuotaBackendError::database)?;
        Ok(Some(
            rows.into_iter()
                .map(|row| {
                    (
                        row.get::<String, _>("policy_id"),
                        row.get::<i64, _>("used_amount").max(0) as u64,
                    )
                })
                .collect(),
        ))
    }
}

/// Reserved Redis selection. It remains disabled until an atomic Lua
/// implementation passes the same conformance suite.
pub struct UnimplementedRedisQuota;

#[async_trait]
impl QuotaBackend for UnimplementedRedisQuota {
    fn enabled(&self) -> bool {
        false
    }

    async fn acquire_many(
        &self,
        _actions: &[QuotaAction],
        _failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        Err(QuotaBackendError::invalid(
            "ROUTER_QUOTA_BACKEND=redis is reserved but not implemented",
        ))
    }

    async fn finish(
        &self,
        _grant: &QuotaGrant,
        _outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        Ok(())
    }

    async fn renew(&self, _permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        Ok(())
    }

    async fn cleanup(&self, _batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        Ok((0, 0))
    }

    async fn fixed_window_usage(
        &self,
        _queries: &[FixedWindowUsageQuery],
    ) -> Result<Option<HashMap<String, u64>>, QuotaBackendError> {
        Ok(None)
    }
}

pub fn from_kind(kind: QuotaBackendKind, pool: Option<sqlx::PgPool>) -> Arc<dyn QuotaBackend> {
    match kind {
        QuotaBackendKind::Disabled => Arc::new(DisabledQuota),
        QuotaBackendKind::Memory => Arc::new(MemoryQuota::new()),
        QuotaBackendKind::Postgres => match pool {
            Some(pool) => Arc::new(PostgresQuota::new(pool)),
            None => {
                tracing::warn!(
                    "ROUTER_QUOTA_BACKEND=postgres but no database is configured; quotas disabled"
                );
                Arc::new(DisabledQuota)
            }
        },
        QuotaBackendKind::Redis => {
            tracing::warn!(
                "ROUTER_QUOTA_BACKEND=redis is reserved but not implemented; quotas disabled"
            );
            Arc::new(UnimplementedRedisQuota)
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::{
        BackgroundOwner, FixedWindowUsageQuery, MemoryPolicyState, MemoryQuota, QuotaAction,
        QuotaAlgorithm, QuotaBackend, QuotaDecision, QuotaMeter, QuotaOutcome, QuotaSource,
        ReservedState, TokenReservation, ACQUIRE_CLEANUP_BATCH, RESERVATION_TTL,
        SOFT_CAP_RESERVE_TOKENS,
    };
    use std::time::{Duration, Instant};

    fn concurrency_action(policy_id: &str, limit: u64) -> QuotaAction {
        QuotaAction {
            policy_id: policy_id.to_string(),
            environment_id: "test".to_string(),
            source: QuotaSource::RateLimit,
            algorithm: QuotaAlgorithm::Concurrency,
            meter: QuotaMeter::ConcurrentRequests,
            limit_value: limit,
            burst: None,
            amount: 1,
            window_start: None,
            window_end: None,
        }
    }

    #[tokio::test]
    async fn background_ownership_survives_drop_and_both_expiry_paths() {
        let backend = std::sync::Arc::new(MemoryQuota::new());
        let owner = BackgroundOwner {
            environment_id: "test".into(),
            event_id: uuid::Uuid::new_v4(),
        };
        let action = concurrency_action("background-concurrency", 1);
        let budget = QuotaAction {
            policy_id: "background-budget".into(),
            environment_id: "test".into(),
            source: QuotaSource::BudgetLimit,
            algorithm: QuotaAlgorithm::FixedWindow,
            meter: QuotaMeter::Tokens,
            limit_value: 100,
            burst: None,
            amount: 80,
            window_start: Some(Utc::now()),
            window_end: Some(Utc::now() + chrono::Duration::days(1)),
        };
        let QuotaDecision::Allowed(Some(grant)) = backend
            .acquire_background(&[action.clone(), budget.clone()], None, &owner)
            .await
            .unwrap()
        else {
            panic!("background grant expected");
        };
        drop(super::QuotaPermit::new(backend.clone(), grant));
        {
            let mut runtime = backend.runtime.lock().unwrap();
            for lease in runtime.leases.values_mut() {
                lease.expires_at = Instant::now() - Duration::from_secs(1);
            }
            for reservation in runtime.reservations.values_mut() {
                reservation.expires_at = Instant::now() - Duration::from_secs(1);
            }
        }
        assert_eq!(backend.cleanup(100).await.unwrap(), (0, 0));
        assert!(matches!(
            backend
                .acquire_many(std::slice::from_ref(&action), None)
                .await
                .unwrap(),
            QuotaDecision::Limited(_)
        ));
        backend
            .finish_background(&owner, QuotaOutcome::Actual(5))
            .await
            .unwrap();
        backend
            .finish_background(&owner, QuotaOutcome::Actual(99))
            .await
            .unwrap();
        let usage = backend
            .fixed_window_usage(&[FixedWindowUsageQuery {
                policy_id: budget.policy_id,
                window_start: budget.window_start.unwrap(),
            }])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage["background-budget"], 5);
        assert!(matches!(
            backend.acquire_many(&[action], None).await.unwrap(),
            QuotaDecision::Allowed(_)
        ));
        let runtime = backend.runtime.lock().unwrap();
        assert!(runtime
            .reservations
            .values()
            .all(|reservation| reservation.background_owner.is_none()));
        assert!(runtime
            .leases
            .values()
            .all(|lease| lease.background_owner.is_none()));
    }

    #[tokio::test]
    async fn background_unknown_usage_keeps_tokens_but_releases_concurrency() {
        let backend = MemoryQuota::new();
        let owner = BackgroundOwner {
            environment_id: "test".into(),
            event_id: uuid::Uuid::new_v4(),
        };
        let concurrency = concurrency_action("unknown-concurrency", 1);
        let tokens = QuotaAction {
            algorithm: QuotaAlgorithm::TokenBucket,
            meter: QuotaMeter::Tokens,
            policy_id: "unknown-tokens".into(),
            limit_value: 100,
            burst: Some(100),
            amount: 90,
            ..concurrency.clone()
        };
        backend
            .acquire_background(&[tokens.clone(), concurrency.clone()], None, &owner)
            .await
            .unwrap();
        backend
            .finish_background(&owner, QuotaOutcome::Unknown)
            .await
            .unwrap();
        let runtime = backend.runtime.lock().unwrap();
        assert_eq!(runtime.states["unknown-concurrency"].active_count, 0);
        assert!(runtime.states["unknown-tokens"].balance.unwrap() < 11.0);
        assert!(runtime.reservations.is_empty());
    }

    #[tokio::test]
    async fn background_owner_cannot_cross_environment_boundaries() {
        let backend = MemoryQuota::new();
        let owner = BackgroundOwner {
            environment_id: "other".into(),
            event_id: uuid::Uuid::new_v4(),
        };
        let action = concurrency_action("environment-boundary", 1);
        assert!(backend
            .acquire_background(std::slice::from_ref(&action), None, &owner)
            .await
            .is_err());
        assert!(matches!(
            backend.acquire_many(&[action], None).await.unwrap(),
            QuotaDecision::Allowed(_)
        ));
    }

    #[test]
    fn conservative_max_propagates_an_unbounded_candidate_in_any_position() {
        assert_eq!(
            TokenReservation::conservative_max([Some(64), Some(1_024), Some(128)]),
            TokenReservation::Known(1_024)
        );
        assert_eq!(
            TokenReservation::conservative_max([None, Some(1_024)]),
            TokenReservation::Unknown
        );
        assert_eq!(
            TokenReservation::conservative_max([Some(1_024), None, Some(2_048)]),
            TokenReservation::Unknown
        );
    }

    #[test]
    fn soft_cap_reservations_add_a_reserve_clamped_to_capacity() {
        let soft = TokenReservation::Known(40).with_soft_cap(true);
        assert_eq!(soft, TokenReservation::SoftCap(40));
        assert_eq!(
            soft.amount_for(1_000_000),
            Some(40 + SOFT_CAP_RESERVE_TOKENS)
        );
        assert_eq!(soft.amount_for(512), Some(512));
        // A visible bound above capacity is held whole, so the policy rejects
        // it exactly as it would a bounded request.
        assert_eq!(TokenReservation::SoftCap(600).amount_for(512), Some(600));
        assert_eq!(TokenReservation::Known(40).amount_for(10), Some(40));
        assert_eq!(
            TokenReservation::Unknown.with_soft_cap(true),
            TokenReservation::Unknown
        );
        assert_eq!(TokenReservation::Unknown.amount_for(512), None);
    }

    #[tokio::test]
    async fn renew_extends_foreground_token_reservations() {
        let backend = MemoryQuota::new();
        let start = Utc::now() - chrono::Duration::hours(1);
        let action = QuotaAction {
            policy_id: "daily-policy".into(),
            environment_id: "test".into(),
            source: QuotaSource::BudgetLimit,
            algorithm: QuotaAlgorithm::FixedWindow,
            meter: QuotaMeter::Tokens,
            limit_value: 100,
            burst: None,
            amount: 25,
            window_start: Some(start),
            window_end: Some(start + chrono::Duration::days(1)),
        };
        let grant = match backend.acquire_many(&[action], None).await.unwrap() {
            QuotaDecision::Allowed(Some(grant)) => grant,
            other => panic!("expected an allowed grant, got {other:?}"),
        };
        let key = ("daily-policy".to_string(), grant.permit_id);
        backend
            .runtime
            .lock()
            .expect("quota runtime lock")
            .reservations
            .get_mut(&key)
            .expect("reservation exists")
            .expires_at = Instant::now();

        backend.renew(grant.permit_id).await.unwrap();

        let runtime = backend.runtime.lock().expect("quota runtime lock");
        let expiry = runtime.reservations.get(&key).expect("reservation kept");
        assert!(
            expiry.expires_at.saturating_duration_since(Instant::now())
                > RESERVATION_TTL - Duration::from_secs(60),
            "a running request must keep the reservation settlement needs"
        );
    }

    #[tokio::test]
    async fn fixed_window_usage_reports_current_admission_state() {
        let backend = MemoryQuota::new();
        let start = Utc.with_ymd_and_hms(2026, 9, 3, 0, 0, 0).unwrap();
        let action = QuotaAction {
            policy_id: "daily-policy".into(),
            environment_id: "test".into(),
            source: QuotaSource::BudgetLimit,
            algorithm: QuotaAlgorithm::FixedWindow,
            meter: QuotaMeter::Tokens,
            limit_value: 100,
            burst: None,
            amount: 25,
            window_start: Some(start),
            window_end: Some(start + chrono::Duration::days(1)),
        };
        assert!(matches!(
            backend.acquire_many(&[action], None).await.unwrap(),
            QuotaDecision::Allowed(_)
        ));

        let usage = backend
            .fixed_window_usage(&[FixedWindowUsageQuery {
                policy_id: "daily-policy".into(),
                window_start: start,
            }])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage.get("daily-policy"), Some(&25));
    }

    #[tokio::test]
    async fn renew_extends_every_lease_a_permit_holds_across_policies() {
        let backend = MemoryQuota::new();
        let grant = match backend
            .acquire_many(
                &[
                    concurrency_action("global-policy", 5),
                    concurrency_action("route-policy", 5),
                ],
                None,
            )
            .await
            .unwrap()
        {
            QuotaDecision::Allowed(Some(grant)) => grant,
            other => panic!("expected an allowed grant with leases, got {other:?}"),
        };

        // Backdate both of this permit's leases so a renewal is observable.
        {
            let mut runtime = backend.runtime.lock().expect("quota runtime lock");
            for policy_id in ["global-policy", "route-policy"] {
                runtime
                    .leases
                    .get_mut(&(policy_id.to_string(), grant.permit_id))
                    .expect("lease exists")
                    .expires_at = Instant::now();
            }
        }

        backend.renew(grant.permit_id).await.unwrap();

        let runtime = backend.runtime.lock().expect("quota runtime lock");
        let now = Instant::now();
        for policy_id in ["global-policy", "route-policy"] {
            let expiry = runtime
                .leases
                .get(&(policy_id.to_string(), grant.permit_id))
                .expect("renew must not drop a permit's lease");
            assert!(
                expiry.expires_at.saturating_duration_since(now) > Duration::from_secs(60),
                "renew must extend every policy's lease for this permit, not just one"
            );
        }
    }

    #[tokio::test]
    async fn renew_is_a_no_op_for_an_unknown_permit() {
        let backend = MemoryQuota::new();
        let unknown = uuid::Uuid::new_v4();

        backend
            .renew(unknown)
            .await
            .expect("renewing an unknown permit must not error");

        let runtime = backend.runtime.lock().expect("quota runtime lock");
        assert!(
            !runtime.leases_by_permit.contains_key(&unknown),
            "renewing an unknown permit must not fabricate a lease entry"
        );
    }

    #[tokio::test]
    async fn acquire_many_fully_expires_its_own_policy_despite_a_large_backlog_elsewhere() {
        let backend = MemoryQuota::new();
        let noisy_policy = "noisy-policy";
        let backlog = ACQUIRE_CLEANUP_BATCH * 3;
        let expired = Instant::now() - Duration::from_secs(1);

        {
            let mut runtime = backend.runtime.lock().expect("quota runtime lock");

            // A large, fully-expired backlog under an unrelated policy, far in
            // excess of the bounded incidental cleanup batch, simulating many
            // other policies' stale state accumulating in a long-running
            // process.
            runtime.states.insert(
                noisy_policy.to_string(),
                MemoryPolicyState {
                    balance: None,
                    updated_at: None,
                    window_start: None,
                    used_amount: 0,
                    active_count: backlog as u64,
                },
            );
            for _ in 0..backlog {
                MemoryQuota::insert_lease_locked(
                    &mut runtime,
                    noisy_policy,
                    uuid::Uuid::new_v4(),
                    expired,
                    None,
                );
            }

            // A stale, already-expired lease under the policy this
            // acquisition is about to evaluate, sitting right at its
            // concurrency limit.
            runtime.states.insert(
                "hot-policy".to_string(),
                MemoryPolicyState {
                    balance: None,
                    updated_at: None,
                    window_start: None,
                    used_amount: 0,
                    active_count: 1,
                },
            );
            MemoryQuota::insert_lease_locked(
                &mut runtime,
                "hot-policy",
                uuid::Uuid::new_v4(),
                expired,
                None,
            );
        }

        // Despite the huge unrelated backlog, this policy's own expired
        // lease must be reaped in full before admission is evaluated, so a
        // stale entry never causes a false rejection.
        let decision = backend
            .acquire_many(&[concurrency_action("hot-policy", 1)], None)
            .await
            .unwrap();
        assert!(
            matches!(decision, QuotaDecision::Allowed(_)),
            "a policy's own expired lease must be fully reclaimed regardless of unrelated backlog size"
        );

        // The unrelated backlog must only be drained in small, bounded
        // batches, not swept in full on every acquisition.
        let runtime = backend.runtime.lock().expect("quota runtime lock");
        let remaining = runtime
            .leases_by_policy
            .get(noisy_policy)
            .map_or(0, |permits| permits.len());
        assert!(
            remaining >= backlog - ACQUIRE_CLEANUP_BATCH,
            "one acquisition must not sweep every other policy's full backlog (remaining={remaining})"
        );
    }

    #[tokio::test]
    async fn reservations_by_policy_index_stays_consistent_across_acquire_finish_and_expire() {
        let backend = MemoryQuota::new();
        let start = Utc.with_ymd_and_hms(2026, 9, 3, 0, 0, 0).unwrap();
        let action = QuotaAction {
            policy_id: "daily-policy".into(),
            environment_id: "test".into(),
            source: QuotaSource::BudgetLimit,
            algorithm: QuotaAlgorithm::FixedWindow,
            meter: QuotaMeter::Tokens,
            limit_value: 100,
            burst: None,
            amount: 25,
            window_start: Some(start),
            window_end: Some(start + chrono::Duration::days(1)),
        };

        // Acquire: the index must record this policy's reservation permit.
        let grant = match backend.acquire_many(&[action.clone()], None).await.unwrap() {
            QuotaDecision::Allowed(Some(grant)) => grant,
            other => panic!("expected an allowed grant with a reservation, got {other:?}"),
        };
        {
            let runtime = backend.runtime.lock().expect("quota runtime lock");
            let permits = runtime
                .reservations_by_policy
                .get("daily-policy")
                .expect("index must record the new reservation's policy");
            assert!(permits.contains(&grant.permit_id));
            assert!(runtime
                .reservations
                .contains_key(&("daily-policy".to_string(), grant.permit_id)));
        }

        // Finish/settle: the index must drop the permit once the reservation is
        // reconciled, exactly like `reservations` itself.
        backend
            .finish(&grant, QuotaOutcome::Actual(25))
            .await
            .unwrap();
        {
            let runtime = backend.runtime.lock().expect("quota runtime lock");
            assert!(
                !runtime
                    .reservations_by_policy
                    .get("daily-policy")
                    .is_some_and(|permits| permits.contains(&grant.permit_id)),
                "finish must remove the settled reservation from the index"
            );
        }

        // Expire: a stale reservation for a policy about to be evaluated must be
        // reclaimed via the index (not a full-map scan) and dropped from it.
        let stale_permit = uuid::Uuid::new_v4();
        {
            let mut runtime = backend.runtime.lock().expect("quota runtime lock");
            MemoryQuota::insert_reservation_locked(
                &mut runtime,
                "daily-policy",
                stale_permit,
                ReservedState {
                    amount: 25,
                    window_start: Some(start),
                    expires_at: Instant::now() - Duration::from_secs(1),
                    background_owner: None,
                    settlement: None,
                },
            );
        }
        backend.acquire_many(&[action], None).await.unwrap();
        {
            let runtime = backend.runtime.lock().expect("quota runtime lock");
            assert!(
                !runtime
                    .reservations_by_policy
                    .get("daily-policy")
                    .is_some_and(|permits| permits.contains(&stale_permit)),
                "expire_for_policies_locked must reclaim the stale reservation via the index"
            );
            assert!(
                !runtime
                    .reservations
                    .contains_key(&("daily-policy".to_string(), stale_permit)),
                "the stale reservation itself must be gone from `reservations`"
            );
        }
    }
}
