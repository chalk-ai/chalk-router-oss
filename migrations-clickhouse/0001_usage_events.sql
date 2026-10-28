-- Router usage ledger, ClickHouse variant.
--
-- Mirrors the chalk-mcp-gateway `migrations-clickhouse/` convention: numbered
-- files, idempotent DDL (`IF NOT EXISTS`), `{db}` substituted by the runner from
-- the usage config, and NO ClickHouse migration-history table — the usage backend
-- applies these at boot. Only used when ROUTER_USAGE_BACKEND=clickhouse.
--
-- Multi-tenant: every row carries `environment_id` (the deployment's
-- CHALK_ENVIRONMENT_ID); reads filter by it.

CREATE TABLE IF NOT EXISTS {db}.usage_events (
    time              DateTime64(3, 'UTC') DEFAULT now64(3),
    environment_id    LowCardinality(String),
    api_key_hash      String,
    usage_pool_id     String,
    provider          LowCardinality(String),
    connection_id     String,
    model             LowCardinality(String),
    prompt_tokens     UInt64,
    completion_tokens UInt64,
    cached_tokens     UInt64,
    total_tokens      UInt64,
    request_count     UInt64
) ENGINE = MergeTree
PARTITION BY toYYYYMMDD(time)
ORDER BY (environment_id, api_key_hash, provider, connection_id, time)
TTL toDateTime(time) + INTERVAL 90 DAY;

-- Daily rollup (budget enforcement / dashboards read this; query with sumMerge).
CREATE TABLE IF NOT EXISTS {db}.usage_daily (
    day               Date,
    environment_id    LowCardinality(String),
    api_key_hash      String,
    usage_pool_id     String,
    provider          LowCardinality(String),
    connection_id     String,
    prompt_tokens     AggregateFunction(sum, UInt64),
    completion_tokens AggregateFunction(sum, UInt64),
    total_tokens      AggregateFunction(sum, UInt64),
    request_count     AggregateFunction(sum, UInt64)
) ENGINE = AggregatingMergeTree
ORDER BY (environment_id, api_key_hash, usage_pool_id, provider, connection_id, day);

CREATE MATERIALIZED VIEW IF NOT EXISTS {db}.usage_daily_mv TO {db}.usage_daily AS
SELECT toDate(time)              AS day,
       environment_id, api_key_hash, usage_pool_id, provider, connection_id,
       sumState(prompt_tokens)     AS prompt_tokens,
       sumState(completion_tokens) AS completion_tokens,
       sumState(total_tokens)      AS total_tokens,
       sumState(request_count)     AS request_count
FROM {db}.usage_events
GROUP BY day, environment_id, api_key_hash, usage_pool_id, provider, connection_id;

-- Monthly rollup.
CREATE TABLE IF NOT EXISTS {db}.usage_monthly (
    month          Date,
    environment_id LowCardinality(String),
    api_key_hash   String,
    usage_pool_id  String,
    provider       LowCardinality(String),
    connection_id  String,
    total_tokens   AggregateFunction(sum, UInt64),
    request_count  AggregateFunction(sum, UInt64)
) ENGINE = AggregatingMergeTree
ORDER BY (environment_id, api_key_hash, usage_pool_id, provider, connection_id, month);

CREATE MATERIALIZED VIEW IF NOT EXISTS {db}.usage_monthly_mv TO {db}.usage_monthly AS
SELECT toStartOfMonth(time)     AS month,
       environment_id, api_key_hash, usage_pool_id, provider, connection_id,
       sumState(total_tokens)  AS total_tokens,
       sumState(request_count) AS request_count
FROM {db}.usage_events
GROUP BY month, environment_id, api_key_hash, usage_pool_id, provider, connection_id;
