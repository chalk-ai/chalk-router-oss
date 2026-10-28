-- Perf: composite indexes to speed up paginated/ordered listing of rate-limit
-- policies and usage pools (both queried as
-- `WHERE environment_id = $1 ORDER BY created_at ASC`).

CREATE INDEX IF NOT EXISTS idx_router_rate_limits_env_created
    ON chalk_router.rate_limits (environment_id, created_at);

CREATE INDEX IF NOT EXISTS idx_router_usage_pools_env_created
    ON chalk_router.usage_pools (environment_id, created_at);
