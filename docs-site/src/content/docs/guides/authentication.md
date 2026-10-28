---
title: Authentication and permissions
---

## Credentials and headers

| Credential | Inference | Administration |
| --- | --- | --- |
| Router API key (`ck-…`) | Restricted to its environment and model/route grants | Rejected |
| Configured static admin key | Accepted by the auth middleware | Accepted, scoped to the configured legacy environment in cluster mode |
| Federated JWT | Requires a configured trusted issuer; cluster inference requires `query.online` | Requires the declared RPC permission and verified tenant ownership |

Send `Authorization: Bearer <credential>` (case-sensitive prefix) or `X-Api-Key: <credential>`. Gemini Interactions also recognizes `x-goog-api-key` for **router** credentials. Avoid sending multiple competing credential headers. These credentials authenticate to the router; provider credentials belong to provider connections.

Health paths `/health`, `/v1/healthcheck`, and `/grpc.health.v1.Health/*` bypass authentication. Missing admin configuration does not enable anonymous administration. The explicitly dangerous development setting `ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED` applies only to inference with credentials absent; an invalid presented credential is still rejected, and cluster mode disallows this setting.

## Administration and UI sessions

This combined release candidate includes operation-specific admin grants. Federation
requires a trusted issuer and audience. Environment-mode JWT administration also
requires a configured owning team, a matching signed environment, and the RPC's
declared permission. Unknown admin operations fail closed. Cross-environment
inference compatibility does not relax administration. Compact `p` payloads remain
rejected on admin/cluster requests pending decoder support.

The UI requires an exact `ROUTER_UI_ORIGIN` and a dedicated 64-hex-character
`ROUTER_UI_SESSION_SECRET`. Use HTTPS outside loopback. Missing or invalid settings
fail closed. Sign-in verifies the user's JWT and `tokens.list` permission; each
admin RPC forwards the same user's credentials and is authorized again by the backend.
Sessions are encrypted, HttpOnly and SameSite=Strict, with at most a 15-minute/JWT-expiry
lifetime and a 3,800-character cookie limit. Signing out clears the browser cookie;
rotate the session secret to invalidate copied cookies.

Playground requests require a separate user-supplied router API key. The UI never
uses a server admin key as a fallback. State-changing requests require the exact
same Origin and JSON content type (DELETE may omit content type).

`DELETE /v1/models/{model_id}` returns 501 for every caller after inference auth.
Delete aliases through `ModelRouteService/DeleteModelRoute` on the admin surface.

## Environment selection

For inference in single-environment mode, a JWT with no `environment_id` is currently accepted; a matching environment is accepted. A differing environment needs `ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS`. This compatibility does not apply to admin RPCs.

In cluster mode, a router key's ownership or a scoped JWT selects the environment. A conflicting `x-chalk-env-id` is rejected. For unscoped working JWTs, the header selects an environment, and verified environment grants or team ownership must authorize access. Recognized working kinds include users, service accounts, sandboxes, engines, metadata-service agents, and tenants; exchange/unknown kinds fail. A directory outage can produce 503.

## Inference key permissions

[ApiKeyPermission](/reference/grpc-api-key/#apikeypermission) defines two independent forms of grants:

- `connection_scopes`: direct model IDs on a connection. An empty model list grants any direct model on that connection.
- `routes.refs`: model routes, by immutable route ID or a public model ID resolved when the permission is written. A route grant covers its targets, judge, outcomes, and fallbacks.

Omitting both restriction forms permits any model. Present-but-empty `routes` with no connection scopes permits none. Deleting and recreating a public route does not transfer a grant, because stored grants refer to immutable IDs. Legacy permission fields `provider`, `models`, and `connection_id` are rejected on writes, not silently ignored.

Rate limits and usage budgets further constrain an otherwise authorized request. Clearing key permission via an update mask makes it unrestricted; review permission updates carefully. Labels are immutable after key creation. `CreateKey` returns `raw_key` once; normal key records expose `key_hash` and `key_prefix`. Update/revoke/usage requests identify the key by hash, not by its secret.

Sources: [auth middleware](https://github.com/chalk-ai/chalk-router/blob/main/src/middleware.rs), [cluster auth](https://github.com/chalk-ai/chalk-router/blob/main/src/cluster_auth.rs), [tenancy tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/tenancy_test.rs), and [key permission definitions](/reference/grpc-api-key/).
