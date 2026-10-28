-- Models are an unordered selector. Canonicalize the persisted arrays before
-- enforcing identity so equivalent selectors cannot occupy separate rows.
WITH normalized AS (
    SELECT environment_id,
           id,
           ARRAY(
               SELECT DISTINCT model
               FROM unnest(limits.models) AS model
               ORDER BY model
           ) AS models
    FROM chalk_router.rate_limits AS limits
)
UPDATE chalk_router.rate_limits AS limits
SET models = normalized.models
FROM normalized
WHERE limits.environment_id = normalized.environment_id
  AND limits.id = normalized.id
  AND limits.models IS DISTINCT FROM normalized.models;

-- Do not discard duplicates automatically: every definition may already own
-- independent mutable quota state. Operators must choose which policy to keep.
DO $$
DECLARE
    duplicate RECORD;
BEGIN
    SELECT environment_id, scope, scope_id, limit_type, connection_id, models,
           array_agg(id ORDER BY id) AS policy_ids
    INTO duplicate
    FROM chalk_router.rate_limits
    GROUP BY environment_id, scope, scope_id, limit_type, connection_id, models
    HAVING count(*) > 1
    LIMIT 1;

    IF FOUND THEN
        RAISE EXCEPTION
            'cannot add unique rate-limit identity: duplicate policies % for environment %, scope % %, type %, connection %, models %; edit or remove duplicates and retry',
            duplicate.policy_ids,
            duplicate.environment_id,
            duplicate.scope,
            duplicate.scope_id,
            duplicate.limit_type,
            COALESCE(duplicate.connection_id, '<all>'),
            duplicate.models;
    END IF;
END;
$$;

CREATE UNIQUE INDEX rate_limits_global_identity_unique
    ON chalk_router.rate_limits
        (environment_id, scope, scope_id, limit_type)
    WHERE connection_id IS NULL;

CREATE UNIQUE INDEX rate_limits_connection_identity_unique
    ON chalk_router.rate_limits
        (environment_id, scope, scope_id, limit_type, connection_id, models)
    WHERE connection_id IS NOT NULL;

ALTER TABLE chalk_router.rate_limits
    DROP COLUMN name;
