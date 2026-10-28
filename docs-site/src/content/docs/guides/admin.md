---
title: Admin gRPC and gRPC-Web
---

Administration is protobuf RPC, not JSON REST. Native gRPC and gRPC-Web share the inference listener (normally port 4001). The [generated service index](/reference/grpc/) covers every registered router RPC. All 47 router RPCs are unary; standard health has a separately declared, unimplemented streaming Watch method.

## Transport and serialization

RPC paths are `/chalk.router.v1.<Service>/<Method>`. Browser clients use gRPC-Web framing; native clients use gRPC over HTTP/2. Use of a Connect client library does not establish support for the Connect wire protocol. A JSON POST to an RPC path is not a substitute for a gRPC request.

Generated tables show protobuf snake_case field names and lowerCamelCase ProtoJSON names. ProtoJSON uses strings for 64-bit integer output, RFC3339 strings for timestamps, duration strings such as `"1s"`, enum names, and comma-separated strings for FieldMask. Native gRPC uses protobuf binary encoding instead. Explicit `optional` scalars and message fields carry presence; ordinary proto3 scalar fields have implicit defaults. Oneofs permit one member. Service validation can impose requirements beyond protobuf presence.

No reflection service is registered. Supply source protos or the [descriptor set](/api-descriptor.pb) to tooling. From the repository root, using a local plaintext listener:

```sh
export ROUTER_GRPC_ADDRESS='127.0.0.1:4001'
export ROUTER_ADMIN_KEY='replace-with-your-admin-credential'
grpcurl -plaintext \
  -import-path proto -proto chalk/router/v1/api_key.proto \
  -H "Authorization: Bearer $ROUTER_ADMIN_KEY" \
  -d '{"limit":40}' \
  "$ROUTER_GRPC_ADDRESS" chalk.router.v1.ApiKeyService/ListKeys
```

Omit `-plaintext` for a TLS endpoint and use the appropriate host:port. In cluster mode, add `-H 'x-chalk-env-id: example-environment'` only when the credential is authorized to select that environment. [Authentication rules and current-main limitations](/guides/authentication/) apply before dispatch.

The repository already generates TypeScript clients with `just gen-proto`. Its server-side gRPC-Web transport pattern is:

```ts
import { createClient } from '@connectrpc/connect';
import { createGrpcWebTransport } from '@connectrpc/connect-node';
import { ApiKeyService } from './gen/chalk/router/v1/api_key_pb';

const transport = createGrpcWebTransport({
  baseUrl: 'http://127.0.0.1:4001',
  httpVersion: '1.1',
});
const keys = createClient(ApiKeyService, transport);
const page = await keys.listKeys({ limit: 40 }, {
  headers: { Authorization: `Bearer ${adminCredential}` },
});
```

Adapt the generated import to your project and supply the credential from your authenticated server context. For a browser, use `createGrpcWebTransport` from `@connectrpc/connect-web`, omit `httpVersion`, and ensure your origin is allowed. Do not embed a shared static admin key in client-side code. This snippet illustrates the repository's existing client API; installing a standalone SDK is not part of site setup.

## Collections and pagination

Pass `next_cursor` back unchanged, and stop when it is absent. For paginated methods, `limit` defaults to 40, rejects nonpositive values, and is clamped to 1000 unless noted below. Omitted/empty cursor starts the collection.

| RPCs | Cursor behavior |
| --- | --- |
| ListKeys, ListProviderConnections, ListModelRoutes | Opaque `(created_at DESC, immutable id DESC)` cursor |
| ListBackgroundResponses | Opaque stable cursor; maximum limit 100; optional phase filter |
| ListRateLimits, ListUsagePools | Legacy RFC3339 timestamp cursor; first page defaults to now; equal-timestamp stability is not guaranteed |

Background phases are `submitting`, `active`, `terminal`, `unresolved`, and `settled`. Keep filters consistent while walking a collection. ListUsageBudgets, ListProviderConnectionModels, ListTelemetryExporters, ListAllUsage, and QueryUsage have no page cursor. HTTP model lists also have no pagination.

```json
{"limit":40,"cursor":"copy-the-previous-nextCursor-verbatim"}
```

This is illustrative ProtoJSON, not a fabricated usable cursor. ProtoJSON responses use `nextCursor`; generated language bindings may expose another casing.

## Create and update a key

[Validated gRPC payloads](/reference/examples/#create-a-restricted-key-with-grpc) include creation of a key restricted to one existing public route. The route must exist in the selected environment. Capture `rawKey` from creation securely; list/get records do not disclose it. Requests named `key` in update/revoke/usage methods use the **hash**.

```sh
# jq extracts only the payload from the checked example's type/body envelope.
jq '.body' docs-site/examples/create-key.grpc.json | \
  grpcurl -plaintext \
    -import-path proto -proto chalk/router/v1/api_key.proto \
    -H "Authorization: Bearer $ROUTER_ADMIN_KEY" \
    -d @ "$ROUTER_GRPC_ADDRESS" chalk.router.v1.ApiKeyService/CreateKey
```

### Field masks

- Omitted/empty mask updates only populated operation fields; no populated fields means no change.
- `"*"` alone requests full replacement, including defaults. It cannot be mixed with other paths.
- Unknown paths fail. lowerCamelCase paths normalize to snake_case.
- Repeated fields and messages replace rather than append or merge.
- A selected clearable field with no supplied value is cleared; unselected fields remain unchanged.

Key `permission`, `rate_limits`, and `usage_budgets` are whole top-level replacements; nested masks are rejected. Clearing permission makes a key unrestricted. Use the generated message definitions and [field-mask implementation](https://github.com/chalk-ai/chalk-router/blob/main/src/grpc/field_mask.rs) to distinguish absent values, defaults, and clearing. The checked update example changes only `description`.

## Service map

| Service | Purpose and caveats |
| --- | --- |
| [ApiKeyService](/reference/grpc-api-key/#apikeyservice) | Key lifecycle, legacy usage views, histogram; labels immutable and deprecated cost-tag writes rejected |
| [ProviderConnectionService](/reference/grpc-provider-connection/#providerconnectionservice) | Configure/test connections and list their models; credentials accepted on writes but not returned in reads |
| [ModelRouteService](/reference/grpc-model-route/#modelrouteservice) | Public model routes, route-owned judges/fallbacks, judge validation/defaults |
| [UsagePoolService](/reference/grpc-usage-pool/#usagepoolservice) | Shared usage pools |
| [RateLimitService](/reference/grpc-rate-limit/#ratelimitservice) | Rate/concurrency constraints |
| [UsageBudgetService](/reference/grpc-usage-budget/#usagebudgetservice) | Admission budgets; current counters are not historical usage ledgers |
| [UsageService](/reference/grpc-usage/#usageservice) | Aggregated usage queries |
| [BackgroundResponseService](/reference/grpc-background-response/#backgroundresponseservice) | Inspect background job accounting state |
| [SamplingPolicyService](/reference/grpc-sampling-policy/#samplingpolicyservice) | Trace sampling policy |
| [TelemetrySettingsService](/reference/grpc-telemetry-settings/#telemetrysettingsservice) | Exporter configuration/test; external test operations can contact the configured exporter |
| [DebugService](/reference/grpc-debug/#debugservice) | Debug/admin diagnostics; treat output as privileged |
| [FallbackPolicyService](/reference/grpc-fallback-policy/#fallbackpolicyservice), [JudgePolicyService](/reference/grpc-judge-policy/#judgepolicyservice) | Retired global APIs; every method returns FAILED_PRECONDITION. Configure ModelRoute.routing instead. |

Connection tests can use a saved connection, complete draft, or a saved-credential draft. The latter requires matching provider kind and does not persist changes. Testing a provider connection can contact that upstream; it is not a schema-only check.

## Usage queries

QueryUsage requires `start`, `end`, and exactly one grouping: `global:true`, a non-unspecified dimension, or an API-key label. Empty `bucket_size` defaults to `1h`; supported sizes are `1m`, `5m`, `15m`, `1h`, `6h`, `1d`, and `7d`/`1w`. The service limits range to 366 days, buckets/results to 10,000, filter lists to 100 values, and label-selected keys to 1,000. Oversized queries error instead of paginating. Disabled usage tracking returns FAILED_PRECONDITION.

Cost fields estimate public list-price USD. An absent estimate differs from a priced zero. `unpriced_request_count` reports missing price-catalog coverage. [Usage schemas](/reference/grpc-usage/) contain exact fields and numeric types.

## RPC errors

Read gRPC status/trailers rather than HTTP 200 alone. Service errors map to INVALID_ARGUMENT, NOT_FOUND, ALREADY_EXISTS, FAILED_PRECONDITION, or INTERNAL. When authentication permits dispatch, unknown methods on known services return UNIMPLEMENTED and unknown service paths return HTTP 404. Cluster JWT requests to unknown admin paths fail closed with HTTP 403 before dispatch.

Authentication runs before gRPC dispatch. On this baseline it can return HTTP JSON 401/403/503 instead of gRPC trailers. Clients must handle transport-level failures as well as RPC status. The [HTTP error guide](/guides/errors-streaming/) explains these envelopes.

Sources: [gRPC router](https://github.com/chalk-ai/chalk-router/blob/main/src/grpc/router.rs), [existing client](https://github.com/chalk-ai/chalk-router/blob/main/ui/src/lib/grpc.ts), [pagination](https://github.com/chalk-ai/chalk-router/blob/main/src/service/pagination.rs), [transport tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/grpc_smoke_test.rs), [usage validation](https://github.com/chalk-ai/chalk-router/blob/main/src/service/usage.rs).
