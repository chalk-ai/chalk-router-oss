-- Provider kind now selects an adapter, not a singleton credential. Names
-- remain unique for administration/seeding and non-null prefixes remain unique
-- for deterministic routing.
DROP INDEX chalk_router.uq_router_provider_connections_builtin_kind;
