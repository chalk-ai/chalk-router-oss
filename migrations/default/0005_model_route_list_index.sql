-- no-transaction
-- Stable newest-first pagination for model routes without blocking writes while
-- the production index is built.
CREATE INDEX CONCURRENTLY idx_router_model_routes_env_created
    ON chalk_router.model_routes (environment_id, created_at DESC, id DESC);
