-- Model-as-judge routing policy. One env-scoped row, mirroring
-- chalk_router.fallback_policy: the whole admin-editable policy (enabled, judge
-- model, per-model downgrade ladders, band boundaries, shadow sampling rate,
-- and the global default opt-in / floor that per-key settings inherit) is
-- stored as a single JSONB document. Deployment wiring (backend kind, sidecar
-- URL, timeout, concurrency, kill switch) lives in env, not here.
--
-- The per-key overrides (`downgradable`, `model_floor`) ride on the existing
-- chalk_router.api_keys.permission JSONB column and need no migration.

CREATE TABLE chalk_router.judge_policy (
    environment_id TEXT PRIMARY KEY,
    policy         JSONB NOT NULL DEFAULT '{}'
);
