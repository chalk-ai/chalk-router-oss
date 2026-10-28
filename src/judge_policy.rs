//! Judge routing policy — the admin-editable half of model-as-judge config.
//!
//! Mirrors [`crate::fallback`]: one env-scoped policy, a store trait with an
//! in-memory and a Postgres implementation, persisted as a single JSONB row.
//! The store trait, in-memory store, and short-TTL cache are shared with
//! [`crate::fallback`] via [`crate::revisioned_policy`] — see that module's
//! docs for the consistency argument. This file keeps only the wire format
//! and the Postgres store (its table and JSON shape are judge-specific);
//! everything else below is a type alias so existing callers (`main.rs`,
//! `lib.rs`, the gRPC/service layers, tests) are unaffected.
//!
//! Ownership split: everything an operator edits in the UI lives here —
//! `enabled`, `judge_model`, the per-model downgrade `ladders`, band
//! boundaries, the shadow sampling rate, and the **global defaults**
//! (`default_opt_in`, `default_floor`) that per-key settings inherit. Pure
//! *deployment wiring* (backend kind, sidecar URL, timeout, concurrency,
//! global kill switch) lives in [`crate::config`]/env and is intentionally
//! not duplicated here.
//!
//! Model identity: `judge_model`, ladder keys, ladder `targets`, and
//! `default_floor` are all **public model ids** — the same `chalk/*` alias
//! namespace the fallback policy and API-key model allowlists use. They are
//! resolved through the routing snapshot at decision time (never stored as
//! upstream ids), so a route edit in the UI takes effect without touching the
//! judge policy.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::judge::canonicalize_model;
use crate::revisioned_policy::{
    RevisionedPolicy, RevisionedPolicyStore, RevisionedPolicyStoreError,
};

/// The downgrade ladder + banding for one requested (public) model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JudgeLadder {
    /// Downgrade targets in ceiling→floor order (cheaper as the list
    /// progresses), as public model ids. Excludes the requested model itself,
    /// which is always the top rung. Filtered per-key at decision time by the
    /// key's `ApiKeyPermission::allows` and truncated at the effective floor.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Sorted band cut points in `(0, 1)` separating rungs; empty = equal-width
    /// bands. See [`crate::judge::select_rung`].
    #[serde(default)]
    pub boundaries: Vec<f32>,
}

/// Env-scoped judge policy. `Default` is the safe off state (disabled).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JudgePolicy {
    /// Monotonically increasing, store-managed version used to protect
    /// read-modify-write admin updates. It is persisted in the JSON blob but
    /// deliberately not exposed in the admin wire format.
    #[serde(default)]
    pub revision: u64,
    /// Master switch. When false the judge never runs (all traffic short-circuits).
    #[serde(default)]
    pub enabled: bool,
    /// Shadow mode: score + log, but never rewrite the served model. The safe
    /// phase-0 calibration state.
    #[serde(default)]
    pub shadow: bool,
    /// Fraction of traffic to shadow-judge, `0.0..=1.0`. Shadow is
    /// fire-and-forget so it may legally sample all traffic.
    #[serde(default)]
    pub shadow_sample_rate: f32,
    /// The judge model to use, as a public model id (e.g. `chalk/haiku`)
    /// resolved through the routing snapshot. Read at call time so UI edits
    /// take effect without a restart.
    #[serde(default)]
    pub judge_model: String,
    /// Per-requested-model ladders, keyed by the canonicalized public model id.
    #[serde(default)]
    pub ladders: HashMap<String, JudgeLadder>,
    /// Global default opt-in: when true, every key is downgradable unless its
    /// permission carries `downgradable: false`. When false (the default),
    /// only keys with `downgradable: true` participate. Lets an operator turn
    /// the feature on fleet-wide with one toggle while any key can still opt
    /// out or tighten its floor.
    #[serde(default)]
    pub default_opt_in: bool,
    /// Global default floor (public model id): no key is ever downgraded below
    /// it unless the key's own `model_floor` overrides. `None` = no default
    /// floor (a ladder's last rung is the floor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_floor: Option<String>,
}

/// Whether `boundaries` is a usable cut-point list for a ladder of
/// `rung_count` rungs: exactly `rung_count - 1` values, each finite, strictly
/// increasing, and inside `(0, 1)`. An empty list is always usable (it means
/// equal-width bands). Anything else mis-bands `select_rung` — e.g. a single
/// boundary on a three-rung ladder sends difficulty 1.0 to the *middle* rung —
/// so it is rejected at write time and ignored (equal-width fallback) at read
/// time as defense in depth.
pub fn boundaries_usable(boundaries: &[f32], rung_count: usize) -> bool {
    if boundaries.is_empty() {
        return true;
    }
    boundaries.len() == rung_count.saturating_sub(1)
        && boundaries
            .iter()
            .all(|b| b.is_finite() && *b > 0.0 && *b < 1.0)
        && boundaries.windows(2).all(|w| w[0] < w[1])
}

impl JudgeLadder {
    /// The number of rungs of the full ladder this config describes for
    /// `requested_model`: the requested model itself plus its deduped targets
    /// (a target equal to the requested model is ignored). Must match the
    /// normalization in `judge::route`, which is what the boundaries band over.
    pub fn rung_count(&self, requested_model: &str) -> usize {
        let req_canon = canonicalize_model(requested_model);
        let mut seen = vec![req_canon];
        for target in &self.targets {
            let canon = canonicalize_model(target);
            if !seen.contains(&canon) {
                seen.push(canon);
            }
        }
        seen.len()
    }
}

impl JudgePolicy {
    /// Look up the ladder configured for a requested model (canonicalized).
    pub fn ladder_for(&self, requested_model: &str) -> Option<&JudgeLadder> {
        self.ladders.get(&canonicalize_model(requested_model))
    }

    /// Validate admin input before persisting. Returns the first problem as a
    /// human-readable message; the service layer maps it to
    /// invalid-argument on both wire formats.
    pub fn validate(&self) -> Result<(), String> {
        for (model, ladder) in &self.ladders {
            if !boundaries_usable(&ladder.boundaries, ladder.rung_count(model)) {
                return Err(format!(
                    "ladder '{model}': boundaries must be exactly one fewer than the \
                     rungs (requested model + {} deduped target(s)), finite, strictly \
                     increasing, and inside (0, 1); got {:?}",
                    ladder.rung_count(model) - 1,
                    ladder.boundaries
                ));
            }
        }
        if !self.shadow_sample_rate.is_finite() || !(0.0..=1.0).contains(&self.shadow_sample_rate) {
            return Err(format!(
                "shadow_sample_rate must be within 0.0..=1.0; got {}",
                self.shadow_sample_rate
            ));
        }
        Ok(())
    }

    /// Whether `key_downgradable` (the key's tri-state override) resolves to
    /// opted-in under this policy's global default.
    pub fn key_opted_in(&self, key_downgradable: Option<bool>) -> bool {
        key_downgradable.unwrap_or(self.default_opt_in)
    }

    /// The effective floor for a key: its own `model_floor` when set, else the
    /// policy's `default_floor`.
    pub fn effective_floor<'a>(&'a self, key_floor: Option<&'a str>) -> Option<&'a str> {
        key_floor.or(self.default_floor.as_deref())
    }
}

impl RevisionedPolicy for JudgePolicy {
    const CACHE_TAG: &'static str = "judge_policy";
    const DISPLAY_NAME: &'static str = "judge policy";

    fn revision(&self) -> u64 {
        self.revision
    }

    fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

// ---------------------------------------------------------------------------
// Store trait, in-memory store, and cache: shared generic implementation
// ---------------------------------------------------------------------------

/// Alias so existing callers keep compiling unchanged; see
/// [`crate::revisioned_policy::RevisionedPolicyStoreError`].
pub type JudgePolicyStoreError = RevisionedPolicyStoreError;

/// Alias for the shared in-memory store; see
/// [`crate::revisioned_policy::InMemoryRevisionedStore`].
pub type InMemoryJudgePolicyStore = crate::revisioned_policy::InMemoryRevisionedStore<JudgePolicy>;

/// Alias for the shared short-TTL cache; see
/// [`crate::revisioned_policy::CachedRevisionedStore`].
pub type CachedJudgePolicyStore = crate::revisioned_policy::CachedRevisionedStore<JudgePolicy>;

/// Marker for `Arc<dyn JudgePolicyStore>` trait objects. The real trait,
/// [`RevisionedPolicyStore<JudgePolicy>`], carries a generic parameter and so
/// cannot be spelled bare after `dyn`; this marker is blanket-implemented for
/// every type that implements the generic trait, so existing call sites
/// (service/gRPC layers constructing `Arc<dyn JudgePolicyStore>`) need no
/// changes.
pub trait JudgePolicyStore: RevisionedPolicyStore<JudgePolicy> {}
impl<T: RevisionedPolicyStore<JudgePolicy>> JudgePolicyStore for T {}

// ---------------------------------------------------------------------------
// PostgreSQL implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PostgresJudgePolicyStore {
    pool: sqlx::PgPool,
    /// Tenancy scope (see `PostgresApiKeyStore`); one policy row per environment.
    environment_id: String,
}

impl PostgresJudgePolicyStore {
    pub fn new(pool: sqlx::PgPool, environment_id: String) -> Self {
        Self {
            pool,
            environment_id,
        }
    }
}

#[async_trait]
impl RevisionedPolicyStore<JudgePolicy> for PostgresJudgePolicyStore {
    async fn get(&self) -> Result<JudgePolicy, JudgePolicyStoreError> {
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT policy FROM chalk_router.judge_policy WHERE environment_id = $1",
        )
        .bind(&self.environment_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to read judge policy");
            JudgePolicyStoreError::other(format!("failed to read judge policy: {e}"))
        })?;

        match row {
            Some((json,)) => serde_json::from_value(json).map_err(|e| {
                tracing::error!(error = %e, "Failed to decode judge policy");
                JudgePolicyStoreError::other(format!("failed to decode judge policy: {e}"))
            }),
            None => Ok(JudgePolicy::default()),
        }
    }

    async fn set(&self, policy: JudgePolicy) -> Result<(), JudgePolicyStoreError> {
        let expected = self.get().await?;
        self.set_cas(&expected, policy).await
    }

    async fn set_cas(
        &self,
        expected: &JudgePolicy,
        mut policy: JudgePolicy,
    ) -> Result<(), JudgePolicyStoreError> {
        policy.revision = expected.revision.saturating_add(1);
        let json = serde_json::to_value(&policy).map_err(|e| {
            JudgePolicyStoreError::other(format!("failed to encode judge policy: {e}"))
        })?;
        let result = sqlx::query(
            r#"
            INSERT INTO chalk_router.judge_policy (environment_id, policy) VALUES ($1, $2)
            ON CONFLICT (environment_id) DO UPDATE SET policy = EXCLUDED.policy
            WHERE COALESCE((chalk_router.judge_policy.policy->>'revision')::bigint, 0) = $3
            "#,
        )
        .bind(&self.environment_id)
        .bind(&json)
        .bind(expected.revision as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to persist judge policy");
            JudgePolicyStoreError::other(format!("failed to persist judge policy: {e}"))
        })?;
        if result.rows_affected() == 0 {
            return Err(JudgePolicyStoreError::Conflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::sync::RwLock;

    struct CountingStore {
        gets: AtomicUsize,
        policy: RwLock<JudgePolicy>,
    }

    #[async_trait]
    impl RevisionedPolicyStore<JudgePolicy> for CountingStore {
        async fn get(&self) -> Result<JudgePolicy, JudgePolicyStoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Ok(self.policy.read().await.clone())
        }
        async fn set(&self, policy: JudgePolicy) -> Result<(), JudgePolicyStoreError> {
            *self.policy.write().await = policy;
            Ok(())
        }
        async fn set_cas(
            &self,
            expected: &JudgePolicy,
            mut policy: JudgePolicy,
        ) -> Result<(), JudgePolicyStoreError> {
            let mut current = self.policy.write().await;
            if current.revision != expected.revision {
                return Err(JudgePolicyStoreError::Conflict);
            }
            policy.revision = expected.revision.saturating_add(1);
            *current = policy;
            Ok(())
        }
    }

    #[tokio::test]
    async fn cached_store_reads_inner_once_per_ttl_window() {
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(JudgePolicy::default()),
        });
        let cached = CachedJudgePolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));
        cached.get().await.unwrap();
        cached.get().await.unwrap();
        cached.get().await.unwrap();
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cached_store_set_writes_through_and_serves_immediately() {
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(JudgePolicy::default()),
        });
        let cached = CachedJudgePolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));
        assert!(!cached.get().await.unwrap().enabled);
        let mut updated = JudgePolicy::default();
        updated.enabled = true;
        cached.set(updated).await.expect("set persists");
        // Served from the refreshed cache — no extra inner read.
        assert!(cached.get().await.unwrap().enabled);
        assert!(inner.policy.read().await.enabled, "write-through persisted");
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn boundaries_validation() {
        // Empty = equal-width, always fine.
        assert!(boundaries_usable(&[], 3));
        // Exact count, increasing, in (0,1).
        assert!(boundaries_usable(&[0.3, 0.7], 3));
        assert!(boundaries_usable(&[0.5], 2));
        // Wrong count for the rungs (the review's case: 1 boundary, 3 rungs).
        assert!(!boundaries_usable(&[0.5], 3));
        assert!(!boundaries_usable(&[0.2, 0.4, 0.6], 3));
        // Not strictly increasing / out of range / non-finite.
        assert!(!boundaries_usable(&[0.7, 0.3], 3));
        assert!(!boundaries_usable(&[0.3, 0.3], 3));
        assert!(!boundaries_usable(&[0.0, 0.5], 3));
        assert!(!boundaries_usable(&[0.5, 1.0], 3));
        assert!(!boundaries_usable(&[f32::NAN], 2));
    }

    #[test]
    fn rung_count_dedupes_and_ignores_requested() {
        let ladder = JudgeLadder {
            targets: vec![
                "chalk/mid".to_string(),
                "CHALK/MID".to_string(), // dupe of mid
                "chalk/big".to_string(), // the requested model itself
                "chalk/cheap".to_string(),
            ],
            boundaries: vec![],
        };
        // requested + mid + cheap
        assert_eq!(ladder.rung_count("chalk/big"), 3);
    }

    #[test]
    fn policy_validate_rejects_bad_boundaries_and_sample_rate() {
        let mut policy = JudgePolicy::default();
        policy.ladders.insert(
            "chalk/big".to_string(),
            JudgeLadder {
                targets: vec!["chalk/mid".to_string(), "chalk/cheap".to_string()],
                boundaries: vec![0.5], // 3 rungs need 2 boundaries
            },
        );
        assert!(policy.validate().is_err());

        policy.ladders.get_mut("chalk/big").unwrap().boundaries = vec![0.3, 0.7];
        assert!(policy.validate().is_ok());

        policy.shadow_sample_rate = 1.5;
        assert!(policy.validate().is_err());
    }

    #[tokio::test]
    async fn cached_store_expires_after_ttl() {
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(JudgePolicy::default()),
        });
        let cached =
            CachedJudgePolicyStore::new(inner.clone(), std::time::Duration::from_millis(0));
        cached.get().await.unwrap();
        cached.get().await.unwrap();
        // Zero TTL → every get falls through.
        assert_eq!(inner.gets.load(Ordering::SeqCst), 2);
    }

    /// Inner store whose `get` parks until released, to script refresh/set races.
    struct BlockingStore {
        gets: AtomicUsize,
        release: tokio::sync::Notify,
        policy: RwLock<JudgePolicy>,
    }

    #[async_trait]
    impl RevisionedPolicyStore<JudgePolicy> for BlockingStore {
        async fn get(&self) -> Result<JudgePolicy, JudgePolicyStoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            Ok(self.policy.read().await.clone())
        }
        async fn set(&self, policy: JudgePolicy) -> Result<(), JudgePolicyStoreError> {
            *self.policy.write().await = policy;
            Ok(())
        }
        async fn set_cas(
            &self,
            expected: &JudgePolicy,
            mut policy: JudgePolicy,
        ) -> Result<(), JudgePolicyStoreError> {
            let mut current = self.policy.write().await;
            if current.revision != expected.revision {
                return Err(JudgePolicyStoreError::Conflict);
            }
            policy.revision = expected.revision.saturating_add(1);
            *current = policy;
            Ok(())
        }
    }

    #[tokio::test]
    async fn concurrent_misses_collapse_into_one_inner_read() {
        let inner = Arc::new(BlockingStore {
            gets: AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
            policy: RwLock::new(JudgePolicy::default()),
        });
        let cached = Arc::new(CachedJudgePolicyStore::new(
            inner.clone(),
            std::time::Duration::from_secs(60),
        ));
        let (a, b) = {
            let (c1, c2) = (cached.clone(), cached.clone());
            (
                tokio::spawn(async move { c1.get().await }),
                tokio::spawn(async move { c2.get().await }),
            )
        };
        // Let both tasks reach the miss path, then unblock the single reader.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        inner.release.notify_waiters();
        inner.release.notify_waiters();
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
        assert_eq!(
            inner.gets.load(Ordering::SeqCst),
            1,
            "the refresh mutex must collapse simultaneous misses"
        );
    }

    #[tokio::test]
    async fn set_overlapping_a_stale_refresh_wins() {
        let inner = Arc::new(BlockingStore {
            gets: AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
            policy: RwLock::new(JudgePolicy::default()), // enabled: false
        });
        let cached = Arc::new(CachedJudgePolicyStore::new(
            inner.clone(),
            std::time::Duration::from_secs(60),
        ));
        // A refresh starts (cache empty) and parks inside the inner read,
        // holding the OLD value — and the update mutex.
        let refresher = {
            let c = cached.clone();
            tokio::spawn(async move { c.get().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // A set with the NEW value queues behind the mutex (it must not
        // interleave with the in-flight refresh).
        let setter = {
            let c = cached.clone();
            tokio::spawn(async move {
                let mut updated = JudgePolicy::default();
                updated.enabled = true;
                c.set(updated).await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Release the parked refresh; it installs the stale value, then the
        // serialized set persists + installs the new one.
        inner.release.notify_waiters();
        refresher.await.unwrap().unwrap();
        setter.await.unwrap().expect("set persists");
        assert!(
            cached.get().await.unwrap().enabled,
            "the update mutex must order the set's install after the stale refresh"
        );
        assert!(inner.policy.read().await.enabled, "and persist it");
    }

    /// Inner store whose writes always fail, to assert failure propagation.
    struct FailingWriteStore {
        policy: RwLock<JudgePolicy>,
    }

    #[async_trait]
    impl RevisionedPolicyStore<JudgePolicy> for FailingWriteStore {
        async fn get(&self) -> Result<JudgePolicy, JudgePolicyStoreError> {
            Ok(self.policy.read().await.clone())
        }
        async fn set(&self, _policy: JudgePolicy) -> Result<(), JudgePolicyStoreError> {
            Err(JudgePolicyStoreError::other("db down"))
        }
        async fn set_cas(
            &self,
            _expected: &JudgePolicy,
            _policy: JudgePolicy,
        ) -> Result<(), JudgePolicyStoreError> {
            Err(JudgePolicyStoreError::other("db down"))
        }
    }

    #[tokio::test]
    async fn failed_persist_neither_caches_nor_reports_success() {
        let inner = Arc::new(FailingWriteStore {
            policy: RwLock::new(JudgePolicy::default()), // enabled: false
        });
        let cached = CachedJudgePolicyStore::new(inner, std::time::Duration::from_secs(60));
        let mut updated = JudgePolicy::default();
        updated.enabled = true;
        assert!(
            cached.set(updated).await.is_err(),
            "persistence failure must propagate"
        );
        assert!(
            !cached.get().await.unwrap().enabled,
            "an unpersisted policy must never be served from the cache"
        );
    }

    /// Inner store whose reads fail after the first success, to assert the
    /// cache serves last-known-good on read failure instead of a fabricated
    /// default. Judge-flavored counterpart of `crate::fallback`'s
    /// `failed_read_serves_last_known_good_not_empty` — this policy had no
    /// equivalent coverage before the generic cache was extracted.
    struct FlakyReadStore {
        policy: JudgePolicy,
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RevisionedPolicyStore<JudgePolicy> for FlakyReadStore {
        async fn get(&self) -> Result<JudgePolicy, JudgePolicyStoreError> {
            if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(self.policy.clone())
            } else {
                Err(JudgePolicyStoreError::other("db down"))
            }
        }
        async fn set(&self, _policy: JudgePolicy) -> Result<(), JudgePolicyStoreError> {
            unreachable!("test never writes")
        }
        async fn set_cas(
            &self,
            _expected: &JudgePolicy,
            _policy: JudgePolicy,
        ) -> Result<(), JudgePolicyStoreError> {
            unreachable!("test never writes")
        }
    }

    #[tokio::test]
    async fn failed_read_serves_last_known_good_not_default() {
        let mut policy = JudgePolicy::default();
        policy.enabled = true;
        let inner = Arc::new(FlakyReadStore {
            policy: policy.clone(),
            reads: AtomicUsize::new(0),
        });
        // Zero TTL forces every get through to the inner store, so the second
        // get exercises the inner-error path with a stale value present.
        let cached = CachedJudgePolicyStore::new(inner, std::time::Duration::from_millis(0));
        assert!(
            cached.get().await.expect("first read succeeds").enabled,
            "must observe the first successful read"
        );
        assert!(
            cached
                .get()
                .await
                .expect("read failure must serve stale, not error")
                .enabled,
            "a transient read failure must not shadow the last-known-good policy"
        );
    }

    /// Inner store whose reads always fail, to assert cold-cache propagation
    /// AND fail-fast backoff. Judge-flavored counterpart of `crate::fallback`'s
    /// `failed_cold_read_propagates_error_and_backs_off`: the pre-extraction
    /// `CachedJudgePolicyStore` never cached a cold-read failure, so every
    /// call within the TTL re-hit the (failing) inner store. This test proves
    /// that gap cannot reopen now that both policies share the generic cache.
    struct FailingReadStore {
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RevisionedPolicyStore<JudgePolicy> for FailingReadStore {
        async fn get(&self) -> Result<JudgePolicy, JudgePolicyStoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Err(JudgePolicyStoreError::other("db down"))
        }
        async fn set(&self, _policy: JudgePolicy) -> Result<(), JudgePolicyStoreError> {
            unreachable!("test never writes")
        }
        async fn set_cas(
            &self,
            _expected: &JudgePolicy,
            _policy: JudgePolicy,
        ) -> Result<(), JudgePolicyStoreError> {
            unreachable!("test never writes")
        }
    }

    #[tokio::test]
    async fn failed_cold_read_propagates_error_and_backs_off() {
        let inner = Arc::new(FailingReadStore {
            reads: AtomicUsize::new(0),
        });
        let cached = CachedJudgePolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));
        assert!(
            cached.get().await.is_err(),
            "with nothing ever read, there is no last-known-good to serve"
        );
        // Within the TTL the failure marker fails fast — no repeated inner
        // reads each waiting out their own timeout under the update mutex.
        assert!(cached.get().await.is_err());
        assert!(cached.get().await.is_err());
        assert_eq!(
            inner.reads.load(Ordering::SeqCst),
            1,
            "cold-read failures within the TTL must not re-read the inner store"
        );
    }

    #[tokio::test]
    async fn cas_conflict_clears_the_cache_so_the_next_get_hits_inner() {
        // Regression for: a CAS conflict left the stale `expected` revision
        // sitting in the cache, so every retry on this replica kept
        // conflicting for the rest of the TTL window even after the inner
        // store moved on.
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(JudgePolicy::default()), // revision 0
        });
        let cached = CachedJudgePolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));

        // Populate the cache at revision 0.
        let expected = cached.get().await.unwrap();
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);

        // Simulate another replica (or a direct writer) advancing the inner
        // store to revision 1 without going through this cache.
        let mut bumped = expected.clone();
        bumped.enabled = true;
        inner
            .set_cas(&expected, bumped)
            .await
            .expect("inner write advances the revision");

        // This replica's cached `expected` (revision 0) is now stale: the CAS
        // through the cache must conflict.
        let mut next = expected.clone();
        next.shadow = true;
        let err = cached
            .set_cas(&expected, next)
            .await
            .expect_err("stale cached revision must conflict");
        assert!(err.is_conflict());

        // The conflict must have evicted the cache: the next get is a real
        // inner read — not another retry against the same stale revision —
        // and observes the concurrent writer's update.
        let after = cached.get().await.unwrap();
        assert_eq!(
            inner.gets.load(Ordering::SeqCst),
            2,
            "a conflict must clear the cache so the next get refreshes from inner"
        );
        assert!(after.enabled, "must see the concurrent writer's update");
        assert!(!after.shadow, "the conflicting write must not have applied");
    }

    /// Inner store whose reads always fail, to assert the RMW-safety
    /// invariant: a cold cache must never fabricate `JudgePolicy::default()`
    /// on a read failure (an RMW caller could then write that back and wipe
    /// the real policy).
    struct FailingGetStore;

    #[async_trait]
    impl RevisionedPolicyStore<JudgePolicy> for FailingGetStore {
        async fn get(&self) -> Result<JudgePolicy, JudgePolicyStoreError> {
            Err(JudgePolicyStoreError::other("db down"))
        }
        async fn set(&self, _policy: JudgePolicy) -> Result<(), JudgePolicyStoreError> {
            Err(JudgePolicyStoreError::other("db down"))
        }
        async fn set_cas(
            &self,
            _expected: &JudgePolicy,
            _policy: JudgePolicy,
        ) -> Result<(), JudgePolicyStoreError> {
            Err(JudgePolicyStoreError::other("db down"))
        }
    }

    #[tokio::test]
    async fn get_failure_on_a_cold_cache_propagates_instead_of_fabricating_default() {
        let cached = CachedJudgePolicyStore::new(
            Arc::new(FailingGetStore),
            std::time::Duration::from_secs(60),
        );
        let err = cached
            .get()
            .await
            .expect_err("a cold cache backed by a failing store must surface the error");
        assert!(!err.is_conflict());
    }

    #[tokio::test]
    async fn set_cas_rejects_a_stale_policy_revision() {
        let store = InMemoryJudgePolicyStore::new(JudgePolicy::default());
        let base = store.get().await.unwrap();

        let mut first = base.clone();
        first.enabled = true;
        store
            .set_cas(&base, first)
            .await
            .expect("first write succeeds");

        let mut stale = base;
        stale.shadow = true;
        let err = store
            .set_cas(&stale, stale.clone())
            .await
            .expect_err("stale write must conflict");
        assert!(err.is_conflict());

        let current = store.get().await.unwrap();
        assert_eq!(current.revision, 1);
        assert!(current.enabled);
        assert!(!current.shadow, "stale write must not overwrite fields");
    }

    #[test]
    fn opt_in_inheritance() {
        let mut policy = JudgePolicy::default();
        // Default off: only an explicit true opts in.
        assert!(!policy.key_opted_in(None));
        assert!(policy.key_opted_in(Some(true)));
        assert!(!policy.key_opted_in(Some(false)));
        // Global default on: None inherits, false still opts out.
        policy.default_opt_in = true;
        assert!(policy.key_opted_in(None));
        assert!(policy.key_opted_in(Some(true)));
        assert!(!policy.key_opted_in(Some(false)));
    }

    #[test]
    fn floor_inheritance() {
        let mut policy = JudgePolicy::default();
        assert_eq!(policy.effective_floor(None), None);
        policy.default_floor = Some("chalk/haiku".to_string());
        assert_eq!(policy.effective_floor(None), Some("chalk/haiku"));
        // A key's own floor overrides the default.
        assert_eq!(
            policy.effective_floor(Some("chalk/sonnet")),
            Some("chalk/sonnet")
        );
    }

    #[test]
    fn ladder_lookup_canonicalizes() {
        let mut policy = JudgePolicy::default();
        policy.ladders.insert(
            "chalk/gpt-4o".to_string(),
            JudgeLadder {
                targets: vec!["chalk/gpt-4o-mini".to_string()],
                boundaries: vec![],
            },
        );
        assert!(policy.ladder_for("Chalk/GPT-4o").is_some());
        assert!(policy.ladder_for("chalk/other").is_none());
    }
}
