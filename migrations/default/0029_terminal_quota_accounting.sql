-- One autocommitted terminal operation. Settlement is receipt-idempotent;
-- usage is event-idempotent. Always replay settlement even if usage already
-- exists, so a standalone usage insert is never mistaken for a combined commit.
CREATE FUNCTION chalk_router.finish_quota_usage(
    p_grants JSONB, p_actual_amount BIGINT,
    p_event_id UUID, p_time TIMESTAMPTZ, p_environment_id TEXT,
    p_api_key_hash TEXT, p_usage_pool_id TEXT, p_connection_id TEXT, p_model TEXT,
    p_prompt_tokens BIGINT, p_completion_tokens BIGINT, p_cached_tokens BIGINT,
    p_cache_write_tokens BIGINT, p_reasoning_tokens BIGINT, p_total_tokens BIGINT,
    p_usage_source SMALLINT, p_estimated_cost_usd DOUBLE PRECISION
) RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    grant_row RECORD;
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

    -- Acquire the union before finishing any grant. Locking each grant's own
    -- sorted subset sequentially would still allow cross-grant deadlocks.
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
        PERFORM chalk_router.finish_quota(grant_row.actions, grant_row.permit_id, p_actual_amount, FALSE);
    END LOOP;

    INSERT INTO chalk_router.usage_events
        (event_id, time, environment_id, api_key_hash, usage_pool_id, connection_id, model,
         prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens,
         reasoning_tokens, total_tokens, request_count, usage_source, estimated_cost_usd)
    VALUES (p_event_id, p_time, p_environment_id, p_api_key_hash, p_usage_pool_id,
            p_connection_id, p_model, p_prompt_tokens, p_completion_tokens,
            p_cached_tokens, p_cache_write_tokens, p_reasoning_tokens,
            p_total_tokens, 1, p_usage_source, p_estimated_cost_usd)
    ON CONFLICT (environment_id, event_id) DO NOTHING;
END;
$$;
