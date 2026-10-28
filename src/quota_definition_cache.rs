//! Bounded, replica-local caches for immutable quota definitions only.
//!
//! Local writes invalidate immediately. A hard TTL bounds edits made by other
//! replicas; expired loads fail rather than serving stale (possibly empty) data.
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::budget::{ApiKeyBudgetInput, BudgetPolicy, BudgetPolicyStore};
use crate::ratelimit::{RateLimitMatch, RateLimitModel, RateLimitPolicy, RateLimitPolicyStore};
use crate::service::ServiceError;

const TTL: Duration = Duration::from_secs(1);
const CAPACITY: usize = 8192;
const LOAD_STRIPES: usize = 64;

struct Entry<V> {
    value: Arc<Vec<V>>,
    loaded_at: Instant,
}

struct State<K, V> {
    generation: u64,
    entries: HashMap<K, Entry<V>>,
    order: VecDeque<K>,
}

struct MatchingCache<K, V> {
    state: Mutex<State<K, V>>,
    loads: [tokio::sync::Mutex<()>; LOAD_STRIPES],
    ttl: Duration,
    capacity: usize,
}

impl<K: Clone + Eq + Hash, V> MatchingCache<K, V> {
    fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                generation: 0,
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
            loads: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
            ttl,
            capacity,
        }
    }

    fn invalidate(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
        state.order.clear();
    }

    fn lookup(&self, key: &K) -> Option<Arc<Vec<V>>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get(key)
            .filter(|entry| entry.loaded_at.elapsed() < self.ttl)
            .map(|entry| entry.value.clone())
    }

    async fn get<F, Fut>(&self, key: K, load: F) -> Result<Arc<Vec<V>>, ServiceError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Vec<V>, ServiceError>>,
    {
        if let Some(value) = self.lookup(&key) {
            return Ok(value);
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hash);
        // A fixed set of locks bounds miss bookkeeping as well as stored entries.
        let _load = self.loads[hash.finish() as usize % LOAD_STRIPES]
            .lock()
            .await;
        loop {
            if let Some(value) = self.lookup(&key) {
                return Ok(value);
            }
            let generation = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .generation;
            let result = load().await;
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // A commit during this load must not republish old definitions or absence.
            if generation != state.generation {
                continue;
            }
            let value = Arc::new(result?);
            if self.capacity > 0 {
                if !state.entries.contains_key(&key) {
                    if state.entries.len() >= self.capacity {
                        if let Some(oldest) = state.order.pop_front() {
                            state.entries.remove(&oldest);
                        }
                    }
                    state.order.push_back(key.clone());
                }
                state.entries.insert(
                    key.clone(),
                    Entry {
                        value: value.clone(),
                        loaded_at: Instant::now(),
                    },
                );
            }
            return Ok(value);
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct BudgetKey {
    key_hash: String,
    usage_pool_id: Option<String>,
}

pub struct CachedBudgetPolicyStore {
    inner: Arc<dyn BudgetPolicyStore>,
    // Each store is scoped to exactly one environment by its constructor.
    cache: MatchingCache<BudgetKey, BudgetPolicy>,
}

impl CachedBudgetPolicyStore {
    pub fn new(inner: Arc<dyn BudgetPolicyStore>) -> Self {
        Self {
            inner,
            cache: MatchingCache::new(TTL, CAPACITY),
        }
    }
}

#[async_trait]
impl BudgetPolicyStore for CachedBudgetPolicyStore {
    fn co_locates_key_writes(&self) -> bool {
        self.inner.co_locates_key_writes()
    }
    fn invalidate_matching(&self) {
        self.cache.invalidate();
        self.inner.invalidate_matching();
    }
    async fn set_key_daily_budget(
        &self,
        key_hash: &str,
        amount: Option<u64>,
    ) -> Result<(), ServiceError> {
        self.inner.set_key_daily_budget(key_hash, amount).await?;
        self.invalidate_matching();
        Ok(())
    }
    async fn replace_key_budgets(
        &self,
        key_hash: &str,
        budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<(), ServiceError> {
        self.inner.replace_key_budgets(key_hash, budgets).await?;
        self.invalidate_matching();
        Ok(())
    }
    async fn get_key_daily_budget(&self, key_hash: &str) -> Result<Option<u64>, ServiceError> {
        self.inner.get_key_daily_budget(key_hash).await
    }
    async fn get_key_daily_budgets(
        &self,
        keys: &[String],
    ) -> Result<HashMap<String, u64>, ServiceError> {
        self.inner.get_key_daily_budgets(keys).await
    }
    async fn matching(
        &self,
        key: &str,
        pool: Option<&str>,
    ) -> Result<Vec<BudgetPolicy>, ServiceError> {
        Ok(self.matching_shared(key, pool).await?.as_ref().clone())
    }
    async fn matching_shared(
        &self,
        key: &str,
        pool: Option<&str>,
    ) -> Result<Arc<Vec<BudgetPolicy>>, ServiceError> {
        self.cache
            .get(
                BudgetKey {
                    key_hash: key.into(),
                    usage_pool_id: pool.map(Into::into),
                },
                || self.inner.matching(key, pool),
            )
            .await
    }
    async fn get(&self, id: &str) -> Result<Option<BudgetPolicy>, ServiceError> {
        self.inner.get(id).await
    }
    async fn list(&self) -> Result<Vec<BudgetPolicy>, ServiceError> {
        self.inner.list().await
    }
    async fn create(&self, policy: BudgetPolicy) -> Result<BudgetPolicy, ServiceError> {
        let value = self.inner.create(policy).await?;
        self.invalidate_matching();
        Ok(value)
    }
    async fn update(
        &self,
        id: &str,
        amount: Option<u64>,
        enabled: Option<bool>,
    ) -> Result<BudgetPolicy, ServiceError> {
        let value = self.inner.update(id, amount, enabled).await?;
        self.invalidate_matching();
        Ok(value)
    }
    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.inner.delete(id).await?;
        self.invalidate_matching();
        Ok(())
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct RateKey {
    key_hash: String,
    usage_pool_id: Option<String>,
    connection_id: Option<String>,
    model_ids: Vec<String>,
    provider_selected: bool,
}

pub struct CachedRateLimitPolicyStore {
    inner: Arc<dyn RateLimitPolicyStore>,
    cache: MatchingCache<RateKey, RateLimitPolicy>,
}
impl CachedRateLimitPolicyStore {
    pub fn new(inner: Arc<dyn RateLimitPolicyStore>) -> Self {
        Self {
            inner,
            cache: MatchingCache::new(TTL, CAPACITY),
        }
    }
}
#[async_trait]
impl RateLimitPolicyStore for CachedRateLimitPolicyStore {
    fn co_locates_key_writes(&self) -> bool {
        self.inner.co_locates_key_writes()
    }
    fn invalidate_matching(&self) {
        self.cache.invalidate();
        self.inner.invalidate_matching();
    }
    async fn matching(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        Ok(self.matching_shared(request).await?.as_ref().clone())
    }
    async fn matching_shared(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<Arc<Vec<RateLimitPolicy>>, ServiceError> {
        let key = RateKey {
            key_hash: request.key_hash.into(),
            usage_pool_id: request.usage_pool_id.map(Into::into),
            connection_id: request.connection_id.map(Into::into),
            model_ids: request.model.selector_ids(),
            provider_selected: matches!(request.model, RateLimitModel::ProviderSelected),
        };
        self.cache.get(key, || self.inner.matching(request)).await
    }
    async fn create(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        let value = self.inner.create(policy).await?;
        self.invalidate_matching();
        Ok(value)
    }
    async fn get(&self, id: &str) -> Result<RateLimitPolicy, ServiceError> {
        self.inner.get(id).await
    }
    async fn list(&self) -> Vec<RateLimitPolicy> {
        self.inner.list().await
    }
    async fn list_page(
        &self,
        before: DateTime<Utc>,
        fetch: i64,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        self.inner.list_page(before, fetch).await
    }
    async fn update(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        let value = self.inner.update(policy).await?;
        self.invalidate_matching();
        Ok(value)
    }
    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.inner.delete(id).await?;
        self.invalidate_matching();
        Ok(())
    }
    async fn delete_for_api_key(&self, key: &str) -> Result<(), ServiceError> {
        self.inner.delete_for_api_key(key).await?;
        self.invalidate_matching();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn coalesces_misses_and_shares_empty_results() {
        let cache = MatchingCache::<u64, u64>::new(TTL, 2);
        let reads = AtomicUsize::new(0);
        let load = || async {
            reads.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(vec![])
        };
        let (a, b) = tokio::join!(cache.get(1, load), cache.get(1, load));
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidation_retries_an_inflight_old_negative_load() {
        let cache = Arc::new(MatchingCache::<u64, u64>::new(TTL, 2));
        let reads = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let task = {
            let (cache, reads, started, resume) = (
                cache.clone(),
                reads.clone(),
                started.clone(),
                resume.clone(),
            );
            tokio::spawn(async move {
                cache
                    .get(1, || async {
                        if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                            started.notify_one();
                            resume.notified().await;
                            Ok(vec![])
                        } else {
                            Ok(vec![42])
                        }
                    })
                    .await
                    .unwrap()
            })
        };
        started.notified().await;
        cache.invalidate();
        resume.notify_one();
        assert_eq!(task.await.unwrap().as_slice(), &[42]);
        assert_eq!(
            cache
                .get(1, || async { panic!("must have cached new generation") })
                .await
                .unwrap()
                .as_slice(),
            &[42]
        );
    }

    #[tokio::test]
    async fn expiry_propagates_errors_and_capacity_is_bounded() {
        let cache = MatchingCache::<u64, u64>::new(Duration::from_millis(10), 2);
        cache.get(1, || async { Ok(vec![]) }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(cache
            .get(1, || async { Err(ServiceError::internal("load failed")) })
            .await
            .is_err());
        assert!(cache
            .get(1, || async { Err(ServiceError::internal("still failed")) })
            .await
            .is_err());
        for key in 1..100 {
            cache.get(key, || async { Ok(vec![key]) }).await.unwrap();
        }
        let state = cache.state.lock().unwrap();
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.order.len(), 2);
    }
}
