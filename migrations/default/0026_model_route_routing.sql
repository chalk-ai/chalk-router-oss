-- Route-owned judge and fallback configuration. Existing routes retain their
-- connection/model destination; an empty grid has no judge or fallback policy.
-- Keep the default so older router versions can continue inserting routes.
ALTER TABLE chalk_router.model_routes
    ADD COLUMN routing JSONB NOT NULL DEFAULT '{}',
    ADD CONSTRAINT model_routes_routing_object CHECK (jsonb_typeof(routing) = 'object');
