CREATE UNIQUE INDEX uq_router_usage_pools_name
    ON chalk_router.usage_pools (environment_id, name);
