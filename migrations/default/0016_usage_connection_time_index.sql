-- no-transaction
-- The existing environment/time and key/time indexes already serve the other
-- common filters. Connection is the one missing dimension introduced earlier.
CREATE INDEX CONCURRENTLY idx_router_usage_events_connection_time
    ON chalk_router.usage_events (environment_id, connection_id, time)
    WHERE (connection_id IS NOT NULL);
