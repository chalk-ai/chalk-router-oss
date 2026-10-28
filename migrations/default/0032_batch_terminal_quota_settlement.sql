-- Keep terminal accounting atomic, but lock states once and update all policies
-- in each grant together. Grants remain ordered: bucket caps and fixed-window
-- rollover are not commutative when two grants contain the same policy.
CREATE OR REPLACE FUNCTION chalk_router.finish_quota_usage(
    p_grants JSONB, p_actual_amount BIGINT,
    p_event_id UUID, p_time TIMESTAMPTZ, p_environment_id TEXT,
    p_api_key_hash TEXT, p_usage_pool_id TEXT, p_connection_id TEXT, p_model TEXT,
    p_prompt_tokens BIGINT, p_completion_tokens BIGINT, p_cached_tokens BIGINT,
    p_cache_write_tokens BIGINT, p_reasoning_tokens BIGINT, p_total_tokens BIGINT,
    p_usage_source SMALLINT, p_estimated_cost_usd DOUBLE PRECISION
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    grant_row RECORD;
    v_now TIMESTAMPTZ;
    v_actual BIGINT := GREATEST(p_actual_amount, 0);
BEGIN
    IF jsonb_typeof(p_grants) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'terminal grants must be an array';
    END IF;
    IF EXISTS (
        SELECT 1 FROM jsonb_array_elements(p_grants) AS grant_item,
                      jsonb_array_elements(grant_item->'actions') AS action
        WHERE action->>'environment_id' IS DISTINCT FROM p_environment_id
           OR grant_item->'background_owner' IS DISTINCT FROM 'null'::JSONB
    ) THEN
        RAISE EXCEPTION 'terminal accounting requires foreground grants in one environment';
    END IF;

    -- Receipt deletion, settlement, and usage insertion commit or roll back together.
    -- Insert before locking quota states: the insert needs no quota lock, so
    -- hot states are held only for settlement. Always settle receipts even when
    -- an earlier standalone insert wrote usage.
    INSERT INTO chalk_router.usage_events
        (event_id, time, environment_id, api_key_hash, usage_pool_id, connection_id, model,
         prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens,
         reasoning_tokens, total_tokens, request_count, usage_source, estimated_cost_usd)
    VALUES (p_event_id, p_time, p_environment_id, p_api_key_hash, p_usage_pool_id,
            p_connection_id, p_model, p_prompt_tokens, p_completion_tokens,
            p_cached_tokens, p_cache_write_tokens, p_reasoning_tokens,
            p_total_tokens, 1, p_usage_source, p_estimated_cost_usd)
    ON CONFLICT (environment_id, event_id) DO NOTHING;

    -- Canonical union ordering prevents deadlocks across differently ordered grants.
    PERFORM 1 FROM chalk_router.quota_states
    WHERE policy_id IN (
        SELECT policy_id FROM chalk_router.quota_reservations
        WHERE permit_id IN (SELECT (item->>'permit_id')::UUID FROM jsonb_array_elements(p_grants) AS item)
        UNION
        SELECT policy_id FROM chalk_router.quota_leases
        WHERE permit_id IN (SELECT (item->>'permit_id')::UUID FROM jsonb_array_elements(p_grants) AS item)
    ) ORDER BY policy_id FOR UPDATE;

    FOR grant_row IN SELECT * FROM jsonb_to_recordset(p_grants) AS item(permit_id UUID, actions JSONB)
    LOOP
        -- Rust admissions validate uniqueness. Retain the old ordered behavior
        -- for direct SQL callers supplying duplicate actions as well.
        IF EXISTS (SELECT 1 FROM jsonb_array_elements(grant_row.actions) AS action
                   GROUP BY action->>'policy_id' HAVING count(*) > 1) THEN
            PERFORM chalk_router.finish_quota(grant_row.actions, grant_row.permit_id, p_actual_amount, FALSE);
            CONTINUE;
        END IF;
        v_now := clock_timestamp();
        WITH reservations AS (
            DELETE FROM chalk_router.quota_reservations
            WHERE permit_id = grant_row.permit_id
            RETURNING *
        ), leases AS (
            DELETE FROM chalk_router.quota_leases
            WHERE permit_id = grant_row.permit_id
            RETURNING policy_id
        ), adjustments AS (
            SELECT stored.policy_id, stored.reserved_amount, stored.window_start,
                   CASE WHEN stored.background_event_id IS NOT NULL THEN stored.algorithm
                        ELSE action.algorithm END AS algorithm,
                   CASE WHEN stored.background_event_id IS NOT NULL THEN stored.limit_value
                        ELSE action.limit_value END AS limit_value,
                   CASE WHEN stored.background_event_id IS NOT NULL THEN stored.burst
                        ELSE action.burst END AS burst,
                   CASE WHEN stored.background_event_id IS NOT NULL THEN NULL
                        ELSE action.window_end END AS window_end,
                   GREATEST(v_actual - stored.reserved_amount, 0) AS excess
            FROM reservations AS stored
            LEFT JOIN jsonb_to_recordset(grant_row.actions) AS action(
                policy_id TEXT, algorithm TEXT, limit_value BIGINT, burst BIGINT,
                window_end TIMESTAMPTZ
            ) ON action.policy_id = stored.policy_id
        ), changes AS (
            SELECT COALESCE(a.policy_id, l.policy_id) AS policy_id,
                   a.reserved_amount, a.window_start, a.algorithm, a.limit_value,
                   a.burst, a.window_end, a.excess, l.policy_id IS NOT NULL AS release_lease
            FROM adjustments AS a FULL JOIN leases AS l USING (policy_id)
        )
        UPDATE chalk_router.quota_states AS state
        SET balance = CASE WHEN p_actual_amount IS NOT NULL AND c.algorithm = 'token_bucket'
                THEN LEAST(c.burst::NUMERIC,
                    LEAST(c.burst::NUMERIC, state.balance
                        + GREATEST(EXTRACT(EPOCH FROM (v_now - state.updated_at)), 0)
                        * c.limit_value::NUMERIC / 60)
                    + c.reserved_amount - v_actual)
                ELSE state.balance END,
            updated_at = CASE WHEN p_actual_amount IS NOT NULL AND c.algorithm = 'token_bucket'
                THEN v_now ELSE state.updated_at END,
            used_amount = CASE WHEN p_actual_amount IS NULL OR c.algorithm IS DISTINCT FROM 'fixed_window'
                THEN state.used_amount
                WHEN state.window_start IS NOT DISTINCT FROM c.window_start THEN
                    CASE WHEN c.window_end IS NOT NULL AND c.window_end <= v_now THEN c.excess
                         ELSE LEAST(GREATEST(state.used_amount::NUMERIC - c.reserved_amount + v_actual, 0),
                             9223372036854775807)::BIGINT END
                WHEN state.window_start > c.window_start
                    THEN LEAST(state.used_amount::NUMERIC + c.excess, 9223372036854775807)::BIGINT
                ELSE state.used_amount END,
            window_start = CASE WHEN p_actual_amount IS NOT NULL AND c.algorithm = 'fixed_window'
                    AND state.window_start IS NOT DISTINCT FROM c.window_start
                    AND c.window_end IS NOT NULL AND c.window_end <= v_now
                THEN c.window_end ELSE state.window_start END,
            active_count = CASE WHEN c.release_lease THEN GREATEST(state.active_count - 1, 0)
                ELSE state.active_count END
        FROM changes AS c WHERE state.policy_id = c.policy_id;
    END LOOP;
END;
$$;
