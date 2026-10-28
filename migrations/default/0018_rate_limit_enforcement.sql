-- Budget and rate-limit definitions remain in their existing source tables.
-- Make budget identity tenant-scoped so the quota bridge can enforce tenant
-- isolation with a composite foreign key.
ALTER TABLE chalk_router.budget_limits
    DROP CONSTRAINT budget_limits_pkey,
    ADD CONSTRAINT budget_limits_pkey PRIMARY KEY (environment_id, id);

-- A rate limit owns both its subject and its optional traffic selector. NULL
-- connection_id means a global key/pool limit acquired once per client request;
-- a connection selector is acquired for every matching upstream dispatch.
ALTER TABLE chalk_router.rate_limits
    ADD COLUMN connection_id TEXT,
    ADD COLUMN models TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN burst_value BIGINT;

-- Existing definitions only exposed exact connection targets. Refuse to guess
-- if a deployment contains another historical target shape.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM chalk_router.rate_limits
        WHERE target_type <> 'connection'
    ) THEN
        RAISE EXCEPTION 'cannot migrate unsupported rate-limit target type';
    END IF;
    IF EXISTS (
        SELECT 1
        FROM chalk_router.rate_limits AS limits
        LEFT JOIN chalk_router.provider_connections AS connection
          ON connection.environment_id = limits.environment_id
         AND connection.id = limits.target_value
        WHERE connection.id IS NULL
    ) THEN
        RAISE EXCEPTION 'cannot migrate rate limit with a missing connection';
    END IF;
    IF EXISTS (
        SELECT 1 FROM chalk_router.rate_limits
        WHERE limit_type NOT IN
                  ('requests_per_minute', 'tokens_per_minute', 'concurrent_requests')
           OR limit_value <= 0
           OR scope NOT IN ('token', 'api_key', 'usage_pool')
           OR scope_id = ''
    ) THEN
        RAISE EXCEPTION 'cannot migrate unsupported rate-limit definition';
    END IF;
END;
$$;

UPDATE chalk_router.rate_limits
SET connection_id = target_value,
    burst_value = CASE WHEN limit_type = 'concurrent_requests'
                       THEN NULL ELSE limit_value END,
    scope = CASE WHEN scope = 'token' THEN 'api_key' ELSE scope END;

ALTER TABLE chalk_router.rate_limits
    DROP COLUMN target_type,
    DROP COLUMN target_value,
    ADD CONSTRAINT rate_limits_type_check CHECK (
        limit_type IN
            ('requests_per_minute', 'tokens_per_minute', 'concurrent_requests')
    ),
    ADD CONSTRAINT rate_limits_value_check CHECK (limit_value > 0),
    ADD CONSTRAINT rate_limits_burst_check CHECK (
        burst_value IS NULL OR burst_value > 0
    ),
    ADD CONSTRAINT rate_limits_concurrency_burst_check CHECK (
        limit_type <> 'concurrent_requests' OR burst_value IS NULL
    ),
    ADD CONSTRAINT rate_limits_scope_check CHECK (
        scope IN ('api_key', 'usage_pool')
    ),
    ADD CONSTRAINT rate_limits_scope_id_check CHECK (scope_id <> ''),
    ADD CONSTRAINT rate_limits_models_require_connection_check CHECK (
        connection_id IS NOT NULL OR cardinality(models) = 0
    ),
    ADD CONSTRAINT rate_limits_connection_fk
        FOREIGN KEY (environment_id, connection_id)
        REFERENCES chalk_router.provider_connections (environment_id, id)
        ON DELETE RESTRICT;

-- Admission resolves enabled policies by tenant, subject, and request/dispatch
-- phase. Keep that lookup independent of the total number of definitions.
CREATE INDEX idx_router_rate_limits_active_selector
    ON chalk_router.rate_limits
        (environment_id, scope, scope_id, connection_id)
    WHERE enabled;

-- Pure one-of bridge from a public source definition to the stable internal
-- identity used by mutable quota state.
CREATE TABLE chalk_router.quota_policies (
    id              TEXT PRIMARY KEY,
    environment_id  TEXT NOT NULL,
    rate_limit_id   TEXT,
    budget_limit_id TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT quota_policy_source_check CHECK (
        (rate_limit_id IS NOT NULL AND budget_limit_id IS NULL)
        OR
        (rate_limit_id IS NULL AND budget_limit_id IS NOT NULL)
    ),
    CONSTRAINT quota_policy_rate_limit_unique
        UNIQUE (environment_id, rate_limit_id),
    CONSTRAINT quota_policy_budget_limit_unique
        UNIQUE (environment_id, budget_limit_id),
    CONSTRAINT quota_policy_rate_limit_fk
        FOREIGN KEY (environment_id, rate_limit_id)
        REFERENCES chalk_router.rate_limits (environment_id, id)
        ON DELETE CASCADE,
    CONSTRAINT quota_policy_budget_limit_fk
        FOREIGN KEY (environment_id, budget_limit_id)
        REFERENCES chalk_router.budget_limits (environment_id, id)
        ON DELETE CASCADE
);

INSERT INTO chalk_router.quota_policies
    (id, environment_id, rate_limit_id, created_at)
SELECT 'rate:' || config.environment_id || ':' || config.id,
       config.environment_id,
       config.id,
       config.created_at
FROM chalk_router.rate_limits AS config;

INSERT INTO chalk_router.quota_policies
    (id, environment_id, budget_limit_id, created_at)
SELECT 'budget:' || limits.environment_id || ':' || limits.id,
       limits.environment_id,
       limits.id,
       limits.created_at
FROM chalk_router.budget_limits AS limits
WHERE limits.window_type = 'calendar'
  AND limits.limit_kind = 'tokens'
  AND limits.limit_amount > 0
  AND limits.scope IN ('api_key', 'usage_pool')
  AND limits.scope_id <> '';

CREATE TABLE chalk_router.quota_states (
    policy_id    TEXT PRIMARY KEY
                 REFERENCES chalk_router.quota_policies (id) ON DELETE CASCADE,
    balance      NUMERIC,
    updated_at   TIMESTAMPTZ,
    window_start TIMESTAMPTZ,
    used_amount  BIGINT NOT NULL DEFAULT 0 CHECK (used_amount >= 0),
    active_count BIGINT NOT NULL DEFAULT 0 CHECK (active_count >= 0)
);

CREATE TABLE chalk_router.quota_reservations (
    policy_id       TEXT NOT NULL
                    REFERENCES chalk_router.quota_policies (id) ON DELETE CASCADE,
    permit_id       UUID NOT NULL,
    reserved_amount BIGINT NOT NULL CHECK (reserved_amount >= 0),
    window_start    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (policy_id, permit_id)
);

CREATE INDEX quota_reservations_permit_idx
    ON chalk_router.quota_reservations (permit_id);
CREATE INDEX quota_reservations_expiry_idx
    ON chalk_router.quota_reservations (expires_at);

CREATE TABLE chalk_router.quota_leases (
    policy_id TEXT NOT NULL
              REFERENCES chalk_router.quota_policies (id) ON DELETE CASCADE,
    permit_id UUID NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (policy_id, permit_id)
);

CREATE INDEX quota_leases_permit_idx
    ON chalk_router.quota_leases (permit_id);
CREATE INDEX quota_leases_expiry_idx
    ON chalk_router.quota_leases (policy_id, expires_at);

-- Atomically acquire already-compiled actions. This function deliberately does
-- not query rate_limits, budget_limits, connections, models, keys, or pools.
-- Definition matching and calendar arithmetic happen in QuotaPolicyResolver.
CREATE FUNCTION chalk_router.acquire_quota(
    p_actions JSONB,
    p_permit_id UUID,
    p_lease_ttl_seconds BIGINT,
    p_reservation_ttl_seconds BIGINT
) RETURNS TABLE (
    allowed BOOLEAN,
    needs_finish BOOLEAN,
    has_concurrency BOOLEAN,
    retry_after_ms BIGINT,
    rejected_source TEXT,
    rejected_meter TEXT,
    rejected_limit BIGINT
) LANGUAGE plpgsql AS $$
DECLARE
    action RECORD;
    state_row RECORD;
    v_now TIMESTAMPTZ := clock_timestamp();
    v_rejected BOOLEAN := false;
    v_needs_finish BOOLEAN := false;
    v_has_concurrency BOOLEAN := false;
    v_retry_ms BIGINT := 0;
    v_policy_retry_ms BIGINT;
    v_refilled NUMERIC;
    v_expired BIGINT;
    v_rejected_source TEXT;
    v_rejected_meter TEXT;
    v_rejected_limit BIGINT;
BEGIN
    IF jsonb_typeof(p_actions) <> 'array' THEN
        RAISE EXCEPTION 'quota actions must be a JSON array';
    END IF;

    IF (
        SELECT count(*) <> count(DISTINCT item.policy_id)
        FROM jsonb_to_recordset(p_actions) AS item(policy_id TEXT)
    ) THEN
        RAISE EXCEPTION 'quota actions contain duplicate policy ids';
    END IF;

    INSERT INTO chalk_router.quota_states
        (policy_id, balance, updated_at, window_start, used_amount, active_count)
    SELECT item.policy_id,
           CASE WHEN item.algorithm = 'token_bucket' THEN item.burst::NUMERIC END,
           CASE WHEN item.algorithm = 'token_bucket' THEN v_now END,
           CASE WHEN item.algorithm = 'fixed_window' THEN item.window_start END,
           0,
           0
    FROM jsonb_to_recordset(p_actions) AS item(
        policy_id TEXT,
        algorithm TEXT,
        burst BIGINT,
        window_start TIMESTAMPTZ
    )
    ON CONFLICT (policy_id) DO NOTHING;

    -- Lock and evaluate in stable policy order. Refill, window reset, and lease
    -- expiry are safe bookkeeping even when a later action rejects the batch.
    FOR action IN
        SELECT *
        FROM jsonb_to_recordset(p_actions) AS item(
            policy_id TEXT,
            source TEXT,
            algorithm TEXT,
            meter TEXT,
            limit_value BIGINT,
            burst BIGINT,
            amount BIGINT,
            window_start TIMESTAMPTZ,
            window_end TIMESTAMPTZ
        )
        ORDER BY policy_id
    LOOP
        IF action.limit_value <= 0 OR action.amount < 0 THEN
            RAISE EXCEPTION 'invalid compiled quota action for %', action.policy_id;
        END IF;

        SELECT * INTO state_row
        FROM chalk_router.quota_states
        WHERE policy_id = action.policy_id
        FOR UPDATE;

        IF action.algorithm = 'token_bucket' THEN
            IF action.burst IS NULL OR action.burst <= 0 THEN
                RAISE EXCEPTION 'token bucket % requires a positive burst', action.policy_id;
            END IF;
            v_refilled := LEAST(
                action.burst::NUMERIC,
                state_row.balance
                    + GREATEST(EXTRACT(EPOCH FROM (v_now - state_row.updated_at)), 0)
                    * action.limit_value::NUMERIC / 60
            );
            UPDATE chalk_router.quota_states
            SET balance = v_refilled, updated_at = v_now
            WHERE policy_id = action.policy_id;

            IF v_refilled < action.amount THEN
                v_policy_retry_ms := CEIL(
                    (action.amount - v_refilled) * 60000
                    / action.limit_value::NUMERIC
                )::BIGINT;
                v_rejected := true;
            ELSE
                v_policy_retry_ms := 0;
            END IF;
        ELSIF action.algorithm = 'fixed_window' THEN
            IF action.window_start IS NULL OR action.window_end IS NULL
               OR action.window_end <= action.window_start THEN
                RAISE EXCEPTION 'fixed window % requires valid boundaries', action.policy_id;
            END IF;
            IF state_row.window_start IS DISTINCT FROM action.window_start THEN
                UPDATE chalk_router.quota_states
                SET window_start = action.window_start, used_amount = 0
                WHERE policy_id = action.policy_id
                RETURNING * INTO state_row;
            END IF;
            IF action.amount > action.limit_value
               OR state_row.used_amount > action.limit_value - action.amount THEN
                v_policy_retry_ms := GREATEST(
                    CEIL(EXTRACT(EPOCH FROM (action.window_end - v_now)) * 1000)::BIGINT,
                    1
                );
                v_rejected := true;
            ELSE
                v_policy_retry_ms := 0;
            END IF;
        ELSIF action.algorithm = 'concurrency' THEN
            WITH expired AS (
                DELETE FROM chalk_router.quota_leases
                WHERE policy_id = action.policy_id AND expires_at <= v_now
                RETURNING 1
            ) SELECT count(*) INTO v_expired FROM expired;
            IF v_expired > 0 THEN
                UPDATE chalk_router.quota_states
                SET active_count = GREATEST(active_count - v_expired, 0)
                WHERE policy_id = action.policy_id
                RETURNING * INTO state_row;
            END IF;
            v_needs_finish := true;
            v_has_concurrency := true;
            IF state_row.active_count >= action.limit_value THEN
                SELECT COALESCE(
                    CEIL(EXTRACT(EPOCH FROM (MIN(expires_at) - v_now)) * 1000)::BIGINT,
                    1000
                ) INTO v_policy_retry_ms
                FROM chalk_router.quota_leases
                WHERE policy_id = action.policy_id;
                v_policy_retry_ms := GREATEST(v_policy_retry_ms, 1);
                v_rejected := true;
            ELSE
                v_policy_retry_ms := 0;
            END IF;
        ELSE
            RAISE EXCEPTION 'unsupported quota algorithm %', action.algorithm;
        END IF;

        IF action.meter = 'tokens' THEN
            v_needs_finish := true;
        END IF;

        IF v_policy_retry_ms >= v_retry_ms AND v_rejected THEN
            v_retry_ms := GREATEST(v_policy_retry_ms, 1);
            v_rejected_source := action.source;
            v_rejected_meter := action.meter;
            v_rejected_limit := action.limit_value;
        END IF;
    END LOOP;

    IF v_rejected THEN
        RETURN QUERY SELECT false, false, false, v_retry_ms,
                            v_rejected_source, v_rejected_meter,
                            v_rejected_limit;
        RETURN;
    END IF;

    FOR action IN
        SELECT *
        FROM jsonb_to_recordset(p_actions) AS item(
            policy_id TEXT,
            source TEXT,
            algorithm TEXT,
            meter TEXT,
            limit_value BIGINT,
            burst BIGINT,
            amount BIGINT,
            window_start TIMESTAMPTZ,
            window_end TIMESTAMPTZ
        )
        ORDER BY policy_id
    LOOP
        IF action.algorithm = 'token_bucket' THEN
            UPDATE chalk_router.quota_states
            SET balance = balance - action.amount
            WHERE policy_id = action.policy_id;
        ELSIF action.algorithm = 'fixed_window' THEN
            UPDATE chalk_router.quota_states
            SET used_amount = used_amount + action.amount
            WHERE policy_id = action.policy_id;
        ELSE
            UPDATE chalk_router.quota_states
            SET active_count = active_count + 1
            WHERE policy_id = action.policy_id;
            INSERT INTO chalk_router.quota_leases (policy_id, permit_id, expires_at)
            VALUES (
                action.policy_id,
                p_permit_id,
                v_now + make_interval(secs => p_lease_ttl_seconds::INTEGER)
            );
        END IF;

        IF action.meter = 'tokens' THEN
            INSERT INTO chalk_router.quota_reservations
                (policy_id, permit_id, reserved_amount, window_start, expires_at)
            VALUES (
                action.policy_id,
                p_permit_id,
                action.amount,
                action.window_start,
                CASE WHEN action.algorithm = 'fixed_window'
                     THEN LEAST(
                         action.window_end,
                         v_now + make_interval(secs => p_reservation_ttl_seconds::INTEGER)
                     )
                     ELSE v_now + make_interval(secs => p_reservation_ttl_seconds::INTEGER)
                END
            );
        END IF;
    END LOOP;

    RETURN QUERY SELECT true, v_needs_finish, v_has_concurrency,
                        0::BIGINT, NULL::TEXT, NULL::TEXT, NULL::BIGINT;
END;
$$;

-- Reconcile token reservations and release concurrency leases. The original
-- compiled actions are supplied by the permit, so settlement does not load
-- mutable control-plane definitions. Deleting the reservation/lease first
-- makes repeated finish calls idempotent.
CREATE FUNCTION chalk_router.finish_quota(
    p_actions JSONB,
    p_permit_id UUID,
    p_actual_amount BIGINT
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    action RECORD;
    reservation RECORD;
    state_row RECORD;
    v_now TIMESTAMPTZ := clock_timestamp();
    v_refilled NUMERIC;
    v_actual BIGINT := GREATEST(COALESCE(p_actual_amount, 0), 0);
BEGIN
    IF p_actual_amount IS NOT NULL THEN
        FOR action IN
            SELECT *
            FROM jsonb_to_recordset(p_actions) AS item(
                policy_id TEXT,
                algorithm TEXT,
                meter TEXT,
                limit_value BIGINT,
                burst BIGINT,
                window_start TIMESTAMPTZ
            )
            WHERE meter = 'tokens'
            ORDER BY policy_id
        LOOP
            DELETE FROM chalk_router.quota_reservations
            WHERE policy_id = action.policy_id AND permit_id = p_permit_id
            RETURNING reserved_amount, window_start INTO reservation;
            IF NOT FOUND THEN
                CONTINUE;
            END IF;

            SELECT * INTO state_row
            FROM chalk_router.quota_states
            WHERE policy_id = action.policy_id
            FOR UPDATE;
            IF NOT FOUND THEN
                CONTINUE;
            END IF;

            IF action.algorithm = 'token_bucket' THEN
                v_refilled := LEAST(
                    action.burst::NUMERIC,
                    state_row.balance
                        + GREATEST(EXTRACT(EPOCH FROM (v_now - state_row.updated_at)), 0)
                        * action.limit_value::NUMERIC / 60
                );
                UPDATE chalk_router.quota_states
                SET balance = LEAST(
                        action.burst::NUMERIC,
                        v_refilled + reservation.reserved_amount - v_actual
                    ),
                    updated_at = v_now
                WHERE policy_id = action.policy_id;
            ELSIF action.algorithm = 'fixed_window'
                  AND state_row.window_start IS NOT DISTINCT FROM reservation.window_start THEN
                UPDATE chalk_router.quota_states
                SET used_amount = LEAST(
                    GREATEST(
                        used_amount::NUMERIC - reservation.reserved_amount + v_actual,
                        0
                    ),
                    9223372036854775807
                )::BIGINT
                WHERE policy_id = action.policy_id;
            END IF;
        END LOOP;
    END IF;

    -- NULL actual retains the conservative charge but still removes the
    -- reconciliation record, matching reservation-expiry behavior.
    DELETE FROM chalk_router.quota_reservations
    WHERE permit_id = p_permit_id;

    -- Use the same state-then-lease lock order as acquisition and cleanup so a
    -- finish racing either path cannot form a deadlock cycle.
    FOR action IN
        SELECT *
        FROM jsonb_to_recordset(p_actions) AS item(
            policy_id TEXT,
            algorithm TEXT
        )
        WHERE algorithm = 'concurrency'
        ORDER BY policy_id
    LOOP
        PERFORM 1 FROM chalk_router.quota_states
        WHERE policy_id = action.policy_id
        FOR UPDATE;
        DELETE FROM chalk_router.quota_leases
        WHERE policy_id = action.policy_id AND permit_id = p_permit_id;
        IF FOUND THEN
            UPDATE chalk_router.quota_states
            SET active_count = GREATEST(active_count - 1, 0)
            WHERE policy_id = action.policy_id;
        END IF;
    END LOOP;
END;
$$;

CREATE FUNCTION chalk_router.renew_quota(
    p_permit_id UUID,
    p_lease_ttl_seconds BIGINT
) RETURNS VOID LANGUAGE sql AS $$
    UPDATE chalk_router.quota_leases
    SET expires_at = clock_timestamp()
        + make_interval(secs => p_lease_ttl_seconds::INTEGER)
    WHERE permit_id = p_permit_id;
$$;

-- Bounded cleanup for policies that receive no further acquisitions.
CREATE FUNCTION chalk_router.cleanup_quota_runtime(p_batch_size BIGINT)
RETURNS TABLE (reservations_deleted BIGINT, leases_deleted BIGINT)
LANGUAGE plpgsql AS $$
DECLARE
    policy RECORD;
    v_reservations BIGINT;
    v_leases BIGINT := 0;
    v_deleted BIGINT;
BEGIN
    WITH candidates AS (
        SELECT policy_id, permit_id
        FROM chalk_router.quota_reservations
        WHERE expires_at <= clock_timestamp()
        ORDER BY expires_at
        LIMIT GREATEST(p_batch_size, 0)
    ), deleted AS (
        DELETE FROM chalk_router.quota_reservations AS reservation
        USING candidates
        WHERE reservation.policy_id = candidates.policy_id
          AND reservation.permit_id = candidates.permit_id
        RETURNING 1
    )
    SELECT count(*) INTO v_reservations FROM deleted;

    FOR policy IN
        SELECT policy_id, permit_id
        FROM chalk_router.quota_leases
        WHERE expires_at <= clock_timestamp()
        ORDER BY policy_id, expires_at, permit_id
        LIMIT GREATEST(p_batch_size, 0)
    LOOP
        PERFORM 1 FROM chalk_router.quota_states
        WHERE policy_id = policy.policy_id
        FOR UPDATE;
        DELETE FROM chalk_router.quota_leases
        WHERE policy_id = policy.policy_id
          AND permit_id = policy.permit_id
          AND expires_at <= clock_timestamp();
        GET DIAGNOSTICS v_deleted = ROW_COUNT;
        IF v_deleted > 0 THEN
            UPDATE chalk_router.quota_states
            SET active_count = GREATEST(active_count - 1, 0)
            WHERE policy_id = policy.policy_id;
            v_leases := v_leases + 1;
        END IF;
    END LOOP;

    RETURN QUERY SELECT v_reservations, v_leases;
END;
$$;
