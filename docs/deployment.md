# Deployment

Use [self-managed setup](self-managed-setup.md) for explicit credentials, database
and image configuration. The repository includes no company deployment pipeline.
Build backend, UI, and the capability-free Caddy image (`caddy/Dockerfile`) into
your own registry and set all three chart image repositories/tags.

## Workload

See [container security](container-security.md) for non-root, read-only,
no-capabilities runtime requirements and bounded writable mounts.

## Networking

No ingress is created by default. Expose services only after configuring backend
and UI authentication. Private ingress and public API Funnel have separate opt-ins.

## Secrets

Provide a Secret selected by `servingSecretName` for backend database and keys.
The UI uses a separate session Secret; it does not receive the backend admin key.
See [authentication](authentication-transition.md).

## Non-secret env

Federation uses `backend.auth.jwtIssuers`, `jwtAudience` and `teamId`. Telemetry
uses `backend.telemetry.otlpEndpoint`; content capture defaults off. No company
identity provider, collector, registry or bucket is selected.

## Migrations on deploy

The chart runs a migration hook before rollout against the configured database.
Validate on an owned disposable database before an authorized installation.
Rendering the chart does not execute a migration.

## Object storage

Configure your own storage URI/credentials when needed for image or trace
attachments. The Files upload API remains disabled regardless of storage setup;
see [file security](file-api-security.md).
