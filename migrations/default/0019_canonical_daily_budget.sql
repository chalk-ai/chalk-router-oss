-- The API-key convenience field and Usage Budget CRUD both address this one
-- canonical policy shape. Do not silently choose a winner if a deployment has
-- historical duplicates: an operator must resolve them before the invariant is
-- installed.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM chalk_router.budget_limits
        WHERE window_type = 'calendar'
          AND window_size = INTERVAL '1 day'
          AND limit_kind = 'tokens'
        GROUP BY environment_id, scope, scope_id
        HAVING count(*) > 1
    ) THEN
        RAISE EXCEPTION
            'cannot enforce canonical daily token budgets while duplicates exist';
    END IF;
END;
$$;

CREATE UNIQUE INDEX budget_limits_canonical_daily_token_unique
    ON chalk_router.budget_limits (environment_id, scope, scope_id)
    WHERE window_type = 'calendar'
      AND window_size = INTERVAL '1 day'
      AND limit_kind = 'tokens';
