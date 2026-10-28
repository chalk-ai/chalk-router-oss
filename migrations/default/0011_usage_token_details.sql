-- Preserve Responses-specific billable token details. These are subsets of
-- the existing prompt/completion totals and must not be added to total_tokens.
ALTER TABLE chalk_router.usage_events
    ADD COLUMN cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN reasoning_tokens BIGINT NOT NULL DEFAULT 0;
