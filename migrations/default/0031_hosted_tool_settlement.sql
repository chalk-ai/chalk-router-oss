-- Hosted-tool requests hold a soft-cap reservation and may finish far beyond
-- it, long after admission. Keep their reservations alive while the permit
-- renews, and never drop the excess when a fixed window rolls over first.

-- New overload; the two-argument form stays for processes still running the
-- previous release.
CREATE FUNCTION chalk_router.renew_quota(
    p_permit_id UUID,
    p_lease_ttl_seconds BIGINT,
    p_reservation_ttl_seconds BIGINT
) RETURNS VOID LANGUAGE sql AS $$
    UPDATE chalk_router.quota_leases
    SET expires_at = clock_timestamp()
        + make_interval(secs => p_lease_ttl_seconds::INTEGER)
    WHERE permit_id = p_permit_id;
    UPDATE chalk_router.quota_reservations
    SET expires_at = clock_timestamp()
        + make_interval(secs => p_reservation_ttl_seconds::INTEGER)
    WHERE permit_id = p_permit_id AND background_event_id IS NULL;
$$;

CREATE OR REPLACE FUNCTION chalk_router.finish_quota(
    p_actions JSONB, p_permit_id UUID, p_actual_amount BIGINT, p_failed BOOLEAN DEFAULT FALSE
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    reservation RECORD;
    state_row RECORD;
    lease_row RECORD;
    v_now TIMESTAMPTZ := clock_timestamp();
    v_refilled NUMERIC;
    v_actual BIGINT;
    v_excess BIGINT;
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
               CASE WHEN stored.background_event_id IS NOT NULL THEN NULL
                    ELSE action.window_end END AS window_end,
               CASE WHEN p_failed AND stored.background_event_id IS NOT NULL
                    THEN stored.failed_amount ELSE p_actual_amount END AS actual_amount
        FROM chalk_router.quota_reservations AS stored
        LEFT JOIN jsonb_to_recordset(p_actions) AS action(
            policy_id TEXT, algorithm TEXT, limit_value BIGINT, burst BIGINT,
            window_end TIMESTAMPTZ
        ) ON action.policy_id = stored.policy_id
        WHERE stored.permit_id = p_permit_id
        ORDER BY stored.policy_id
    LOOP
        IF reservation.actual_amount IS NULL THEN CONTINUE; END IF;
        v_actual := GREATEST(reservation.actual_amount, 0);
        v_excess := GREATEST(v_actual - reservation.reserved_amount, 0);
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
            IF reservation.window_end IS NOT NULL AND reservation.window_end <= v_now THEN
                -- The admitting window closed and no admission has rolled the
                -- counter yet. Open the next window with the excess so the
                -- reset keeps it.
                UPDATE chalk_router.quota_states
                SET window_start = reservation.window_end, used_amount = v_excess
                WHERE policy_id = reservation.policy_id;
            ELSE
                UPDATE chalk_router.quota_states
                SET used_amount = LEAST(GREATEST(
                        used_amount::NUMERIC - reservation.reserved_amount + v_actual, 0
                    ), 9223372036854775807)::BIGINT
                WHERE policy_id = reservation.policy_id;
            END IF;
        ELSIF reservation.algorithm = 'fixed_window'
              AND state_row.window_start > reservation.window_start THEN
            -- A later admission already opened a newer window.
            UPDATE chalk_router.quota_states
            SET used_amount = LEAST(used_amount::NUMERIC + v_excess,
                    9223372036854775807)::BIGINT
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
