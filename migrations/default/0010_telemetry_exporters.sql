-- Optional telemetry exporter settings. The config document is intentionally
-- generic so new exporter types do not require a schema migration. Recoverable
-- API keys are encrypted with ROUTER_SECRETS_KEK, the same key used for provider
-- credentials.

CREATE TABLE chalk_router.telemetry_exporters (
    environment_id     TEXT    NOT NULL,
    id                 TEXT    NOT NULL,
    name               TEXT    NOT NULL,
    config             JSONB   NOT NULL,
    enabled            BOOLEAN NOT NULL DEFAULT true,
    api_key_envelope   BYTEA   NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at         TIMESTAMPTZ,
    PRIMARY KEY (environment_id, id)
);

CREATE UNIQUE INDEX uq_router_telemetry_exporters_name
    ON chalk_router.telemetry_exporters (environment_id, lower(btrim(name)))
    WHERE deleted_at IS NULL;
