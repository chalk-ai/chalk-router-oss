# Cluster router deployments

[Documentation index](README.md) · [Deployment](deployment.md)

This guide describes current-main tenancy behavior. The admin permission and
federation requirements proposed in PR #353 are not yet merged; read the
[authentication transition](authentication-transition.md) before a rollout.

A router runs in one of two tenancy modes, chosen by `ROUTER_TENANCY_MODE`:

- **`environment`** (the default) serves one environment, `CHALK_ENVIRONMENT_ID`.
  It never calls the Chalk metadata API. Existing deployments need no changes.
- **`cluster`** serves the environments of many teams from one process. Each
  request's credential selects an environment. The metadata API confirms the
  environment, and its runtime is loaded on first use.

Each environment has its own stores, routing and judge state, quota policies,
sampling history, diagnostics, background jobs, telemetry settings, response
locators and object-storage namespace. A cluster shares one database pool,
pepper, KEK, HTTP client, completion-cache infrastructure, and collector and
export queues.

A cluster never lends its own cloud identity to a tenant. A keyless Bedrock
Runtime connection signs with the router process's AWS credential chain, so only
the legacy environment may create or use one. Every other environment must
store a Bedrock API key; creating, updating or testing a keyless connection is
rejected, and a row that is keyless anyway fails when its backend is built.
Vertex connections always carry their own service-account key.

Outside the legacy environment a cluster router also refuses to connect to
anything but public addresses, because a tenant chooses several destinations the
router then calls from inside the operator's network: a connection's `base_url`,
the `token_uri` inside a Vertex service-account key (raw or base64-encoded),
the `api_url` of a Braintrust telemetry exporter, and image URLs in requests.
Three layers apply (`src/egress.rs`):

- On create, update and test, and again when a backend is built, a URL must be
  `http`/`https` with no embedded credentials, and must not be an IP literal in a
  private, loopback, link-local, carrier-grade NAT, reserved or metadata range
  (including IPv4 addresses embedded in IPv6, such as NAT64 and 6to4) or an obviously local name such as
  `localhost`, `*.internal` or `*.cluster.local`. Image URLs and Braintrust `api_url`s get the same check; a stored exporter that
  fails it is not exported to.
- These environments use one shared HTTP client whose resolver refuses a name
  when any answer is non-public, so a public-looking hostname that resolves to
  a private address, or that changes its answer, is refused where the connection
  is made. Telemetry exporters for these environments use their own such client.
- That client follows a redirect only to a URL that passes the same check, up to
  five hops.

A tenant can still use its own `vllm` or `openai-compatible` endpoint at a public
address. The legacy environment, environment-mode routers and operator seeds
keep the ordinary client and may target private hosts such as an in-cluster vLLM.
A proxy configured on the router process is not applied (the client calls
`no_proxy()`), so the resolver check cannot be bypassed that way.

## Configuration

```sh
ROUTER_TENANCY_MODE=cluster
ROUTER_METADATA_API_ENDPOINT='https://api.chalk.ai'   # metadata API gRPC endpoint
ROUTER_KUBE_CLUSTER_ID='...'                          # optional: serve only this kube cluster's environments
ROUTER_DATABASE_URL='postgres://.../dataplane'
ROUTER_USAGE_BACKEND=postgres
ROUTER_QUOTA_BACKEND=postgres
OTEL_EXPORTER_OTLP_ENDPOINT='http://canonical-cluster-collector:4317'
```

Provision `ROUTER_METADATA_API_TOKEN`, `ROUTER_KEY_PEPPER` and
`ROUTER_SECRETS_KEK` through the deployment's existing serving-secret channel.
`ROUTER_METADATA_API_TOKEN` is a long-lived exchange token. The router trades it
for short-lived working tokens carrying `DEPLOY_READ`, refreshed before they
expire, exactly as the dataplane API server does. Provider credentials stay
encrypted in rows filtered by environment.

There is no environment list to maintain. Environments created after the router
starts are served on their first request, with no rollout.

`CHALK_ENVIRONMENT_ID` is optional in cluster mode. When set, it names the
legacy environment:

- It is loaded at startup from trusted configuration, without a metadata lookup.
- It keeps the deployment-default Braintrust exporters and its existing response
  locators.
- It is the only environment the static `ROUTER_ADMIN_KEY` can manage.

If `ROUTER_ADMIN_KEY` is configured, `CHALK_ENVIRONMENT_ID` must be too. Cluster
mode rejects `ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED`.

For the existing Argo router, keep `CHALK_ENVIRONMENT_ID=demo` and its existing
pepper and KEK. This preserves existing opaque keys, encrypted credentials and
legacy response locators. New environments configure their own exporters through
admin gRPC. In cluster mode, object-storage writes and inline reads use the
environment's namespace; old unscoped attachment URIs are not made readable by
other environments.

The Helm chart exposes `backend.tenancyMode`, `backend.metadataApiEndpoint`,
`backend.kubeClusterId` and `backend.legacyEnvironmentId`. The migrate Job gets
the mode and `CHALK_ENVIRONMENT_ID` but no metadata access: migrations are
environment-agnostic. In environment mode, the Job still requires
`CHALK_ENVIRONMENT_ID` as a tripwire for the Deployment it precedes.

## Which environments are served

The metadata API (`TeamService.GetEnvIncludingArchived`) answers "does this
environment exist, and which team owns it". The router treats an environment as
absent when it is:

- not found, or the metadata API denies the lookup
- archived or suspended
- on a different kube cluster, when `ROUTER_KUBE_CLUSTER_ID` is set

Answers are cached per environment: confirmed environments for 10 minutes,
absent ones for 30 seconds. Concurrent misses for the same environment share a
single call. Archival therefore takes effect within 10 minutes, and a new
environment becomes servable within 30 seconds of a lookup that missed it.

A runtime is created only for an environment the metadata API confirms. Runtimes
stay loaded until the process exits, so their number is bounded by the
environments this cluster actually serves. Concurrent first requests for an
environment build its runtime once; a failed build returns 503 and is retried by
the next request.

## Request identity and permissions

Opaque router keys need no environment header: an indexed hash lookup returns
the owning environment. A conflicting `x-chalk-env-id` is denied, and duplicate
hash ownership fails closed. Router keys authorize inference only, subject to
their existing connection and model permissions and quotas; they cannot invoke
admin RPCs.

Federated Chalk tokens are verified against the configured issuer registry
before their Chalk claims are used. The decoded claims are cached with the
verification, so repeat requests skip both the signature check and the payload
parse. Credentials reach an environment in one of two ways:

- **Named by the credential:**
  - environment-scoped working tokens (their `environment_id` claim)
  - user tokens with a grant for the environment selected by `x-chalk-env-id`
  - opaque keys (their owning row)

  The metadata API must confirm the environment exists. If the API is
  unavailable, environments that are already loaded keep being served, and
  others return 503.
- **Through the team:**
  - team service accounts
  - tenant tokens
  - support impersonation
  - user tokens with team grants

  These select an environment with `x-chalk-env-id`, and the metadata API must
  report the credential's team as its owner. If the API is unavailable, these
  requests return 503 because ownership cannot be proven.

Exchange tokens cannot invoke router operations. Static `ROUTER_ADMIN_KEY`
access stays confined to the legacy environment; cross-team admin callers use
federated tokens.

Each admin RPC declares the permission it requires in its proto, as
`option (chalk.auth.v1.permission)` on the RPC in `proto/chalk/router/v1`,
matching chalk-private's copies of the router protos.
`proto/chalk/auth/v1/permissions.proto` is copied byte for byte from
chalk-private (`just sync-chalk-protos`). The build fails if an RPC declares no
permission. On the main revision reviewed on 2026-10-26, every admin RPC declares
`PERMISSION_AUTHENTICATED`: any working credential admitted to the environment
may call it. [PR #353](https://github.com/chalk-ai/chalk-router/pull/353) replaces
this with per-operation grants and removes implicit service-account read grants;
do not assume those checks are already deployed. Inference requires
`query.online`.

Privileged-token semantics apply after environment selection:

- Empty or absent tenant-token permissions mean unrestricted access within the
  token's scope.
- Verified support impersonation grants every operation within the target team.
- Team-scoped service accounts get the router's explicit viewer read set,
  without writes or credential decryption.

## Telemetry and lifecycle

Attribution comes from the runtime that authentication selected. The shared OTLP
exporter groups mixed batches into separate environment resources at the one
canonical cluster collector. Each group retries independently, preserving its
resource without replaying groups that already succeeded. Braintrust
credentials, projects, content handling and destination generations are
specific to each environment, and stale-generation or unknown-tenant Braintrust
spans are discarded. Sampling histories and secrets are scoped even when clients
reuse identical trace, span and session IDs.

Inbound baggage is bounded and cannot set tenant or exporter controls.
Client-submitted spans keep flowing, but any `chalk.*` or `user.id` attributes
they carry are dropped. When content logging is off, only an explicit list of
non-content metadata survives; the shared span writer's policy discards
arbitrary custom attributes.

Each loaded environment gets its own background worker, which serializes its
own bounded polling batches with the configured per-batch concurrency, so a
stalled upstream does not block other environments. As soon as it starts
serving, and every minute after, the router also loads confirmed environments
that have pending background jobs. That covers restarts and jobs left behind by
another replica. This recovery runs on its own task, a few environments at a
time, with a time limit per environment: a slow metadata API delays neither
startup, nor settings refresh and quota maintenance, nor other environments'
recovery, and an environment that runs out of time is retried a minute later.

Settings refresh visits at most four environments concurrently. Quota
maintenance runs every minute: once for the shared Postgres runtime, or once per
in-memory runtime. After HTTP drains, shutdown stops the workers and drains each
environment's detached usage writes before flushing telemetry. Aborted job
leases stay recoverable through the existing lease expiry and idempotent
settlement rules.

This is logical tenant isolation within a trusted cluster: one KEK does not give
each team separate cryptographic custody. A noisy tenant can still contend for
shared CPU, pool and collector capacity, so configure quotas per environment and
key.

## Rollout scope

Cluster provisioning, data plane database selection, minting the router's
exchange token, and the metadata proxy preserving the real environment belong to
`chalk-private`. The exchange token must be able to read every environment the
router should serve. This repository supplies the router behavior and chart
inputs. No new public collector DNS name or cross-cluster telemetry transport is
needed. No migration runs at serving startup; shared schema changes use the
existing deployment Job. Net-new cluster routers do not migrate credentials from
other router deployments.
