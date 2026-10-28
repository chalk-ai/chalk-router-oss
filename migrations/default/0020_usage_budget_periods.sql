-- Named UTC calendar periods replace the daily-only resource identity.
ALTER TABLE chalk_router.budget_limits
    ADD COLUMN period TEXT NOT NULL DEFAULT 'daily'
        CHECK (period IN ('daily', 'weekly', 'monthly'));

UPDATE chalk_router.budget_limits
SET period = CASE
    WHEN window_type = 'calendar' AND window_size = INTERVAL '7 days' THEN 'weekly'
    WHEN window_type = 'calendar' AND window_size = INTERVAL '1 month' THEN 'monthly'
    ELSE 'daily'
END
WHERE limit_kind = 'tokens';

DROP INDEX IF EXISTS chalk_router.budget_limits_canonical_daily_token_unique;
CREATE UNIQUE INDEX budget_limits_canonical_token_period_unique
    ON chalk_router.budget_limits (environment_id, scope, scope_id, period)
    WHERE limit_kind = 'tokens' AND window_type = 'calendar';
