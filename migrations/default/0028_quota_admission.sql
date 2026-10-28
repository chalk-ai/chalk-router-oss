-- Fold bucket refill and deduction into one write on admitted requests.
-- Settlement already prelocks all states since 0025. Complete the same
-- hierarchy for first-use unique-index conflicts and janitor receipt deletion.
-- Renewal touches leases only and never follows them by acquiring state locks.
CREATE OR REPLACE FUNCTION chalk_router.acquire_quota(
    p_actions JSONB,
    p_permit_id UUID,
    p_lease_ttl_seconds BIGINT,
    p_reservation_ttl_seconds BIGINT,
    p_environment_id TEXT DEFAULT NULL,
    p_event_id UUID DEFAULT NULL,
    p_failed_amount BIGINT DEFAULT NULL
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
    IF (p_environment_id IS NULL) <> (p_event_id IS NULL) THEN
        RAISE EXCEPTION 'background ownership requires both environment and event';
    END IF;
    IF p_event_id IS NOT NULL THEN
        PERFORM 1 FROM chalk_router.background_response_jobs
        WHERE environment_id = p_environment_id AND event_id = p_event_id
          AND phase = 'submitting' FOR UPDATE;
        IF NOT FOUND THEN RAISE EXCEPTION 'background submission intent is missing'; END IF;
        IF EXISTS (SELECT 1 FROM jsonb_array_elements(p_actions) AS item
                   WHERE item->>'environment_id' IS DISTINCT FROM p_environment_id)
        THEN RAISE EXCEPTION 'background ownership crossed an environment boundary'; END IF;
    END IF;
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
    ORDER BY item.policy_id
    ON CONFLICT (policy_id) DO NOTHING;
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
                  AND background_event_id IS NULL
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
        -- Rejected acquisitions still persist refill, but never deduct any
        -- action. Successful acquisitions perform this write with deduction.
        UPDATE chalk_router.quota_states AS state
        SET balance = LEAST(item.burst::NUMERIC,
                state.balance + GREATEST(EXTRACT(EPOCH FROM (v_now - state.updated_at)), 0)
                * item.limit_value::NUMERIC / 60),
            updated_at = v_now
        FROM jsonb_to_recordset(p_actions) AS item(
            policy_id TEXT, algorithm TEXT, burst BIGINT, limit_value BIGINT)
        WHERE state.policy_id = item.policy_id AND item.algorithm = 'token_bucket';
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
            SET balance = LEAST(action.burst::NUMERIC,
                    balance + GREATEST(EXTRACT(EPOCH FROM (v_now - updated_at)), 0)
                    * action.limit_value::NUMERIC / 60) - action.amount,
                updated_at = v_now
            WHERE policy_id = action.policy_id;
        ELSIF action.algorithm = 'fixed_window' THEN
            UPDATE chalk_router.quota_states
            SET used_amount = used_amount + action.amount
            WHERE policy_id = action.policy_id;
        ELSE
            UPDATE chalk_router.quota_states
            SET active_count = active_count + 1
            WHERE policy_id = action.policy_id;
            INSERT INTO chalk_router.quota_leases
                (policy_id, permit_id, expires_at, background_environment_id, background_event_id)
            VALUES (
                action.policy_id,
                p_permit_id,
                v_now + make_interval(secs => p_lease_ttl_seconds::INTEGER),
                p_environment_id,
                p_event_id
            );
        END IF;

        IF action.meter = 'tokens' THEN
            INSERT INTO chalk_router.quota_reservations
                (policy_id, permit_id, reserved_amount, window_start, expires_at,
                 background_environment_id, background_event_id,
                 algorithm, limit_value, burst, failed_amount)
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
                END,
                p_environment_id,
                p_event_id,
                CASE WHEN p_event_id IS NOT NULL THEN action.algorithm END,
                CASE WHEN p_event_id IS NOT NULL THEN action.limit_value END,
                CASE WHEN p_event_id IS NOT NULL THEN action.burst END,
                CASE WHEN p_event_id IS NOT NULL THEN p_failed_amount END
            );
        END IF;
    END LOOP;

    RETURN QUERY SELECT true, v_needs_finish, v_has_concurrency,
                        0::BIGINT, NULL::TEXT, NULL::TEXT, NULL::BIGINT;
END;
$$;

-- A foreground request has two admission phases. Route denial retains the
-- request phase's RPM charge and returns its reconcilable grant to the caller.
-- Preinitialize and prelock their union before either phase to preserve the
-- state-before-receipt hierarchy even when phases list policies in opposite order.
CREATE FUNCTION chalk_router.acquire_request_and_route_quota(
    p_request_actions JSONB,
    p_route_actions JSONB,
    p_request_permit_id UUID,
    p_route_permit_id UUID,
    p_lease_ttl_seconds BIGINT,
    p_reservation_ttl_seconds BIGINT,
    p_route_failed_amount BIGINT
) RETURNS TABLE (request_decision JSONB, route_decision JSONB, route_error TEXT)
LANGUAGE plpgsql AS $$
DECLARE
    v_now TIMESTAMPTZ := clock_timestamp();
    v_request RECORD;
    v_route RECORD;
BEGIN
    IF jsonb_typeof(p_request_actions) <> 'array'
       OR jsonb_typeof(p_route_actions) <> 'array' THEN
        RAISE EXCEPTION 'quota actions must be JSON arrays';
    END IF;
    INSERT INTO chalk_router.quota_states
        (policy_id, balance, updated_at, window_start, used_amount, active_count)
    SELECT item.policy_id,
           CASE WHEN item.algorithm = 'token_bucket' THEN item.burst::NUMERIC END,
           CASE WHEN item.algorithm = 'token_bucket' THEN v_now END,
           CASE WHEN item.algorithm = 'fixed_window' THEN item.window_start END,
           0, 0
    FROM (
        SELECT DISTINCT ON (policy_id) *
        FROM jsonb_to_recordset(p_request_actions || p_route_actions) AS a(
            policy_id TEXT, algorithm TEXT, burst BIGINT, window_start TIMESTAMPTZ)
        ORDER BY policy_id
    ) AS item
    ORDER BY item.policy_id
    ON CONFLICT (policy_id) DO NOTHING;

    PERFORM 1 FROM chalk_router.quota_states
    WHERE policy_id IN (
        SELECT item->>'policy_id'
        FROM jsonb_array_elements(p_request_actions || p_route_actions) AS item
    ) ORDER BY policy_id FOR UPDATE;

    SELECT * INTO v_request FROM chalk_router.acquire_quota(
        p_request_actions, p_request_permit_id,
        p_lease_ttl_seconds, p_reservation_ttl_seconds);
    IF NOT v_request.allowed THEN
        RETURN QUERY SELECT to_jsonb(v_request), NULL::JSONB, NULL::TEXT;
        RETURN;
    END IF;
    BEGIN
        SELECT * INTO v_route FROM chalk_router.acquire_quota(
            p_route_actions, p_route_permit_id,
            p_lease_ttl_seconds, p_reservation_ttl_seconds,
            NULL::TEXT, NULL::UUID, p_route_failed_amount);
    EXCEPTION WHEN OTHERS THEN
        -- Roll back only the route phase. A definite route backend failure
        -- must retain the request charge, matching separate admissions. Query
        -- cancellation still aborts normally; clients never replay admission.
        RETURN QUERY SELECT to_jsonb(v_request), NULL::JSONB, SQLSTATE;
        RETURN;
    END;
    RETURN QUERY SELECT to_jsonb(v_request), to_jsonb(v_route), NULL::TEXT;
END;
$$;
