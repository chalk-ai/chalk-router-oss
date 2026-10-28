-- no-transaction
-- Resolve credential ownership without a client-supplied environment.
CREATE INDEX CONCURRENTLY idx_router_api_keys_key_hash
    ON chalk_router.api_keys (key_hash);
