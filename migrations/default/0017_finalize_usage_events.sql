-- Validate historical rows only after the event-id backfill and concurrent
-- index build have committed. NOT VALID avoids a table scan while the check is
-- first installed, while still enforcing it for concurrent writes.
ALTER TABLE chalk_router.usage_events
    ADD CONSTRAINT ck_router_usage_token_invariants
        CHECK (
            prompt_tokens >= 0
            AND completion_tokens >= 0
            AND cached_tokens >= 0
            AND cache_write_tokens >= 0
            AND reasoning_tokens >= 0
            AND total_tokens >= 0
            AND request_count >= 0
            AND cached_tokens <= prompt_tokens
            AND cache_write_tokens <= prompt_tokens
            AND reasoning_tokens <= completion_tokens
        ) NOT VALID;

ALTER TABLE chalk_router.usage_events
    VALIDATE CONSTRAINT ck_router_usage_event_id_present;

ALTER TABLE chalk_router.usage_events
    VALIDATE CONSTRAINT ck_router_usage_source;

ALTER TABLE chalk_router.usage_events
    VALIDATE CONSTRAINT ck_router_usage_token_invariants;

-- PostgreSQL can use the validated check to avoid a second full-table scan.
ALTER TABLE chalk_router.usage_events
    ALTER COLUMN event_id SET NOT NULL;

ALTER TABLE chalk_router.usage_events
    DROP CONSTRAINT ck_router_usage_event_id_present;
