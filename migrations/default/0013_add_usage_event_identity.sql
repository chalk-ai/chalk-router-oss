-- Add the durable per-call identity and usage provenance without rewriting the
-- existing usage table. PostgreSQL stores the constant SMALLINT default in table
-- metadata for historical rows; event ids are backfilled separately.
ALTER TABLE chalk_router.usage_events
    ADD COLUMN event_id UUID,
    -- Values match chalk.router.v1.UsageSource; zero/unspecified is not stored.
    ADD COLUMN usage_source SMALLINT NOT NULL DEFAULT 3,
    ALTER COLUMN provider DROP NOT NULL,
    ADD CONSTRAINT ck_router_usage_source
        CHECK (usage_source IN (1, 2, 3)) NOT VALID,
    ADD CONSTRAINT ck_router_usage_event_id_present
        CHECK (event_id IS NOT NULL) NOT VALID;

COMMENT ON COLUMN chalk_router.usage_events.provider IS
    'Deprecated compatibility column; new usage writers leave it NULL and queries use connection_id.';

-- Old binaries omit event_id while a rollout is in progress. Give those rows
-- an identity too; new binaries still allocate the UUID before their first
-- write so retries retain the same value.
ALTER TABLE chalk_router.usage_events
    ALTER COLUMN event_id SET DEFAULT gen_random_uuid();
