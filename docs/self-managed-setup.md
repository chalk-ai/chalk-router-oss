# Self-managed setup

Start without a database using the authenticated loopback quickstart in README.
`just setup` installs UI dependencies only. Provider connections are configured
through the admin API; `just seed --file /path/to/connections.json` accepts your
explicit seed file and environment credentials. There is no implicit seed dataset.

For persistence, supply an owned PostgreSQL 16 database. Configure
`ROUTER_DATABASE_URL` and `CHALK_ENVIRONMENT_ID`, plus independently generated
admin, pepper, and encryption keys using the formats documented in README and
configuration code. For a new local database, generate independent values:

```sh
export ROUTER_ADMIN_KEY="$(openssl rand -hex 32)"
export ROUTER_KEY_PEPPER="$(openssl rand -hex 32)"
export ROUTER_SECRETS_KEK="$(openssl rand -base64 32)"
```

Persist these securely for an existing database; generating a new pepper or KEK
on every restart invalidates stored keys or prevents credential decryption.
`just migrate` rejects non-loopback database destinations;
shared deployments should use a reviewed migration workflow. The chart's migration
hook is a write operation, so rendering a chart does not authorize installing it.

Build images locally with `docker build -t chalk-router:local .` and
`docker build -t chalk-router-ui:local ui`. For a cluster, publish to a registry you
control and set both chart image repository/tag values explicitly. Put credentials
in your own Secret named by `servingSecretName`; do not reuse development fixtures
or commit the Secret to Git. Verify the security-hardening and authentication
changes required by the release checklist before exposing any service.

The synthetic connection example in `examples/connections.json` uses a loopback
mock upstream and no customer data. Start `just mock-openai` before using it.
Seeding requires the explicitly configured local database. It is intended only for local testing, not as a shared deployment seed.

Cloud credentials, buckets, identity-provider JWKS, metadata API access (for cluster
mode), and telemetry destinations are optional operator settings. Default chart
values contain no company registry, issuer, collector, or environment identifier.
Company deployment assets remain in private Git history; public release must use
a reviewed history-free export rather than making this development repository public.
