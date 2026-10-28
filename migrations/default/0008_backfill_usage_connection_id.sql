-- Before this migration, each built-in provider kind had at most one current
-- connection per environment. Attribute a legacy event only when that current
-- connection already existed at the event timestamp. OpenAI-compatible
-- connections were never singleton, so their legacy events remain NULL.
-- Migration 0009 removes the singleton index only after this backfill, so each
-- updated event has at most one matching connection. Preserve any attribution
-- already written by a connection-aware router between migrations 0007 and
-- 0008.
UPDATE chalk_router.usage_events AS usage
SET connection_id = connection.id
FROM chalk_router.provider_connections AS connection
WHERE usage.connection_id IS NULL
  AND connection.environment_id = usage.environment_id
  AND connection.provider_kind = usage.provider
  AND connection.provider_kind <> 'openai-compatible'
  AND connection.created_at <= usage.time;
