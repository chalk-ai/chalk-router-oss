-- Router core schema (default-pool DB). Applied once per database by
-- `openai-server migrate`; the app never creates tables at boot.
-- Multi-tenant: every row is scoped by environment_id; one process serves one
-- environment (CHALK_ENVIRONMENT_ID) and the store layer filters by it.

CREATE SCHEMA IF NOT EXISTS chalk_router;

CREATE TABLE chalk_router.usage_pools (
    environment_id TEXT NOT NULL,
    id             TEXT NOT NULL,
    name           TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (environment_id, id)
);

-- API keys stored HASHED: key_hash = hex(HMAC-SHA256(raw_key, ROUTER_KEY_PEPPER)).
-- The raw secret is shown once at creation, never persisted.
CREATE TABLE chalk_router.api_keys (
    environment_id TEXT NOT NULL,
    key_hash       TEXT NOT NULL,
    key_prefix     TEXT NOT NULL,             -- non-secret display prefix, e.g. "ck-1a2b3c4d"
    description    TEXT NOT NULL DEFAULT '',
    permission     JSONB NOT NULL DEFAULT '{}',
    labels         JSONB NOT NULL DEFAULT '{}',
    cost_tags      JSONB NOT NULL DEFAULT '{}',
    usage_pool_id  TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (environment_id, key_hash),
    -- MATCH SIMPLE: NULL usage_pool_id skips the FK (unassigned keys allowed).
    FOREIGN KEY (environment_id, usage_pool_id)
        REFERENCES chalk_router.usage_pools (environment_id, id)
);
CREATE INDEX idx_router_api_keys_pool ON chalk_router.api_keys (environment_id, usage_pool_id);

-- Provider API keys stored ENCRYPTED (AES-256-GCM under ROUTER_SECRETS_KEK; see
-- src/secrets.rs). Not hashable: they're replayed verbatim to the provider.
CREATE TABLE chalk_router.provider_connections (
    environment_id     TEXT  NOT NULL,
    id                 TEXT  NOT NULL,
    name               TEXT  NOT NULL,
    provider_kind      TEXT  NOT NULL,           -- built-in kind, or "openai-compatible"
    base_url           TEXT,                     -- NULL => built-in registry default
    prefix             TEXT,                     -- e.g. "openai/"; NULL only for 'explicit'
    exposure_policy    TEXT  NOT NULL DEFAULT 'dynamic'
        CHECK (exposure_policy IN ('dynamic','unlisted','explicit')),
    routing_enabled    BOOLEAN NOT NULL DEFAULT true,
    api_key_envelope   BYTEA NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (environment_id, id),
    CHECK (prefix IS NULL OR prefix LIKE '%/')
);
CREATE UNIQUE INDEX uq_router_provider_connections_name
    ON chalk_router.provider_connections (environment_id, name);
CREATE UNIQUE INDEX uq_router_provider_connections_prefix
    ON chalk_router.provider_connections (environment_id, prefix) WHERE prefix IS NOT NULL;
-- One connection per built-in kind (no multi-account). openai-compatible unlimited.
CREATE UNIQUE INDEX uq_router_provider_connections_builtin_kind
    ON chalk_router.provider_connections (environment_id, provider_kind)
    WHERE provider_kind <> 'openai-compatible';

-- Model routes: chalk/* aliases -> (connection, upstream model). connection_id
-- has NO foreign key by design (warning-only integrity): a route may reference a
-- since-deleted connection; dangling refs surface as diagnostics, not errors.
CREATE TABLE chalk_router.model_routes (
    environment_id    TEXT NOT NULL,
    id                TEXT NOT NULL,
    public_model_id   TEXT NOT NULL CHECK (public_model_id LIKE 'chalk/%'),
    connection_id     TEXT NOT NULL,
    upstream_model_id TEXT NOT NULL,
    published         BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (environment_id, id)
);
CREATE UNIQUE INDEX uq_router_model_routes_public_model
    ON chalk_router.model_routes (environment_id, public_model_id);
CREATE INDEX idx_router_model_routes_connection
    ON chalk_router.model_routes (environment_id, connection_id);

-- One fallback policy per environment.
CREATE TABLE chalk_router.fallback_policy (
    environment_id TEXT PRIMARY KEY,
    policy         JSONB NOT NULL DEFAULT '{}'
);

-- Budget config; enforcement lives in the budget backend. A (environment, scope)
-- may hold several rows (e.g. burst + sustained); every enabled budget must pass.
CREATE TABLE chalk_router.budget_limits (
    id             TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL,
    scope          TEXT NOT NULL CHECK (scope IN ('usage_pool','api_key')),
    scope_id       TEXT NOT NULL,             -- usage_pool: pool uuid (as text); api_key: key_hash
    window_type    TEXT NOT NULL DEFAULT 'calendar' CHECK (window_type IN ('calendar','rolling')),
    window_size    INTERVAL NOT NULL,         -- '1 hour','1 day','7 days','1 month','1 year',...
    anchor         TIMESTAMPTZ,               -- calendar origin; NULL = tz midnight/epoch
    timezone       TEXT NOT NULL DEFAULT 'UTC',
    limit_kind     TEXT NOT NULL DEFAULT 'tokens' CHECK (limit_kind IN ('tokens','cost_micro_usd')),
    limit_amount   BIGINT NOT NULL,
    soft_ratio     DOUBLE PRECISION,          -- e.g. 0.8 -> warn at 80%; NULL = hard-only
    enabled        BOOLEAN NOT NULL DEFAULT true,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_router_budget_limits_scope
    ON chalk_router.budget_limits (environment_id, scope, scope_id) WHERE enabled;

-- Plain-Postgres usage ledger + rate-limit store (the deployed default). The
-- TimescaleDB variants in migrations/{usage,ratelimit} target separate DBs and
-- are alternatives; do not apply both to one database. IF NOT EXISTS so the
-- timescale usage file can coexist.

-- Append-only ledger; budgets/histograms aggregate it on the fly. Columns match
-- the TimescaleDB variant so the two are interchangeable.
CREATE TABLE IF NOT EXISTS chalk_router.usage_events (
    time              TIMESTAMPTZ NOT NULL,
    environment_id    TEXT NOT NULL,
    api_key_hash      TEXT NOT NULL,
    usage_pool_id     TEXT,
    provider          TEXT NOT NULL,
    model             TEXT,
    prompt_tokens     BIGINT NOT NULL DEFAULT 0,
    completion_tokens BIGINT NOT NULL DEFAULT 0,
    cached_tokens     BIGINT NOT NULL DEFAULT 0,
    total_tokens      BIGINT NOT NULL DEFAULT 0,
    request_count     BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_router_usage_events_key_time
    ON chalk_router.usage_events (environment_id, api_key_hash, time);   -- per-key usage
CREATE INDEX IF NOT EXISTS idx_router_usage_events_pool_time
    ON chalk_router.usage_events (environment_id, usage_pool_id, time);  -- per-pool budgets
CREATE INDEX IF NOT EXISTS idx_router_usage_events_env_time
    ON chalk_router.usage_events (environment_id, time);                 -- unfiltered histogram

-- Rate-limit policies (plain-Postgres store; see ratelimit::RateLimitPolicy).
CREATE TABLE IF NOT EXISTS chalk_router.rate_limits (
    environment_id TEXT NOT NULL,
    id             TEXT NOT NULL,
    name           TEXT NOT NULL,
    limit_type     TEXT NOT NULL,
    limit_value    BIGINT NOT NULL,
    target_type    TEXT NOT NULL,
    target_value   TEXT NOT NULL,
    scope          TEXT NOT NULL,             -- "usage_pool:<uuid>" or "token:<key_hash>"
    scope_id       TEXT NOT NULL,
    enabled        BOOLEAN NOT NULL DEFAULT true,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (environment_id, id)
);
