ALTER TABLE chalk_router.quota_reservations
    ADD COLUMN background_environment_id TEXT,
    ADD COLUMN background_event_id UUID,
    ADD COLUMN algorithm TEXT,
    ADD COLUMN limit_value BIGINT,
    ADD COLUMN burst BIGINT,
    ADD COLUMN failed_amount BIGINT CHECK (failed_amount >= 0),
    ADD CONSTRAINT quota_reservations_background_job_fk
        FOREIGN KEY (background_environment_id, background_event_id)
        REFERENCES chalk_router.background_response_jobs (environment_id, event_id)
        MATCH FULL ON DELETE RESTRICT,
    ADD CONSTRAINT quota_reservations_background_settlement_check CHECK (
        background_event_id IS NULL OR (
            algorithm IS NOT NULL AND algorithm IN ('token_bucket', 'fixed_window')
            AND limit_value IS NOT NULL AND limit_value > 0
            AND (algorithm <> 'token_bucket' OR (burst IS NOT NULL AND burst > 0))
        )
    );
ALTER TABLE chalk_router.quota_leases
    ADD COLUMN background_environment_id TEXT,
    ADD COLUMN background_event_id UUID,
    ADD CONSTRAINT quota_leases_background_job_fk
        FOREIGN KEY (background_environment_id, background_event_id)
        REFERENCES chalk_router.background_response_jobs (environment_id, event_id)
        MATCH FULL ON DELETE RESTRICT;
CREATE INDEX quota_reservations_background_job_idx
    ON chalk_router.quota_reservations (background_environment_id, background_event_id, permit_id)
    WHERE background_event_id IS NOT NULL;
CREATE INDEX quota_leases_background_job_idx
    ON chalk_router.quota_leases (background_environment_id, background_event_id, permit_id)
    WHERE background_event_id IS NOT NULL;

DROP FUNCTION chalk_router.acquire_quota(JSONB, UUID, BIGINT, BIGINT);
CREATE FUNCTION chalk_router.acquire_quota(
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

CREATE OR REPLACE FUNCTION chalk_router.cleanup_quota_runtime(p_batch_size BIGINT)
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
          AND background_event_id IS NULL
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
          AND background_event_id IS NULL
        ORDER BY policy_id, expires_at, permit_id
        LIMIT GREATEST(p_batch_size, 0)
    LOOP
        PERFORM 1 FROM chalk_router.quota_states
        WHERE policy_id = policy.policy_id
        FOR UPDATE;
        DELETE FROM chalk_router.quota_leases
        WHERE policy_id = policy.policy_id
          AND permit_id = policy.permit_id
          AND expires_at <= clock_timestamp()
          AND background_event_id IS NULL;
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

DROP FUNCTION chalk_router.finish_quota(JSONB, UUID, BIGINT);
CREATE FUNCTION chalk_router.finish_quota(
    p_actions JSONB, p_permit_id UUID, p_actual_amount BIGINT, p_failed BOOLEAN DEFAULT FALSE
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    reservation RECORD;
    state_row RECORD;
    lease_row RECORD;
    v_now TIMESTAMPTZ := clock_timestamp();
    v_refilled NUMERIC;
    v_actual BIGINT;
BEGIN
    PERFORM 1 FROM chalk_router.quota_states
    WHERE policy_id IN (
        SELECT policy_id FROM chalk_router.quota_reservations WHERE permit_id = p_permit_id
        UNION
        SELECT policy_id FROM chalk_router.quota_leases WHERE permit_id = p_permit_id
    ) ORDER BY policy_id FOR UPDATE;
    FOR reservation IN
        SELECT stored.policy_id, stored.reserved_amount, stored.window_start,
               CASE WHEN stored.background_event_id IS NOT NULL THEN stored.algorithm
                    ELSE action.algorithm END AS algorithm,
               CASE WHEN stored.background_event_id IS NOT NULL THEN stored.limit_value
                    ELSE action.limit_value END AS limit_value,
               CASE WHEN stored.background_event_id IS NOT NULL THEN stored.burst
                    ELSE action.burst END AS burst,
               CASE WHEN p_failed AND stored.background_event_id IS NOT NULL
                    THEN stored.failed_amount ELSE p_actual_amount END AS actual_amount
        FROM chalk_router.quota_reservations AS stored
        LEFT JOIN jsonb_to_recordset(p_actions) AS action(
            policy_id TEXT, algorithm TEXT, limit_value BIGINT, burst BIGINT
        ) ON action.policy_id = stored.policy_id
        WHERE stored.permit_id = p_permit_id
        ORDER BY stored.policy_id
    LOOP
        IF reservation.actual_amount IS NULL THEN CONTINUE; END IF;
        v_actual := GREATEST(reservation.actual_amount, 0);
        SELECT * INTO state_row FROM chalk_router.quota_states
        WHERE policy_id = reservation.policy_id;
        IF NOT FOUND THEN CONTINUE; END IF;
        IF reservation.algorithm = 'token_bucket' THEN
            v_refilled := LEAST(
                reservation.burst::NUMERIC,
                state_row.balance
                    + GREATEST(EXTRACT(EPOCH FROM (v_now - state_row.updated_at)), 0)
                    * reservation.limit_value::NUMERIC / 60
            );
            UPDATE chalk_router.quota_states
            SET balance = LEAST(reservation.burst::NUMERIC,
                    v_refilled + reservation.reserved_amount - v_actual),
                updated_at = v_now
            WHERE policy_id = reservation.policy_id;
        ELSIF reservation.algorithm = 'fixed_window'
              AND state_row.window_start IS NOT DISTINCT FROM reservation.window_start THEN
            UPDATE chalk_router.quota_states
            SET used_amount = LEAST(GREATEST(
                    used_amount::NUMERIC - reservation.reserved_amount + v_actual, 0
                ), 9223372036854775807)::BIGINT
            WHERE policy_id = reservation.policy_id;
        END IF;
    END LOOP;
    DELETE FROM chalk_router.quota_reservations WHERE permit_id = p_permit_id;
    FOR lease_row IN
        DELETE FROM chalk_router.quota_leases WHERE permit_id = p_permit_id RETURNING policy_id
    LOOP
        UPDATE chalk_router.quota_states SET active_count = GREATEST(active_count - 1, 0)
        WHERE policy_id = lease_row.policy_id;
    END LOOP;
END;
$$;

CREATE FUNCTION chalk_router.finish_background_response_quota(
    p_environment_id TEXT, p_event_id UUID, p_actual_amount BIGINT, p_failed BOOLEAN
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE owned_permit RECORD;
BEGIN
    PERFORM 1 FROM chalk_router.background_response_jobs
    WHERE environment_id = p_environment_id AND event_id = p_event_id FOR UPDATE;
    PERFORM 1 FROM chalk_router.quota_states
    WHERE policy_id IN (
        SELECT policy_id FROM chalk_router.quota_reservations
        WHERE background_environment_id = p_environment_id AND background_event_id = p_event_id
        UNION
        SELECT policy_id FROM chalk_router.quota_leases
        WHERE background_environment_id = p_environment_id AND background_event_id = p_event_id
    ) ORDER BY policy_id FOR UPDATE;
    FOR owned_permit IN
        SELECT permit_id FROM chalk_router.quota_reservations
        WHERE background_environment_id = p_environment_id AND background_event_id = p_event_id
        UNION
        SELECT permit_id FROM chalk_router.quota_leases
        WHERE background_environment_id = p_environment_id AND background_event_id = p_event_id
        ORDER BY permit_id
    LOOP
        PERFORM chalk_router.finish_quota('[]', owned_permit.permit_id, p_actual_amount, p_failed);
    END LOOP;
END;
$$;
