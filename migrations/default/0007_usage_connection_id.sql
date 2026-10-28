-- New writers always provide an immutable connection id. The column remains
-- nullable because legacy rows cannot always be attributed safely, and because
-- an old router replica may still flush its pre-upgrade buffer during rollout.
ALTER TABLE chalk_router.usage_events
    ADD COLUMN connection_id TEXT;
