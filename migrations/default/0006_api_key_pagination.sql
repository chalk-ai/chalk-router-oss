-- no-transaction
-- Supports the stable newest-first cursor used by ListKeys.
-- key_hash is the tie-breaker so a page boundary cannot skip keys created in
-- the same timestamp tick.
CREATE INDEX CONCURRENTLY idx_router_api_keys_page
    ON chalk_router.api_keys (environment_id, created_at DESC, key_hash DESC);
