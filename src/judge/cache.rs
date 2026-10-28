//! Bounded, TTL'd cache of judge *difficulty scores*.
//!
//! We cache the difficulty — not the chosen model — because difficulty is
//! key-independent: two keys with different floors would otherwise get each
//! other's rung. Caching the score lets the (cheap) per-key rung mapping
//! recompute while the cache stays shared across all keys, raising the hit rate.
//!
//! Two key derivations share the one cache:
//!   * [`difficulty_cache_key`] hashes the full truncated conversation — pure
//!     dedupe of identical prompts (the session-affinity-off mode).
//!   * [`session_cache_key`] hashes only the conversation *prefix* (system +
//!     first user message), which is stable across the turns of one
//!     conversation — so a hit pins the first-turn difficulty for the whole
//!     session, keeping the served model stable and preserving provider-side
//!     prompt caches. Hard heuristic signals may overwrite the pin upward
//!     (see `route`); nothing ever lowers it mid-session.
//!
//! The key is user-controlled (a hash of message content), so the cache is
//! size-bounded with FIFO eviction and a TTL. Failures and fail-open verdicts
//! are never inserted (the caller only stores genuine results).

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::types::ChatMessage;

use super::message_text_for_prompt;

/// Compute the full-conversation cache key. `prompt_version` also stands in
/// for the truncation parameters (bump it when N-turns/K-chars change), so a
/// truncation change can't serve stale verdicts.
pub fn difficulty_cache_key(
    messages: &[ChatMessage],
    prompt_version: u32,
    judge_model: &str,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prompt_version.hash(&mut hasher);
    judge_model.hash(&mut hasher);
    for m in messages {
        m.role.hash(&mut hasher);
        // content is a serde_json::Value; hash its compact string form.
        if let Some(c) = &m.content {
            c.to_string().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Compute the session-affinity cache key: hashes only the conversation
/// *prefix* (leading system message, if any, plus the first user message), so
/// every turn of one conversation maps to the same key. Namespaced away from
/// [`difficulty_cache_key`] so the two derivations can never collide on one
/// cache entry.
pub fn session_cache_key(messages: &[ChatMessage], prompt_version: u32, judge_model: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Distinct namespace tag vs the full-conversation key.
    "session-affinity".hash(&mut hasher);
    prompt_version.hash(&mut hasher);
    judge_model.hash(&mut hasher);
    if let Some(sys) = messages.first().filter(|m| m.role == "system") {
        sys.role.hash(&mut hasher);
        message_text_for_prompt(sys).hash(&mut hasher);
    }
    if let Some(first_user) = messages.iter().find(|m| m.role == "user") {
        first_user.role.hash(&mut hasher);
        message_text_for_prompt(first_user).hash(&mut hasher);
    }
    hasher.finish()
}

struct Entry {
    difficulty: f32,
    /// For TTL expiry only — never used as identity (see `seq`).
    inserted: Instant,
    /// Insertion identity: assigned from a counter under the cache mutex, so
    /// it is unique even when two rapid inserts land on the same `Instant`
    /// tick (Instants are monotonic but not unique, and a tag that matched a
    /// superseded insert could evict a freshly ratcheted entry).
    seq: u64,
}

/// A bounded FIFO+TTL cache. Not a strict LRU (no per-get reordering) — the
/// point is only to bound memory and skip repeat judging.
///
/// The insertion-order queue tags each entry with a per-insert sequence
/// number (assigned under the mutex, so unique — unlike an `Instant`, which
/// two rapid inserts can share). A queue entry is *stale* when the map no
/// longer holds a matching entry for its key (expired-and-removed by `get`,
/// evicted, or superseded by a newer insert of the same key). Stale entries
/// are skipped — never treated as the live key — during capacity eviction,
/// and the queue itself is compacted once stale entries outnumber live ones,
/// so BOTH structures stay bounded even under expire/reinsert churn.
pub struct DifficultyCache {
    inner: Mutex<Inner>,
    capacity: usize,
    ttl: Duration,
}

struct Inner {
    map: HashMap<u64, Entry>,
    /// `(cache key, insertion seq)` — the seq says WHICH insert of that key
    /// this tag belongs to.
    order: VecDeque<(u64, u64)>,
    next_seq: u64,
}

impl DifficultyCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
                next_seq: 0,
            }),
            capacity: capacity.max(1),
            ttl,
        }
    }

    /// Fetch a fresh difficulty, or `None` on miss/expiry. Expiry removes the
    /// map entry; its queue entry goes stale and is cleaned up lazily by
    /// `insert`'s eviction/compaction.
    pub fn get(&self, key: u64) -> Option<f32> {
        let mut inner = self.inner.lock().ok()?;
        match inner.map.get(&key) {
            Some(e) if e.inserted.elapsed() < self.ttl => Some(e.difficulty),
            Some(_) => {
                inner.map.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Store a genuine judge result. Evicts the oldest live entry when over
    /// capacity. Re-inserting an existing key overwrites it (how a hard-signal
    /// override ratchets a session pin upward) and stales its old queue entry.
    pub fn insert(&self, key: u64, difficulty: f32) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let Inner {
            map,
            order,
            next_seq,
        } = &mut *inner;
        let seq = *next_seq;
        *next_seq += 1;
        map.insert(
            key,
            Entry {
                difficulty,
                inserted: Instant::now(),
                seq,
            },
        );
        order.push_back((key, seq));

        // Capacity eviction: pop from the front until the map fits, removing
        // only entries whose seq still matches (a mismatch is a stale tag for
        // an expired/superseded insert — dropping it must not evict the key's
        // LIVE entry, whose own tag sits further back in the queue).
        while map.len() > self.capacity {
            match order.pop_front() {
                Some((old, tag_seq)) => {
                    if map.get(&old).is_some_and(|e| e.seq == tag_seq) {
                        map.remove(&old);
                    }
                }
                None => break,
            }
        }

        // Bound the queue itself: expire-then-reinsert churn (and ratchet
        // overwrites) accumulate stale tags that eviction alone never pops
        // while the map stays small. Compact once they can outnumber the live
        // entries; retain keeps exactly the tags the map still vouches for.
        if order.len() > self.capacity.saturating_mul(2) {
            order.retain(|(k, tag_seq)| map.get(k).is_some_and(|e| e.seq == *tag_seq));
        }
    }

    /// Test-only view of the insertion-queue length, to assert boundedness.
    #[cfg(test)]
    fn queue_len(&self) -> usize {
        self.inner.lock().map(|i| i.order.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::test_msg as msg;

    #[test]
    fn hit_and_miss() {
        let c = DifficultyCache::new(8, Duration::from_secs(60));
        let k = difficulty_cache_key(&[msg("user", "hi")], 1, "chalk/haiku");
        assert_eq!(c.get(k), None);
        c.insert(k, 0.25);
        assert_eq!(c.get(k), Some(0.25));
    }

    #[test]
    fn key_changes_with_prompt_version() {
        let m = [msg("user", "hi")];
        let k1 = difficulty_cache_key(&m, 1, "j");
        let k2 = difficulty_cache_key(&m, 2, "j");
        assert_ne!(k1, k2);
    }

    #[test]
    fn ttl_expiry() {
        let c = DifficultyCache::new(8, Duration::from_millis(0));
        let k = difficulty_cache_key(&[msg("user", "x")], 1, "j");
        c.insert(k, 0.5);
        // zero TTL → immediately stale.
        assert_eq!(c.get(k), None);
    }

    #[test]
    fn capacity_bound() {
        let c = DifficultyCache::new(2, Duration::from_secs(60));
        c.insert(1, 0.1);
        c.insert(2, 0.2);
        c.insert(3, 0.3); // evicts key 1
        assert_eq!(c.get(1), None);
        assert_eq!(c.get(2), Some(0.2));
        assert_eq!(c.get(3), Some(0.3));
    }

    #[test]
    fn overwrite_ratchets_in_place() {
        let c = DifficultyCache::new(8, Duration::from_secs(60));
        c.insert(7, 0.1);
        c.insert(7, 1.0);
        assert_eq!(c.get(7), Some(1.0));
    }

    #[test]
    fn expire_reinsert_churn_keeps_queue_bounded() {
        // Zero TTL: every get() expires the entry out of the map, leaving its
        // queue tag stale; each reinsert appends a fresh tag. Without
        // compaction the queue grows one tag per cycle forever.
        let c = DifficultyCache::new(4, Duration::from_millis(0));
        for i in 0..100u64 {
            c.insert(i % 4, 0.5);
            assert_eq!(c.get(i % 4), None); // expired → removed from map
        }
        assert!(
            c.queue_len() <= 8,
            "queue must stay within 2x capacity, got {}",
            c.queue_len()
        );
    }

    #[test]
    fn stale_queue_tags_do_not_evict_live_keys() {
        let c = DifficultyCache::new(2, Duration::from_secs(60));
        // Three inserts of the same key: two stale tags + one live at the back.
        c.insert(1, 0.1);
        c.insert(1, 0.2);
        c.insert(1, 0.3);
        c.insert(2, 0.2);
        // Over capacity: eviction must skip key 1's stale tags and land on its
        // live one — evicting key 1 exactly once, never key 2 or 3 wrongly.
        c.insert(3, 0.3);
        assert_eq!(c.get(1), None, "oldest live entry evicted");
        assert_eq!(c.get(2), Some(0.2));
        assert_eq!(c.get(3), Some(0.3));
    }

    #[test]
    fn session_key_stable_across_turns() {
        let turn1 = [msg("system", "s"), msg("user", "first question")];
        let turn3 = [
            msg("system", "s"),
            msg("user", "first question"),
            msg("assistant", "answer"),
            msg("user", "follow-up"),
        ];
        assert_eq!(
            session_cache_key(&turn1, 1, "j"),
            session_cache_key(&turn3, 1, "j")
        );
        // But the full-conversation key differs turn to turn.
        assert_ne!(
            difficulty_cache_key(&turn1, 1, "j"),
            difficulty_cache_key(&turn3, 1, "j")
        );
        // And the two derivations never collide for the same conversation.
        assert_ne!(
            session_cache_key(&turn1, 1, "j"),
            difficulty_cache_key(&turn1, 1, "j")
        );
    }

    #[test]
    fn session_key_differs_across_sessions() {
        let a = [msg("user", "hello")];
        let b = [msg("user", "prove the halting problem is undecidable")];
        assert_ne!(session_cache_key(&a, 1, "j"), session_cache_key(&b, 1, "j"));
    }
}
