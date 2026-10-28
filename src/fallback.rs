use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::revisioned_policy::{
    RevisionedPolicy, RevisionedPolicyStore, RevisionedPolicyStoreError,
};

/// Env-scoped fallback chains. `revision` is store-managed for compare-and-swap
/// admin updates and is not exposed on the admin wire format.
///
/// The store trait, in-memory store, and short-TTL cache are shared with
/// [`crate::judge_policy`] via [`crate::revisioned_policy`] — see that
/// module's docs for the consistency argument. This file keeps only the wire
/// format and the Postgres store (its table and JSON shape are
/// fallback-specific); everything else below is a type alias so existing
/// callers (`main.rs`, `lib.rs`, the gRPC/service layers, tests) are
/// unaffected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FallbackPolicy {
    /// Monotonically increasing, store-managed version used to protect
    /// read-modify-write admin updates.
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    fallbacks: HashMap<String, Vec<String>>,
}

impl FallbackPolicy {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_map(fallbacks: HashMap<String, Vec<String>>) -> Self {
        Self {
            revision: 0,
            fallbacks,
        }
    }

    pub fn fallbacks_for(&self, model: &str) -> &[String] {
        self.fallbacks.get(model).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn as_map(&self) -> &HashMap<String, Vec<String>> {
        &self.fallbacks
    }

    /// Decode a persisted JSON blob. Accepts the current envelope
    /// (`{revision, fallbacks}`) and the legacy bare model→list map.
    fn from_stored_json(json: serde_json::Value) -> Result<Self, String> {
        match &json {
            serde_json::Value::Object(map)
                if map.get("fallbacks").is_some_and(|value| value.is_object()) =>
            {
                serde_json::from_value(json).map_err(|e| e.to_string())
            }
            _ => {
                let fallbacks: HashMap<String, Vec<String>> =
                    serde_json::from_value(json).map_err(|e| e.to_string())?;
                Ok(Self {
                    revision: 0,
                    fallbacks,
                })
            }
        }
    }
}

impl RevisionedPolicy for FallbackPolicy {
    const CACHE_TAG: &'static str = "fallback_policy";
    const DISPLAY_NAME: &'static str = "fallback policy";

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
pub type FallbackPolicyStoreError = RevisionedPolicyStoreError;

/// Alias for the shared in-memory store; see
/// [`crate::revisioned_policy::InMemoryRevisionedStore`].
pub type InMemoryFallbackPolicyStore =
    crate::revisioned_policy::InMemoryRevisionedStore<FallbackPolicy>;

/// Alias for the shared short-TTL cache; see
/// [`crate::revisioned_policy::CachedRevisionedStore`].
pub type CachedFallbackPolicyStore =
    crate::revisioned_policy::CachedRevisionedStore<FallbackPolicy>;

/// Marker for `Arc<dyn FallbackPolicyStore>` trait objects. The real trait,
/// [`RevisionedPolicyStore<FallbackPolicy>`], carries a generic parameter and
/// so cannot be spelled bare after `dyn`; this marker is blanket-implemented
/// for every type that implements the generic trait, so existing call sites
/// (service/gRPC layers constructing `Arc<dyn FallbackPolicyStore>`) need no
/// changes.
pub trait FallbackPolicyStore: RevisionedPolicyStore<FallbackPolicy> {}
impl<T: RevisionedPolicyStore<FallbackPolicy>> FallbackPolicyStore for T {}

// ---------------------------------------------------------------------------
// PostgreSQL implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PostgresFallbackPolicyStore {
    pool: sqlx::PgPool,
    /// Tenancy scope (see `PostgresApiKeyStore`); one policy row per environment.
    environment_id: String,
}

impl PostgresFallbackPolicyStore {
    /// Wrap an existing pool. Tables are provisioned by migrations before boot.
    pub fn new(pool: sqlx::PgPool, environment_id: String) -> Self {
        Self {
            pool,
            environment_id,
        }
    }
}

#[async_trait]
impl RevisionedPolicyStore<FallbackPolicy> for PostgresFallbackPolicyStore {
    async fn get(&self) -> Result<FallbackPolicy, FallbackPolicyStoreError> {
        // A read failure must surface as an error, never as an empty policy:
        // the cached wrapper keeps serving last-known-good on `Err`, whereas a
        // silently-empty result would disable every configured fallback for a
        // full TTL window.
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT policy FROM chalk_router.fallback_policy WHERE environment_id = $1",
        )
        .bind(&self.environment_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to read fallback policy");
            FallbackPolicyStoreError::other(format!("failed to read fallback policy: {e}"))
        })?;

        match row {
            Some((json,)) => FallbackPolicy::from_stored_json(json).map_err(|e| {
                tracing::error!(error = %e, "Failed to decode fallback policy");
                FallbackPolicyStoreError::other(format!("failed to decode fallback policy: {e}"))
            }),
            None => Ok(FallbackPolicy::empty()),
        }
    }

    async fn set(&self, policy: FallbackPolicy) -> Result<(), FallbackPolicyStoreError> {
        let expected = self.get().await?;
        self.set_cas(&expected, policy).await
    }

    async fn set_cas(
        &self,
        expected: &FallbackPolicy,
        mut policy: FallbackPolicy,
    ) -> Result<(), FallbackPolicyStoreError> {
        policy.revision = expected.revision.saturating_add(1);
        let json = serde_json::to_value(&policy).map_err(|e| {
            FallbackPolicyStoreError::other(format!("failed to encode fallback policy: {e}"))
        })?;
        let result = sqlx::query(
            r#"
            INSERT INTO chalk_router.fallback_policy (environment_id, policy) VALUES ($1, $2)
            ON CONFLICT (environment_id) DO UPDATE SET policy = EXCLUDED.policy
            WHERE COALESCE((chalk_router.fallback_policy.policy->>'revision')::bigint, 0) = $3
            "#,
        )
        .bind(&self.environment_id)
        .bind(&json)
        .bind(expected.revision as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to persist fallback policy");
            FallbackPolicyStoreError::other(format!("failed to persist fallback policy: {e}"))
        })?;
        if result.rows_affected() == 0 {
            return Err(FallbackPolicyStoreError::Conflict);
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
        policy: RwLock<FallbackPolicy>,
    }

    #[async_trait]
    impl RevisionedPolicyStore<FallbackPolicy> for CountingStore {
        async fn get(&self) -> Result<FallbackPolicy, FallbackPolicyStoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Ok(self.policy.read().await.clone())
        }
        async fn set(&self, policy: FallbackPolicy) -> Result<(), FallbackPolicyStoreError> {
            let expected = self.get().await?;
            self.set_cas(&expected, policy).await
        }
        async fn set_cas(
            &self,
            expected: &FallbackPolicy,
            mut policy: FallbackPolicy,
        ) -> Result<(), FallbackPolicyStoreError> {
            let mut current = self.policy.write().await;
            if current.revision != expected.revision {
                return Err(FallbackPolicyStoreError::Conflict);
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
            policy: RwLock::new(FallbackPolicy::empty()),
        });
        let cached =
            CachedFallbackPolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));
        cached.get().await.expect("get");
        cached.get().await.expect("get");
        cached.get().await.expect("get");
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cached_store_set_writes_through_and_serves_immediately() {
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(FallbackPolicy::empty()),
        });
        let cached =
            CachedFallbackPolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));
        assert!(cached
            .get()
            .await
            .expect("get")
            .fallbacks_for("chalk/big")
            .is_empty());
        let mut map = HashMap::new();
        map.insert("chalk/big".to_string(), vec!["chalk/mid".to_string()]);
        cached
            .set(FallbackPolicy::from_map(map))
            .await
            .expect("set persists");
        // Served from the refreshed cache — no extra inner read.
        assert_eq!(
            cached.get().await.expect("get").fallbacks_for("chalk/big"),
            ["chalk/mid".to_string()]
        );
        assert!(
            !inner
                .policy
                .read()
                .await
                .fallbacks_for("chalk/big")
                .is_empty(),
            "write-through persisted"
        );
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cached_store_expires_after_ttl() {
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(FallbackPolicy::empty()),
        });
        let cached =
            CachedFallbackPolicyStore::new(inner.clone(), std::time::Duration::from_millis(0));
        cached.get().await.expect("get");
        cached.get().await.expect("get");
        // Zero TTL → every get falls through.
        assert_eq!(inner.gets.load(Ordering::SeqCst), 2);
    }

    /// Inner store whose writes always fail, to assert failure propagation.
    struct FailingWriteStore {
        policy: RwLock<FallbackPolicy>,
    }

    #[async_trait]
    impl RevisionedPolicyStore<FallbackPolicy> for FailingWriteStore {
        async fn get(&self) -> Result<FallbackPolicy, FallbackPolicyStoreError> {
            Ok(self.policy.read().await.clone())
        }
        async fn set(&self, _policy: FallbackPolicy) -> Result<(), FallbackPolicyStoreError> {
            Err(FallbackPolicyStoreError::other("db down"))
        }
        async fn set_cas(
            &self,
            _expected: &FallbackPolicy,
            _policy: FallbackPolicy,
        ) -> Result<(), FallbackPolicyStoreError> {
            Err(FallbackPolicyStoreError::other("db down"))
        }
    }

    #[tokio::test]
    async fn failed_persist_neither_caches_nor_reports_success() {
        let inner = Arc::new(FailingWriteStore {
            policy: RwLock::new(FallbackPolicy::empty()),
        });
        let cached = CachedFallbackPolicyStore::new(inner, std::time::Duration::from_secs(60));
        let mut map = HashMap::new();
        map.insert("chalk/big".to_string(), vec!["chalk/mid".to_string()]);
        assert!(
            cached.set(FallbackPolicy::from_map(map)).await.is_err(),
            "persistence failure must propagate"
        );
        assert!(
            cached
                .get()
                .await
                .expect("get")
                .fallbacks_for("chalk/big")
                .is_empty(),
            "an unpersisted policy must never be served from the cache"
        );
    }

    /// Inner store whose reads fail after the first success, to assert the
    /// cache serves last-known-good on read failure instead of an empty policy.
    struct FlakyReadStore {
        policy: FallbackPolicy,
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RevisionedPolicyStore<FallbackPolicy> for FlakyReadStore {
        async fn get(&self) -> Result<FallbackPolicy, FallbackPolicyStoreError> {
            if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(self.policy.clone())
            } else {
                Err(FallbackPolicyStoreError::other("db down"))
            }
        }
        async fn set(&self, _policy: FallbackPolicy) -> Result<(), FallbackPolicyStoreError> {
            unreachable!("test never writes")
        }
        async fn set_cas(
            &self,
            _expected: &FallbackPolicy,
            _policy: FallbackPolicy,
        ) -> Result<(), FallbackPolicyStoreError> {
            unreachable!("test never writes")
        }
    }

    #[tokio::test]
    async fn failed_read_serves_last_known_good_not_empty() {
        let mut map = HashMap::new();
        map.insert("chalk/big".to_string(), vec!["chalk/mid".to_string()]);
        let inner = Arc::new(FlakyReadStore {
            policy: FallbackPolicy::from_map(map),
            reads: AtomicUsize::new(0),
        });
        // Zero TTL forces every get through to the inner store, so the second
        // get exercises the inner-error path with a stale value present.
        let cached = CachedFallbackPolicyStore::new(inner, std::time::Duration::from_millis(0));
        assert_eq!(
            cached
                .get()
                .await
                .expect("first read succeeds")
                .fallbacks_for("chalk/big"),
            ["chalk/mid".to_string()]
        );
        assert_eq!(
            cached
                .get()
                .await
                .expect("read failure must serve stale, not error")
                .fallbacks_for("chalk/big"),
            ["chalk/mid".to_string()],
            "a transient read failure must not shadow the last-known-good policy"
        );
    }

    /// Inner store whose reads always fail, to assert cold-cache propagation.
    struct FailingReadStore {
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RevisionedPolicyStore<FallbackPolicy> for FailingReadStore {
        async fn get(&self) -> Result<FallbackPolicy, FallbackPolicyStoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Err(FallbackPolicyStoreError::other("db down"))
        }
        async fn set(&self, _policy: FallbackPolicy) -> Result<(), FallbackPolicyStoreError> {
            unreachable!("test never writes")
        }
        async fn set_cas(
            &self,
            _expected: &FallbackPolicy,
            _policy: FallbackPolicy,
        ) -> Result<(), FallbackPolicyStoreError> {
            unreachable!("test never writes")
        }
    }

    #[tokio::test]
    async fn failed_cold_read_propagates_error_and_backs_off() {
        let inner = Arc::new(FailingReadStore {
            reads: AtomicUsize::new(0),
        });
        let cached =
            CachedFallbackPolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));
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
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(FallbackPolicy::empty()),
        });
        let cached =
            CachedFallbackPolicyStore::new(inner.clone(), std::time::Duration::from_secs(60));

        let expected = cached.get().await.unwrap();
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);

        let mut bumped = expected.clone();
        bumped
            .fallbacks
            .insert("chalk/big".to_string(), vec!["chalk/mid".to_string()]);
        inner
            .set_cas(&expected, bumped)
            .await
            .expect("inner write advances the revision");

        let mut next = expected.clone();
        next.fallbacks
            .insert("chalk/big".to_string(), vec!["chalk/other".to_string()]);
        let err = cached
            .set_cas(&expected, next)
            .await
            .expect_err("stale cached revision must conflict");
        assert!(err.is_conflict());

        let after = cached.get().await.unwrap();
        assert_eq!(
            inner.gets.load(Ordering::SeqCst),
            2,
            "a conflict must clear the cache so the next get refreshes from inner"
        );
        assert_eq!(after.fallbacks_for("chalk/big"), ["chalk/mid".to_string()]);
    }

    #[tokio::test]
    async fn set_cas_rejects_a_stale_policy_revision() {
        let store = InMemoryFallbackPolicyStore::new(FallbackPolicy::empty());
        let base = store.get().await.unwrap();
        let mut first = base.clone();
        first
            .fallbacks
            .insert("chalk/a".to_string(), vec!["chalk/b".to_string()]);
        store
            .set_cas(&base, first)
            .await
            .expect("first CAS succeeds");

        let stale = base;
        let err = store
            .set_cas(&stale, stale.clone())
            .await
            .expect_err("stale expected revision must conflict");
        assert!(err.is_conflict());
        let current = store.get().await.unwrap();
        assert_eq!(current.revision, 1);
        assert_eq!(current.fallbacks_for("chalk/a"), ["chalk/b".to_string()]);
    }

    #[test]
    fn from_stored_json_accepts_legacy_bare_map() {
        let json = serde_json::json!({
            "chalk/big": ["chalk/mid", "chalk/small"]
        });
        let policy = FallbackPolicy::from_stored_json(json).expect("legacy decode");
        assert_eq!(policy.revision, 0);
        assert_eq!(
            policy.fallbacks_for("chalk/big"),
            ["chalk/mid".to_string(), "chalk/small".to_string()]
        );
    }

    #[test]
    fn from_stored_json_accepts_revision_envelope() {
        let json = serde_json::json!({
            "revision": 7,
            "fallbacks": { "chalk/big": ["chalk/mid"] }
        });
        let policy = FallbackPolicy::from_stored_json(json).expect("envelope decode");
        assert_eq!(policy.revision, 7);
        assert_eq!(policy.fallbacks_for("chalk/big"), ["chalk/mid".to_string()]);
    }
}
