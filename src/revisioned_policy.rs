//! Generic short-TTL, compare-and-swap policy store shared by
//! [`crate::fallback`] and [`crate::judge_policy`] (see their docs for the
//! domain-specific wire formats). Both are a single env-scoped JSON blob read
//! on every request's hot path and updated via admin read-modify-write, so
//! both need: an in-memory revisioned store, a short-TTL write-through cache
//! in front of it, and a Postgres store using the same `revision`-guarded
//! `INSERT ... ON CONFLICT ... WHERE` compare-and-swap. This module provides
//! the store trait, the in-memory store, and the cache; each policy module
//! keeps its own `Postgres*Store` (the SQL table name and JSON shape differ)
//! plus a thin set of type aliases so existing callers keep their names.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::Instrument;

/// A policy usable with [`RevisionedPolicyStore`]: carries a store-managed
/// `revision` for compare-and-swap updates, plus fixed tags used to
/// reconstruct policy-specific tracing fields and log text from otherwise
/// fully generic code.
pub trait RevisionedPolicy: Clone + Default + Send + Sync + 'static {
    /// Short, snake_case tag for tracing fields (e.g. `"fallback_policy"`).
    const CACHE_TAG: &'static str;
    /// Human-readable name for log messages (e.g. `"fallback policy"`).
    const DISPLAY_NAME: &'static str;

    fn revision(&self) -> u64;
    fn set_revision(&mut self, revision: u64);
}

/// Persistence failure from a [`RevisionedPolicyStore`].
///
/// `Conflict` is a distinct variant (not a stringly-typed sentinel) so a
/// wrapper store can never accidentally downgrade a CAS conflict into a
/// generic failure by reformatting or re-wrapping the message.
#[derive(Debug, Clone)]
pub enum RevisionedPolicyStoreError {
    /// The CAS `expected` revision no longer matches the stored policy.
    Conflict,
    /// Any other read/write/encode/decode failure, with a human-readable
    /// message for logs and error responses.
    Other(String),
}

impl RevisionedPolicyStoreError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict)
    }
}

impl std::fmt::Display for RevisionedPolicyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict => write!(
                f,
                "policy was modified concurrently; re-read and retry the update"
            ),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for RevisionedPolicyStoreError {}

#[async_trait]
pub trait RevisionedPolicyStore<P: RevisionedPolicy>: Send + Sync {
    /// Read the active policy. An `Err` means the policy could not be READ
    /// (store unreachable, row undecodable) — NOT that none is configured;
    /// an absent policy reads as `Ok(P::default())`. Callers must not
    /// conflate the two: treating a read failure as a default policy can
    /// silently disable configured behavior.
    async fn get(&self) -> Result<P, RevisionedPolicyStoreError>;
    /// Persist a policy. An `Err` means the policy was NOT durably stored and
    /// must not be reported (or served) as the active policy — callers
    /// propagate it to the admin rather than logging and pretending success.
    async fn set(&self, policy: P) -> Result<(), RevisionedPolicyStoreError>;
    /// Persist `next` only when the stored policy still has the revision in
    /// `expected`. Successful writes must advance the revision by exactly one.
    /// A stale expected revision returns [`RevisionedPolicyStoreError::Conflict`].
    async fn set_cas(&self, expected: &P, next: P) -> Result<(), RevisionedPolicyStoreError>;
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct InMemoryRevisionedStore<P> {
    policy: Arc<RwLock<P>>,
}

impl<P: RevisionedPolicy> InMemoryRevisionedStore<P> {
    pub fn new(initial: P) -> Self {
        Self {
            policy: Arc::new(RwLock::new(initial)),
        }
    }
}

#[async_trait]
impl<P: RevisionedPolicy> RevisionedPolicyStore<P> for InMemoryRevisionedStore<P> {
    async fn get(&self) -> Result<P, RevisionedPolicyStoreError> {
        Ok(self.policy.read().await.clone())
    }

    async fn set(&self, policy: P) -> Result<(), RevisionedPolicyStoreError> {
        let expected = self.get().await?;
        self.set_cas(&expected, policy).await
    }

    async fn set_cas(&self, expected: &P, mut next: P) -> Result<(), RevisionedPolicyStoreError> {
        let mut current = self.policy.write().await;
        if current.revision() != expected.revision() {
            return Err(RevisionedPolicyStoreError::Conflict);
        }
        next.set_revision(expected.revision().saturating_add(1));
        *current = next;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Short-TTL write-through cache
// ---------------------------------------------------------------------------

/// What the cache currently knows about the policy.
enum CacheEntry<P> {
    /// Nothing read yet (cold).
    Empty,
    /// Last successful read/write. Kept past the TTL as the last-known-good
    /// value to serve when a refresh fails.
    Good(std::time::Instant, P),
    /// The last COLD read failed (there was no last-known-good to serve).
    /// Within the TTL this fails fast instead of letting every request
    /// serialize behind another doomed inner read — with a slow store (e.g.
    /// a pool-acquire timeout), N queued requests would otherwise each wait
    /// their own full timeout under the `update` mutex.
    Failed(std::time::Instant, String),
}

/// Wraps a [`RevisionedPolicyStore`] with a short-TTL in-memory cache so the
/// per-request policy read on the hot path is a memory read in steady state
/// (~1 DB read per TTL window per replica). `set` writes through and
/// refreshes the cache immediately, so the replica that takes a policy edit
/// serves it at once; other replicas converge within the TTL.
///
/// Consistency comes from one rule: every inner read/write **and** its cache
/// install happen under the single `update` mutex. Concurrent misses collapse
/// into one DB query; a refresh can't interleave with a `set` (nor two `set`s
/// with each other) to leave the cache and the database disagreeing; and a
/// failed persist never touches the cache, so this replica keeps serving the
/// last durably-stored policy instead of one that exists nowhere else. A
/// failed inner READ serves the last-known-good value (re-stamped, so
/// retries back off a full TTL) instead of caching a default policy; with no
/// last-known-good (cold cache) the failure itself is cached for a TTL so
/// concurrent and subsequent requests fail fast instead of each waiting out
/// their own inner read.
pub struct CachedRevisionedStore<P: RevisionedPolicy> {
    inner: Arc<dyn RevisionedPolicyStore<P>>,
    ttl: std::time::Duration,
    cached: RwLock<CacheEntry<P>>,
    /// Serializes inner reads/writes together with their cache installs (see
    /// the type docs). Reads of a fresh cache never take it.
    update: tokio::sync::Mutex<()>,
}

impl<P: RevisionedPolicy> CachedRevisionedStore<P> {
    pub fn new(inner: Arc<dyn RevisionedPolicyStore<P>>, ttl: std::time::Duration) -> Self {
        Self {
            inner,
            ttl,
            cached: RwLock::new(CacheEntry::Empty),
            update: tokio::sync::Mutex::new(()),
        }
    }

    /// The cache's verdict while its entry is within the TTL: a fresh value,
    /// a fresh failure (fail fast, see [`CacheEntry::Failed`]), or `None` —
    /// time to consult the inner store.
    async fn fresh(&self) -> Option<Result<P, RevisionedPolicyStoreError>> {
        match &*self.cached.read().await {
            CacheEntry::Good(at, policy) if at.elapsed() < self.ttl => Some(Ok(policy.clone())),
            CacheEntry::Failed(at, msg) if at.elapsed() < self.ttl => {
                Some(Err(RevisionedPolicyStoreError::other(format!(
                    "{} read failing (backing off): {msg}",
                    P::DISPLAY_NAME
                ))))
            }
            _ => None,
        }
    }
}

#[async_trait]
impl<P: RevisionedPolicy> RevisionedPolicyStore<P> for CachedRevisionedStore<P> {
    async fn get(&self) -> Result<P, RevisionedPolicyStoreError> {
        if let Some(verdict) = self.fresh().await {
            tracing::trace!(cache = P::CACHE_TAG, hit = true);
            return verdict;
        }
        let lock_started = std::time::Instant::now();
        let _update = self.update.lock().await;
        tracing::debug!(
            cache = P::CACHE_TAG,
            wait_ms = lock_started.elapsed().as_secs_f64() * 1_000.0,
            "policy cache refresh lock acquired"
        );
        // A refresh (or a set) that we waited on may have repopulated the cache
        // — including with a fresh failure marker, in which case fail fast
        // rather than repeating the doomed read.
        if let Some(verdict) = self.fresh().await {
            return verdict;
        }
        match self
            .inner
            .get()
            .instrument(tracing::info_span!(
                "policy_cache_refresh",
                cache = P::CACHE_TAG
            ))
            .await
        {
            Ok(policy) => {
                *self.cached.write().await =
                    CacheEntry::Good(std::time::Instant::now(), policy.clone());
                Ok(policy)
            }
            Err(e) => {
                // A transient read failure must not shadow the last-known-good
                // policy with a default one. Serve the stale value and
                // re-stamp it so the hotpath backs off for a full TTL before
                // re-reading a store that just failed. With no last-known-good
                // (cold cache), record the failure — also stamped, so
                // concurrent and subsequent requests fail fast for a TTL
                // instead of each waiting out their own inner read — and
                // propagate the error.
                let mut cached = self.cached.write().await;
                match &mut *cached {
                    CacheEntry::Good(at, stale) => {
                        tracing::warn!(
                            error = %e,
                            "{} read failed; serving last-known-good policy",
                            P::DISPLAY_NAME
                        );
                        *at = std::time::Instant::now();
                        Ok(stale.clone())
                    }
                    _ => {
                        *cached = CacheEntry::Failed(std::time::Instant::now(), e.to_string());
                        Err(e)
                    }
                }
            }
        }
    }

    async fn set(&self, policy: P) -> Result<(), RevisionedPolicyStoreError> {
        let lock_started = std::time::Instant::now();
        let _update = self.update.lock().await;
        tracing::debug!(
            cache = P::CACHE_TAG,
            wait_ms = lock_started.elapsed().as_secs_f64() * 1_000.0,
            "policy cache write lock acquired"
        );
        // Prefer the cache snapshot while holding `update`: a concurrent refresh
        // cannot interleave, and we must not call `inner.get()` here — that can
        // park behind a refresh that already holds this same mutex (deadlock).
        let expected = match &*self.cached.read().await {
            CacheEntry::Good(_, policy) => policy.clone(),
            _ => self.inner.get().await?,
        };
        self.set_cas_locked(&expected, policy).await?;
        Ok(())
    }

    async fn set_cas(&self, expected: &P, policy: P) -> Result<(), RevisionedPolicyStoreError> {
        let lock_started = std::time::Instant::now();
        let _update = self.update.lock().await;
        tracing::debug!(
            cache = P::CACHE_TAG,
            wait_ms = lock_started.elapsed().as_secs_f64() * 1_000.0,
            "policy cache compare-and-swap lock acquired"
        );
        self.set_cas_locked(expected, policy).await
    }
}

impl<P: RevisionedPolicy> CachedRevisionedStore<P> {
    async fn set_cas_locked(
        &self,
        expected: &P,
        mut policy: P,
    ) -> Result<(), RevisionedPolicyStoreError> {
        // Persist FIRST; only a durably-stored policy may enter the cache.
        policy.set_revision(expected.revision().saturating_add(1));
        match self.inner.set_cas(expected, policy.clone()).await {
            Ok(()) => {
                // Also clears any Failed backoff marker: a successful write is
                // proof the store is reachable again.
                *self.cached.write().await = CacheEntry::Good(std::time::Instant::now(), policy);
                Ok(())
            }
            Err(e) => {
                // On ANY failure — and especially a conflict — the cached
                // `expected` revision this replica would retry with is no
                // longer trustworthy: a conflict means it's definitely stale,
                // and any other failure leaves its relationship to the
                // durable state unknown. Clear it so the next get/set
                // refreshes from the inner store instead of spinning against
                // a revision that can never CAS again within the TTL.
                *self.cached.write().await = CacheEntry::Empty;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone, Default)]
    struct TestPolicy {
        revision: u64,
        value: u64,
    }

    impl RevisionedPolicy for TestPolicy {
        const CACHE_TAG: &'static str = "test_policy";
        const DISPLAY_NAME: &'static str = "test policy";

        fn revision(&self) -> u64 {
            self.revision
        }
        fn set_revision(&mut self, revision: u64) {
            self.revision = revision;
        }
    }

    struct CountingStore {
        gets: AtomicUsize,
        policy: RwLock<TestPolicy>,
    }

    #[async_trait]
    impl RevisionedPolicyStore<TestPolicy> for CountingStore {
        async fn get(&self) -> Result<TestPolicy, RevisionedPolicyStoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Ok(self.policy.read().await.clone())
        }
        async fn set(&self, policy: TestPolicy) -> Result<(), RevisionedPolicyStoreError> {
            let expected = self.get().await?;
            self.set_cas(&expected, policy).await
        }
        async fn set_cas(
            &self,
            expected: &TestPolicy,
            mut policy: TestPolicy,
        ) -> Result<(), RevisionedPolicyStoreError> {
            let mut current = self.policy.write().await;
            if current.revision != expected.revision {
                return Err(RevisionedPolicyStoreError::Conflict);
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
            policy: RwLock::new(TestPolicy::default()),
        });
        let cached: CachedRevisionedStore<TestPolicy> =
            CachedRevisionedStore::new(inner.clone(), std::time::Duration::from_secs(60));
        cached.get().await.expect("get");
        cached.get().await.expect("get");
        cached.get().await.expect("get");
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cached_store_set_writes_through_and_serves_immediately() {
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(TestPolicy::default()),
        });
        let cached: CachedRevisionedStore<TestPolicy> =
            CachedRevisionedStore::new(inner.clone(), std::time::Duration::from_secs(60));
        assert_eq!(cached.get().await.expect("get").value, 0);
        cached
            .set(TestPolicy {
                revision: 0,
                value: 7,
            })
            .await
            .expect("set persists");
        // Served from the refreshed cache — no extra inner read.
        assert_eq!(cached.get().await.expect("get").value, 7);
        assert_eq!(
            inner.policy.read().await.value,
            7,
            "write-through persisted"
        );
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cached_store_expires_after_ttl() {
        let inner = Arc::new(CountingStore {
            gets: AtomicUsize::new(0),
            policy: RwLock::new(TestPolicy::default()),
        });
        let cached: CachedRevisionedStore<TestPolicy> =
            CachedRevisionedStore::new(inner.clone(), std::time::Duration::from_millis(0));
        cached.get().await.expect("get");
        cached.get().await.expect("get");
        // Zero TTL → every get falls through.
        assert_eq!(inner.gets.load(Ordering::SeqCst), 2);
    }

    /// Inner store whose writes always fail, to assert failure propagation.
    struct FailingWriteStore {
        policy: RwLock<TestPolicy>,
    }

    #[async_trait]
    impl RevisionedPolicyStore<TestPolicy> for FailingWriteStore {
        async fn get(&self) -> Result<TestPolicy, RevisionedPolicyStoreError> {
            Ok(self.policy.read().await.clone())
        }
        async fn set(&self, _policy: TestPolicy) -> Result<(), RevisionedPolicyStoreError> {
            Err(RevisionedPolicyStoreError::other("db down"))
        }
        async fn set_cas(
            &self,
            _expected: &TestPolicy,
            _policy: TestPolicy,
        ) -> Result<(), RevisionedPolicyStoreError> {
            Err(RevisionedPolicyStoreError::other("db down"))
        }
    }

    #[tokio::test]
    async fn failed_persist_neither_caches_nor_reports_success() {
        let inner = Arc::new(FailingWriteStore {
            policy: RwLock::new(TestPolicy::default()),
        });
        let cached: CachedRevisionedStore<TestPolicy> =
            CachedRevisionedStore::new(inner, std::time::Duration::from_secs(60));
        assert!(
            cached
                .set(TestPolicy {
                    revision: 0,
                    value: 7,
                })
                .await
                .is_err(),
            "persistence failure must propagate"
        );
        assert_eq!(
            cached.get().await.expect("get").value,
            0,
            "an unpersisted policy must never be served from the cache"
        );
    }

    /// Inner store whose reads fail after the first success, to assert the
    /// cache serves last-known-good on read failure instead of a default.
    struct FlakyReadStore {
        policy: TestPolicy,
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RevisionedPolicyStore<TestPolicy> for FlakyReadStore {
        async fn get(&self) -> Result<TestPolicy, RevisionedPolicyStoreError> {
            if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(self.policy.clone())
            } else {
                Err(RevisionedPolicyStoreError::other("db down"))
            }
        }
        async fn set(&self, _policy: TestPolicy) -> Result<(), RevisionedPolicyStoreError> {
            unreachable!("test never writes")
        }
        async fn set_cas(
            &self,
            _expected: &TestPolicy,
            _policy: TestPolicy,
        ) -> Result<(), RevisionedPolicyStoreError> {
            unreachable!("test never writes")
        }
    }

    #[tokio::test]
    async fn failed_read_serves_last_known_good_not_default() {
        let inner = Arc::new(FlakyReadStore {
            policy: TestPolicy {
                revision: 0,
                value: 7,
            },
            reads: AtomicUsize::new(0),
        });
        // Zero TTL forces every get through to the inner store, so the second
        // get exercises the inner-error path with a stale value present.
        let cached: CachedRevisionedStore<TestPolicy> =
            CachedRevisionedStore::new(inner, std::time::Duration::from_millis(0));
        assert_eq!(cached.get().await.expect("first read succeeds").value, 7);
        assert_eq!(
            cached
                .get()
                .await
                .expect("read failure must serve stale, not error")
                .value,
            7,
            "a transient read failure must not shadow the last-known-good policy"
        );
    }

    /// Inner store whose reads always fail, to assert cold-cache propagation
    /// AND backoff (no re-read of a store that just failed, within the TTL).
    struct FailingReadStore {
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RevisionedPolicyStore<TestPolicy> for FailingReadStore {
        async fn get(&self) -> Result<TestPolicy, RevisionedPolicyStoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Err(RevisionedPolicyStoreError::other("db down"))
        }
        async fn set(&self, _policy: TestPolicy) -> Result<(), RevisionedPolicyStoreError> {
            unreachable!("test never writes")
        }
        async fn set_cas(
            &self,
            _expected: &TestPolicy,
            _policy: TestPolicy,
        ) -> Result<(), RevisionedPolicyStoreError> {
            unreachable!("test never writes")
        }
    }

    #[tokio::test]
    async fn failed_cold_read_propagates_error_and_backs_off() {
        let inner = Arc::new(FailingReadStore {
            reads: AtomicUsize::new(0),
        });
        let cached: CachedRevisionedStore<TestPolicy> =
            CachedRevisionedStore::new(inner.clone(), std::time::Duration::from_secs(60));
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
            policy: RwLock::new(TestPolicy::default()),
        });
        let cached: CachedRevisionedStore<TestPolicy> =
            CachedRevisionedStore::new(inner.clone(), std::time::Duration::from_secs(60));

        let expected = cached.get().await.unwrap();
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);

        let mut bumped = expected.clone();
        bumped.value = 7;
        inner
            .set_cas(&expected, bumped)
            .await
            .expect("inner write advances the revision");

        let mut next = expected.clone();
        next.value = 9;
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
        assert_eq!(after.value, 7, "must see the concurrent writer's update");
    }

    #[tokio::test]
    async fn set_cas_rejects_a_stale_policy_revision() {
        let store: InMemoryRevisionedStore<TestPolicy> =
            InMemoryRevisionedStore::new(TestPolicy::default());
        let base = store.get().await.unwrap();

        let mut first = base.clone();
        first.value = 7;
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
        assert_eq!(current.value, 7, "stale write must not overwrite fields");
    }
}
