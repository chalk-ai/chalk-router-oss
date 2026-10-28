-- no-transaction
-- Cover newest-first provider cursor pagination. The leading environment_id
-- keeps each tenant's scan contiguous.

CREATE INDEX CONCURRENTLY idx_router_provider_connections_env_created
    ON chalk_router.provider_connections (environment_id, created_at DESC, id DESC);
