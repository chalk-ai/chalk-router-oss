//! In-process, bounded, LRU prompt cache.
//!
//! The zero-dependency backend: no Valkey to run, useful for a single-replica
//! deployment, local development, and the tests. Two bounds are enforced
//! together — an entry count and a total byte budget — because completions vary
//! by orders of magnitude in size, and either bound alone leaves the other
//! unbounded (10k entries of 2 MB, or 20 MB spread across a million tiny ones).
//!
//! Eviction is a true LRU (recency is updated on every read, not only on
//! insert), which matters for the shape of this workload: a shared system
//! prompt or a hot eval question is read far more often than it is written, and
//! an insertion-order policy would evict exactly those in favour of one-shot
//! prompts that will never be asked again.
//!
//! Being per-replica, its hit rate falls off roughly as `1/replicas` and it is
//! lost on restart. [`super::valkey::ValkeyPromptCache`] is the shared-cache
//! answer to both; this one is deliberately the simple option.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::{CacheKey, CachedCompletion, PromptCache};

struct Entry {
    value: CachedCompletion,
    expires_at: Instant,
    /// This entry's position in `recency`. Every read and write assigns a
    /// fresh, strictly increasing tick, so the lowest tick in `recency` is
    /// always the least recently *used* entry.
    tick: u64,
    /// Cached at insert so eviction can decrement the byte total without
    /// re-measuring a value it is about to drop.
    weight: usize,
}

struct Inner {
    entries: HashMap<CacheKey, Entry>,
    /// `tick -> key`, kept in sync with `Entry::tick`. A `BTreeMap` rather than
    /// a queue of possibly-stale tags: the tick is *moved* on every access, so
    /// there is exactly one live tag per entry and eviction is an O(log n)
    /// `pop_first` with no staleness to reason about.
    recency: BTreeMap<u64, CacheKey>,
    next_tick: u64,
    bytes: usize,
}

/// A bounded LRU cache with per-entry TTLs.
pub struct MemoryPromptCache {
    inner: Mutex<Inner>,
    max_entries: usize,
    max_bytes: usize,
}

impl MemoryPromptCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                recency: BTreeMap::new(),
                next_tick: 0,
                bytes: 0,
            }),
            // At least one entry: a zero bound would make every `put` evict
            // what it just inserted, which reads as a silently broken cache
            // rather than as a disabled one (`backend=disabled` is how you
            // turn it off).
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
        }
    }

    /// Live entry count, for tests and the debug surface.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.entries.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes currently held, for tests and the debug surface.
    pub fn bytes(&self) -> usize {
        self.inner.lock().map(|i| i.bytes).unwrap_or(0)
    }
}

impl Inner {
    /// Drop an entry and everything that accounts for it.
    fn remove(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.recency.remove(&entry.tick);
            self.bytes = self.bytes.saturating_sub(entry.weight);
        }
    }

    fn next_tick(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick += 1;
        tick
    }
}

#[async_trait]
impl PromptCache for MemoryPromptCache {
    fn kind(&self) -> &'static str {
        "memory"
    }

    async fn get(&self, key: &CacheKey) -> Option<CachedCompletion> {
        // A poisoned mutex means some other caller panicked mid-update. The
        // cache is an optimization, so report a miss rather than propagate.
        let mut inner = self.inner.lock().ok()?;
        let entry = inner.entries.get(key)?;
        if entry.expires_at <= Instant::now() {
            inner.remove(key);
            return None;
        }
        // Promote to most-recently-used: move the tag rather than leaving the
        // old one behind, so `recency` never holds a stale entry.
        let old_tick = entry.tick;
        let value = entry.value.clone();
        let tick = inner.next_tick();
        inner.recency.remove(&old_tick);
        inner.recency.insert(tick, *key);
        if let Some(entry) = inner.entries.get_mut(key) {
            entry.tick = tick;
        }
        Some(value)
    }

    async fn put(&self, key: &CacheKey, value: CachedCompletion, ttl: Duration) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let weight = value.weight();
        // A single entry larger than the whole budget would evict everything
        // and then sit alone; the caller's per-entry ceiling normally catches
        // this first, but a deployment can be configured with the two out of
        // step.
        if weight > self.max_bytes {
            return;
        }
        // Overwrite in place: drop the old accounting before adding the new,
        // or a re-store of the same key would double-count its bytes.
        inner.remove(key);

        let tick = inner.next_tick();
        inner.recency.insert(tick, *key);
        inner.bytes += weight;
        inner.entries.insert(
            *key,
            Entry {
                value,
                expires_at: Instant::now() + ttl,
                tick,
                weight,
            },
        );

        while inner.entries.len() > self.max_entries || inner.bytes > self.max_bytes {
            let Some((_, victim)) = inner.recency.pop_first() else {
                break;
            };
            if let Some(entry) = inner.entries.remove(&victim) {
                inner.bytes = inner.bytes.saturating_sub(entry.weight);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_cache::PayloadKind;

    fn key(n: u8) -> CacheKey {
        let mut bytes = [0u8; 32];
        bytes[0] = n;
        CacheKey(bytes)
    }

    fn value(payload: &[u8]) -> CachedCompletion {
        CachedCompletion::new(
            payload.to_vec(),
            PayloadKind::Json,
            "chalk/smart".to_string(),
        )
    }

    const MINUTE: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn stores_and_returns_an_entry() {
        let cache = MemoryPromptCache::new(8, 1 << 20);
        assert_eq!(cache.get(&key(1)).await, None);
        cache.put(&key(1), value(b"hello"), MINUTE).await;
        assert_eq!(
            cache.get(&key(1)).await.map(|v| v.payload),
            Some(b"hello".to_vec())
        );
    }

    #[tokio::test]
    async fn expired_entries_read_as_a_miss_and_are_dropped() {
        let cache = MemoryPromptCache::new(8, 1 << 20);
        cache.put(&key(1), value(b"hello"), Duration::ZERO).await;
        assert_eq!(cache.get(&key(1)).await, None);
        assert_eq!(cache.len(), 0, "an expired read must reclaim the entry");
        assert_eq!(cache.bytes(), 0);
    }

    #[tokio::test]
    async fn evicts_the_least_recently_used_entry_over_the_count_bound() {
        let cache = MemoryPromptCache::new(2, 1 << 20);
        cache.put(&key(1), value(b"a"), MINUTE).await;
        cache.put(&key(2), value(b"b"), MINUTE).await;
        // Reading 1 makes 2 the least recently used, so 2 — not the
        // oldest-inserted 1 — is what the third insert evicts.
        assert!(cache.get(&key(1)).await.is_some());
        cache.put(&key(3), value(b"c"), MINUTE).await;
        assert!(
            cache.get(&key(1)).await.is_some(),
            "recently read entry survives"
        );
        assert!(
            cache.get(&key(2)).await.is_none(),
            "least recently used evicted"
        );
        assert!(cache.get(&key(3)).await.is_some());
    }

    #[tokio::test]
    async fn evicts_over_the_byte_bound_even_under_the_count_bound() {
        let one = value(b"x").weight();
        let cache = MemoryPromptCache::new(1_000, one * 2);
        cache.put(&key(1), value(b"x"), MINUTE).await;
        cache.put(&key(2), value(b"x"), MINUTE).await;
        cache.put(&key(3), value(b"x"), MINUTE).await;
        assert_eq!(
            cache.len(),
            2,
            "byte budget bounds the cache below the entry bound"
        );
        assert!(cache.get(&key(1)).await.is_none());
        assert!(cache.bytes() <= one * 2);
    }

    #[tokio::test]
    async fn an_entry_larger_than_the_whole_budget_is_refused() {
        let cache = MemoryPromptCache::new(8, 16);
        cache.put(&key(1), value(&[0; 4096]), MINUTE).await;
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
    }

    #[tokio::test]
    async fn overwriting_a_key_does_not_double_count_its_bytes() {
        let cache = MemoryPromptCache::new(8, 1 << 20);
        cache.put(&key(1), value(b"first"), MINUTE).await;
        let after_first = cache.bytes();
        cache.put(&key(1), value(b"first"), MINUTE).await;
        assert_eq!(cache.bytes(), after_first);
        assert_eq!(cache.len(), 1);
        cache.put(&key(1), value(b"second"), MINUTE).await;
        assert_eq!(
            cache.get(&key(1)).await.map(|v| v.payload),
            Some(b"second".to_vec())
        );
    }

    #[tokio::test]
    async fn recency_bookkeeping_stays_bounded_under_churn() {
        // Reads move a tag rather than appending one, so repeated access of a
        // small working set must not grow the index.
        let cache = MemoryPromptCache::new(4, 1 << 20);
        for i in 0..200u8 {
            cache.put(&key(i % 4), value(b"x"), MINUTE).await;
            let _ = cache.get(&key(i % 4)).await;
        }
        let inner = cache.inner.lock().unwrap();
        assert_eq!(inner.recency.len(), inner.entries.len());
        assert!(inner.entries.len() <= 4);
    }
}
