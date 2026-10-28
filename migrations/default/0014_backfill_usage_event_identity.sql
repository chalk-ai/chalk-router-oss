-- Keep the data rewrite in its own committed migration. The predicate makes a
-- retry safe and avoids changing ids already assigned by rolling-deployment
-- writers.
UPDATE chalk_router.usage_events
SET event_id = gen_random_uuid()
WHERE event_id IS NULL;
