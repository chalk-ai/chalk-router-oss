-- Bound how long one admission waits on quota rows another request holds. An
-- unbounded wait parks a shared pool connection per queued request, so one hot
-- key can exhaust the pool and push every other tenant into fail-open
-- admission. The router fails a timed-out admission closed (429). The function
-- SET clause scopes the timeout to this call and restores the caller's setting.
ALTER FUNCTION chalk_router.acquire_quota(JSONB, UUID, BIGINT, BIGINT, TEXT, UUID, BIGINT)
    SET lock_timeout = '1s';
