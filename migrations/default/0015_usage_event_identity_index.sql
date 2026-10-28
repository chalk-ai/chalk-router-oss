-- no-transaction
-- Build outside a transaction so inserts can continue while PostgreSQL scans
-- a large historical ledger.
CREATE UNIQUE INDEX CONCURRENTLY uq_router_usage_events_event
    ON chalk_router.usage_events (environment_id, event_id);
