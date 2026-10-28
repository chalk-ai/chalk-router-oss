CREATE TABLE chalk_router.background_response_jobs (
    environment_id TEXT NOT NULL,
    event_id UUID NOT NULL,
    connection_id TEXT,
    upstream_response_id TEXT,
    phase TEXT NOT NULL CHECK (phase IN ('submitting', 'active', 'terminal', 'settled', 'unresolved')),
    payload JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    next_poll_at TIMESTAMPTZ NOT NULL,
    claim_owner UUID,
    claim_until TIMESTAMPTZ,
    poll_attempts BIGINT NOT NULL DEFAULT 0 CHECK (poll_attempts >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    settled_at TIMESTAMPTZ,
    last_error_code TEXT,
    PRIMARY KEY (environment_id, event_id),
    CHECK (phase NOT IN ('active', 'terminal') OR
           (connection_id IS NOT NULL AND upstream_response_id IS NOT NULL))
);
CREATE UNIQUE INDEX background_response_identity_idx
    ON chalk_router.background_response_jobs (environment_id, connection_id, upstream_response_id)
    WHERE upstream_response_id IS NOT NULL;
CREATE INDEX background_response_due_idx
    ON chalk_router.background_response_jobs (environment_id, next_poll_at)
    WHERE phase IN ('submitting', 'active', 'terminal', 'unresolved');
CREATE INDEX background_response_claim_idx
    ON chalk_router.background_response_jobs (claim_until) WHERE claim_owner IS NOT NULL;
CREATE INDEX background_response_retention_idx
    ON chalk_router.background_response_jobs (settled_at) WHERE phase = 'settled';
