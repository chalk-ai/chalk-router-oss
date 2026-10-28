//! Valkey-backed prompt cache.
//!
//! The shared backend: every router replica reads and writes the same entries,
//! so a prompt answered once is answered from cache on every replica, and the
//! cache survives restarts and rollouts. (Valkey speaks RESP, so this is the
//! Redis client — the two are wire-compatible, and a Redis URL works here
//! unchanged.)
//!
//! Three properties keep it off the critical path:
//!
//! * **Lazy connect.** The connection is established on first use, not at boot,
//!   so a Valkey that is down during a deploy costs uncached serving rather
//!   than a router that will not start. `ConnectionManager` reconnects
//!   underneath on its own once it exists.
//! * **Deadlined.** Every command runs under [`OP_TIMEOUT`]. A cache lookup that
//!   is slower than the model call it saves is worse than no cache, so a slow
//!   Valkey reads as a miss instead of adding its latency to the request.
//! * **Silent on failure.** Errors are logged and swallowed; `get` reports a
//!   miss and `put` drops the write. Nothing here can fail a request.
//!
//! Entries are stored under `{namespace}:{hex digest}` with a Valkey-native
//! `PX` expiry, so the TTL is enforced by the server and eviction/capacity is
//! Valkey's `maxmemory` policy rather than anything this process tracks.

use std::time::Duration;

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use tokio::sync::OnceCell;

use super::{CacheKey, CachedCompletion, PromptCache};

/// Per-command deadline. Deliberately tight: this is a same-network key/value
/// GET, and anything approaching a model call's latency has already lost the
/// argument for consulting the cache at all.
const OP_TIMEOUT: Duration = Duration::from_millis(250);

/// Deadline for establishing the connection on first use. Longer than
/// [`OP_TIMEOUT`] because it includes DNS and the TCP handshake, but still
/// bounded so a black-holed Valkey address cannot stall the first request that
/// touches it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub struct ValkeyPromptCache {
    client: redis::Client,
    /// Established on first use and shared thereafter. `OnceCell::get_or_try_init`
    /// leaves the cell empty on failure, so a failed connect is retried by the
    /// next request instead of poisoning the cache for the process's lifetime.
    connection: OnceCell<ConnectionManager>,
    /// Key prefix, so one Valkey can serve several router deployments (and be
    /// scanned/flushed per deployment) without them colliding. Tenancy itself
    /// does not depend on it — the environment id is inside the digest.
    namespace: String,
}

impl ValkeyPromptCache {
    /// Validate the URL and build the client. Does not connect — see the
    /// module docs.
    pub fn new(url: &str, namespace: &str) -> Result<Self, redis::RedisError> {
        Ok(Self {
            client: redis::Client::open(url)?,
            connection: OnceCell::new(),
            namespace: namespace.to_string(),
        })
    }

    fn redis_key(&self, key: &CacheKey) -> String {
        format!("{}:{}", self.namespace, key.to_hex())
    }

    /// The shared connection, connecting on first use. `None` (logged) when the
    /// connection cannot be established within [`CONNECT_TIMEOUT`].
    async fn connection(&self) -> Option<ConnectionManager> {
        self.connection
            .get_or_try_init(|| async {
                match tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    ConnectionManager::new(self.client.clone()),
                )
                .await
                {
                    Ok(Ok(manager)) => Ok(manager),
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "prompt cache: valkey connect failed; serving uncached");
                        Err(())
                    }
                    Err(_) => {
                        tracing::warn!(
                            timeout_ms = CONNECT_TIMEOUT.as_millis() as u64,
                            "prompt cache: valkey connect timed out; serving uncached"
                        );
                        Err(())
                    }
                }
            })
            .await
            .ok()
            .cloned()
    }
}

#[async_trait]
impl PromptCache for ValkeyPromptCache {
    fn kind(&self) -> &'static str {
        "valkey"
    }

    async fn get(&self, key: &CacheKey) -> Option<CachedCompletion> {
        let mut conn = self.connection().await?;
        let redis_key = self.redis_key(key);
        let result = tokio::time::timeout(
            OP_TIMEOUT,
            redis::cmd("GET")
                .arg(&redis_key)
                .query_async::<Option<Vec<u8>>>(&mut conn),
        )
        .await;
        let bytes = match result {
            Ok(Ok(Some(bytes))) => bytes,
            Ok(Ok(None)) => return None,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "prompt cache: valkey GET failed; treating as a miss");
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = OP_TIMEOUT.as_millis() as u64,
                    "prompt cache: valkey GET timed out; treating as a miss"
                );
                return None;
            }
        };
        match CachedCompletion::decode(&bytes) {
            Some(value) => Some(value),
            None => {
                // A value written by a different (or corrupted) encoding. Read
                // it as a miss; the store that follows overwrites it.
                tracing::warn!("prompt cache: undecodable valkey entry; treating as a miss");
                None
            }
        }
    }

    async fn put(&self, key: &CacheKey, value: CachedCompletion, ttl: Duration) {
        let Some(mut conn) = self.connection().await else {
            return;
        };
        let redis_key = self.redis_key(key);
        // `PX` (milliseconds) rather than `EX`: sub-second TTLs are meaningless
        // for this cache, but rounding a sub-second configured TTL to zero
        // would mean SET-without-expiry, i.e. an entry that never expires.
        let ttl_ms = ttl.as_millis().max(1).min(u64::MAX as u128) as u64;
        let result = tokio::time::timeout(
            OP_TIMEOUT,
            redis::cmd("SET")
                .arg(&redis_key)
                .arg(value.encode())
                .arg("PX")
                .arg(ttl_ms)
                .query_async::<()>(&mut conn),
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "prompt cache: valkey SET failed; entry not cached")
            }
            Err(_) => tracing::warn!(
                timeout_ms = OP_TIMEOUT.as_millis() as u64,
                "prompt cache: valkey SET timed out; entry not cached"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_url_that_is_not_a_redis_url() {
        assert!(ValkeyPromptCache::new("not a url", "crc").is_err());
        assert!(ValkeyPromptCache::new("postgres://host/db", "crc").is_err());
    }

    #[test]
    fn accepts_redis_and_valkey_urls() {
        assert!(ValkeyPromptCache::new("redis://valkey.internal:6379", "crc").is_ok());
        assert!(ValkeyPromptCache::new("redis://valkey.internal:6379/2", "crc").is_ok());
    }

    #[test]
    fn keys_are_namespaced_hex_digests() {
        let cache = ValkeyPromptCache::new("redis://localhost:6379", "chalk-router:pc").unwrap();
        let key = CacheKey([0xab; 32]);
        assert_eq!(
            cache.redis_key(&key),
            format!("chalk-router:pc:{}", "ab".repeat(32))
        );
    }

    #[tokio::test]
    async fn an_unreachable_valkey_reads_as_a_miss_and_never_panics() {
        // Port 1 is reserved and closed: the connect fails fast rather than
        // hanging, and both operations must degrade silently.
        let cache = ValkeyPromptCache::new("redis://127.0.0.1:1", "crc").unwrap();
        let key = CacheKey([7; 32]);
        assert!(cache.get(&key).await.is_none());
        cache
            .put(
                &key,
                CachedCompletion::new(
                    b"{}".to_vec(),
                    crate::prompt_cache::PayloadKind::Json,
                    "m".to_string(),
                ),
                Duration::from_secs(60),
            )
            .await;
        assert!(cache.get(&key).await.is_none());
    }
}
