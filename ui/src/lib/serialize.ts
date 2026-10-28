/**
 * Helpers for serializing proto messages from `@/lib/grpc` into the JSON
 * shapes that the legacy `/api/admin/*` route handlers expose to the
 * browser. Keeping these shapes stable lets us migrate the route handlers
 * to Connect without touching every client component at once.
 */
import { routeGridFromProto } from "./routeGrid";
import { CacheScope, type ApiKey } from "@/gen/chalk/router/v1/api_key_pb";
import { RouteDiagnostic, type ModelRoute } from "@/gen/chalk/router/v1/model_route_pb";
import {
  ExposurePolicy,
  type ProviderConnection,
} from "@/gen/chalk/router/v1/provider_connection_pb";
import { type RateLimitPolicy } from "@/gen/chalk/router/v1/rate_limit_pb";
import {
  OutcomeComparator,
  type OutcomeRule,
  type SamplingPolicy,
  type SamplingRule,
} from "@/gen/chalk/router/v1/sampling_policy_pb";
import { type TelemetryExporter } from "@/gen/chalk/router/v1/telemetry_settings_pb";
import { type UsagePool } from "@/gen/chalk/router/v1/usage_pool_pb";
import { UsageBudgetPeriod, type UsageBudget } from "@/gen/chalk/router/v1/usage_budget_pb";
import { toJson } from "@bufbuild/protobuf";
import { ValueSchema, type Timestamp } from "@bufbuild/protobuf/wkt";

export type UsageBudgetPeriodJson = "daily" | "weekly" | "monthly";

/**
 * Usage-budget period, proto enum <-> the snake_case string the browser uses.
 * UNSPECIFIED (and any other unrecognized value) renders as "daily", the
 * server's default period.
 */
export function usageBudgetPeriodToJson(period: UsageBudgetPeriod): UsageBudgetPeriodJson {
  switch (period) {
    case UsageBudgetPeriod.WEEKLY:
      return "weekly";
    case UsageBudgetPeriod.MONTHLY:
      return "monthly";
    default:
      return "daily";
  }
}

/** Returns `undefined` for anything that isn't a recognized period string. */
export function usageBudgetPeriodFromJson(period: string | undefined): UsageBudgetPeriod | undefined {
  switch (period) {
    case "daily":
      return UsageBudgetPeriod.DAILY;
    case "weekly":
      return UsageBudgetPeriod.WEEKLY;
    case "monthly":
      return UsageBudgetPeriod.MONTHLY;
    default:
      return undefined;
  }
}

/**
 * Prompt-cache scope, proto enum <-> the snake_case string the browser uses.
 * UNSPECIFIED renders as "token", matching how the server reads it: the
 * narrowest scope, so an omitted field never widens who a cached completion is
 * shared with.
 */
export function cacheScopeToString(s: CacheScope): string {
  switch (s) {
    case CacheScope.MODEL:
      return "model";
    case CacheScope.TEAM:
      return "team";
    default:
      return "token";
  }
}

export function cacheScopeFromString(s: string | undefined): CacheScope {
  switch (s) {
    case "model":
      return CacheScope.MODEL;
    case "team":
      return CacheScope.TEAM;
    default:
      return CacheScope.TOKEN;
  }
}

function isoFromTimestamp(ts: Timestamp | undefined): string {
  if (!ts) return "";
  return new Date(Number(ts.seconds) * 1000).toISOString();
}

export function apiKeyToJson(k: ApiKey) {
  return {
    key_hash: k.keyHash,
    key_prefix: k.keyPrefix,
    description: k.description,
    created_at: isoFromTimestamp(k.createdAt),
    permission: {
      // Access is the union of model-route grants and connection scopes.
      // `routes` is null only on an unrestricted key (no routes and no
      // connection scopes); a restricted key always carries it, possibly with
      // no ids. The legacy provider/connection_id/models fields are never
      // returned by the server and are not emitted.
      connection_scopes: (k.permission?.connectionScopes ?? []).map((s) => ({
        connection_id: s.connectionId,
        models: s.models,
      })),
      routes: k.permission?.routes ? {
        refs: k.permission.routes.refs.map((route) => {
          if (route.identifier.case !== "routeId") throw new Error("Expected a canonical route_id in key response");
          return { route_id: route.identifier.value };
        }),
      } : null,
      // Model-as-judge tri-state opt-in (undefined = inherit the policy default)
      // and the per-key downgrade floor (public model id).
      downgradable: k.permission?.downgradable ?? null,
      model_floor: k.permission?.modelFloor ?? null,
      // Exact-prefix completion caching. `null` means the key never opted in
      // (it is neither served from the cache nor populates it).
      cache: k.permission?.cache
        ? {
            enabled: k.permission.cache.enabled,
            scope: cacheScopeToString(k.permission.cache.scope),
            ttl_secs:
              k.permission.cache.ttlSecs != null
                ? Number(k.permission.cache.ttlSecs)
                : null,
            max_entry_bytes:
              k.permission.cache.maxEntryBytes != null
                ? Number(k.permission.cache.maxEntryBytes)
                : null,
          }
        : null,
    },
    labels: k.labels,
    usage: {
      prompt_tokens: Number(k.usage?.promptTokens ?? 0n),
      completion_tokens: Number(k.usage?.completionTokens ?? 0n),
      total_tokens: Number(k.usage?.totalTokens ?? 0n),
      request_count: Number(k.usage?.requestCount ?? 0n),
    },
    daily_token_budget: k.dailyTokenBudget != null ? Number(k.dailyTokenBudget) : null,
    usage_budgets: k.usageBudgets.map((budget) => ({
      id: budget.id,
      token_limit: Number(budget.tokenLimit),
      period: usageBudgetPeriodToJson(budget.period),
      enabled: budget.enabled,
    })),
    usage_pool_id: k.usagePoolId ?? null,
    rate_limits: k.rateLimits.map((limit) => ({
      connection_id: limit.connectionId,
      models: limit.models,
      requests_per_minute: limit.requestsPerMinute != null ? Number(limit.requestsPerMinute) : null,
      request_burst: limit.requestBurst != null ? Number(limit.requestBurst) : null,
      tokens_per_minute: limit.tokensPerMinute != null ? Number(limit.tokensPerMinute) : null,
      token_burst: limit.tokenBurst != null ? Number(limit.tokenBurst) : null,
      concurrent_requests: limit.concurrentRequests != null ? Number(limit.concurrentRequests) : null,
      enabled: limit.enabled ?? true,
    })),
  };
}

export function usagePoolToJson(p: UsagePool) {
  return {
    id: p.id,
    name: p.name,
    created_at: isoFromTimestamp(p.createdAt),
  };
}

export function usageBudgetToJson(budget: UsageBudget) {
  return {
    id: budget.id,
    scope: budget.scope,
    scope_id: budget.scopeId,
    token_limit: Number(budget.tokenLimit),
    period: usageBudgetPeriodToJson(budget.period),
    enabled: budget.enabled,
    created_at: isoFromTimestamp(budget.createdAt),
    current_usage: budget.currentUsage != null ? Number(budget.currentUsage) : null,
    next_reset_at: isoFromTimestamp(budget.nextResetAt),
  };
}

export function rateLimitToJson(p: RateLimitPolicy) {
  return {
    id: p.id,
    limit_type: p.limitType,
    limit_value: Number(p.limitValue),
    burst_value: p.burstValue != null ? Number(p.burstValue) : null,
    connection_id: p.connectionId ?? null,
    models: p.models,
    scope: p.scope === "token" ? "api_key" : p.scope,
    scope_id: p.scopeId,
    enabled: p.enabled,
    created_at: isoFromTimestamp(p.createdAt),
  };
}

/* ------------------------------------------------------------------ */
/*  Provider connections                                              */
/* ------------------------------------------------------------------ */

export function exposureToString(e: ExposurePolicy): string {
  switch (e) {
    case ExposurePolicy.DYNAMIC:
      return "dynamic";
    case ExposurePolicy.UNLISTED:
      return "unlisted";
    case ExposurePolicy.EXPLICIT:
      return "explicit";
    default:
      return "unspecified";
  }
}

export function exposureFromString(s: string | undefined): ExposurePolicy {
  switch (s) {
    case "dynamic":
      return ExposurePolicy.DYNAMIC;
    case "unlisted":
      return ExposurePolicy.UNLISTED;
    case "explicit":
      return ExposurePolicy.EXPLICIT;
    default:
      return ExposurePolicy.UNSPECIFIED;
  }
}

export function providerConnectionToJson(c: ProviderConnection) {
  return {
    id: c.id,
    name: c.name,
    provider_kind: c.providerKind,
    base_url: c.baseUrl ?? null,
    prefix: c.prefix ?? null,
    exposure: exposureToString(c.exposure),
    routing_enabled: c.routingEnabled,
    state: {
      configured: c.state?.configured ?? false,
      unreadable: c.state?.unreadable ?? false,
      healthy: c.state?.healthy ?? null,
    },
    declared_capabilities: c.declaredCapabilities ?? null,
    created_at: isoFromTimestamp(c.createdAt),
    updated_at: isoFromTimestamp(c.updatedAt),
  };
}

/* ------------------------------------------------------------------ */
/*  Model routes                                                      */
/* ------------------------------------------------------------------ */

export function diagnosticToString(d: RouteDiagnostic): string {
  switch (d) {
    case RouteDiagnostic.READY:
      return "ready";
    case RouteDiagnostic.UNCONFIGURED:
      return "unconfigured";
    case RouteDiagnostic.DISABLED:
      return "disabled";
    case RouteDiagnostic.UNREADABLE:
      return "unreadable";
    case RouteDiagnostic.MISSING_CONNECTION:
      return "missing_connection";
    default:
      return "unspecified";
  }
}

export function modelRouteToJson(r: ModelRoute) {
  return {
    id: r.id,
    public_model_id: r.publicModelId,
    connection_id: r.connectionId,
    upstream_model_id: r.upstreamModelId,
    published: r.published,
    diagnostic: diagnosticToString(r.diagnostic),
    routing: routeGridFromProto(r.routing),
    created_at: isoFromTimestamp(r.createdAt),
    updated_at: isoFromTimestamp(r.updatedAt),
  };
}

export interface TelemetryExporterJson {
  id: string;
  name: string;
  exporter_type: string;
  config: Record<string, unknown>;
  enabled: boolean;
  api_key_set: boolean;
  read_only: boolean;
  created_at: string;
  updated_at: string;
}

export function telemetryExporterToJson(s: TelemetryExporter): TelemetryExporterJson {
  let config: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(s.configJson);
    if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)) {
      config = parsed as Record<string, unknown>;
    }
  } catch {
    // Keep the row visible even if an older/custom exporter has malformed JSON.
  }
  return {
    id: s.id,
    name: s.name,
    exporter_type: s.exporterType,
    config,
    enabled: s.enabled,
    api_key_set: s.apiKeySet,
    read_only: s.readOnly,
    created_at: isoFromTimestamp(s.createdAt),
    updated_at: isoFromTimestamp(s.updatedAt),
  };
}

/* ------------------------------------------------------------------ */
/*  Sampling policy (trace sampling for LLM frontends)                */
/* ------------------------------------------------------------------ */

export type OutcomeComparatorJson = "eq" | "ne" | "gt" | "lt" | "gte" | "lte";

/**
 * Response-content comparator, proto enum <-> the snake_case string the
 * browser uses. UNSPECIFIED (and any other unrecognized value) renders as
 * "eq" — the browser always sends an explicit choice on save, so this only
 * affects a row that somehow reached storage with an unspecified comparator.
 */
export function outcomeComparatorToJson(op: OutcomeComparator): OutcomeComparatorJson {
  switch (op) {
    case OutcomeComparator.NE:
      return "ne";
    case OutcomeComparator.GT:
      return "gt";
    case OutcomeComparator.LT:
      return "lt";
    case OutcomeComparator.GTE:
      return "gte";
    case OutcomeComparator.LTE:
      return "lte";
    default:
      return "eq";
  }
}

export function outcomeComparatorFromJson(op: string | undefined): OutcomeComparator {
  switch (op) {
    case "ne":
      return OutcomeComparator.NE;
    case "gt":
      return OutcomeComparator.GT;
    case "lt":
      return OutcomeComparator.LT;
    case "gte":
      return OutcomeComparator.GTE;
    case "lte":
      return OutcomeComparator.LTE;
    case "eq":
      return OutcomeComparator.EQ;
    default:
      return OutcomeComparator.UNSPECIFIED;
  }
}

export type SamplingRuleTargetJson =
  | "request_tag"
  | "key_id"
  | "usage_pool_id";

export interface SamplingRuleJson {
  applies_to: SamplingRuleTargetJson;
  value: string;
  rate: number;
  log_content: boolean;
}

export function samplingRuleToJson(r: SamplingRule): SamplingRuleJson {
  const target = (() => {
    switch (r.appliesTo.case) {
      case "requestTag":
        return { applies_to: "request_tag" as const, value: r.appliesTo.value };
      case "keyId":
        return { applies_to: "key_id" as const, value: r.appliesTo.value };
      case "usagePoolId":
        return { applies_to: "usage_pool_id" as const, value: r.appliesTo.value };
      case undefined:
        throw new Error("sampling rule is missing its target");
    }
  })();
  return {
    ...target,
    rate: r.rate,
    log_content: r.logContent,
  };
}

export type UnsupportedOutcomeRuleValueJson = {
  unsupported: true;
  value: unknown;
};

export type OutcomeRuleValueJson =
  | string
  | number
  | boolean
  | null
  | UnsupportedOutcomeRuleValueJson;

export interface OutcomeRuleJson {
  path: string;
  op: OutcomeComparatorJson;
  // `google.protobuf.Value`'s own JSON representation is the plain scalar
  // (or `null`/an object/array), which is what `toJson(ValueSchema, ...)`
  // round-trips through — see `outcomeEditToValue` in the client for the
  // reverse conversion.
  value: OutcomeRuleValueJson;
  rate: number;
}

export function outcomeRuleToJson(r: OutcomeRule): OutcomeRuleJson {
  const value = r.value != null ? toJson(ValueSchema, r.value) : null;
  return {
    path: r.path,
    op: outcomeComparatorToJson(r.op),
    // Legacy structured values cannot be edited as scalar rules. Preserve the
    // original payload in a tagged form so the UI can require an explicit
    // replacement or removal instead of silently saving it as null.
    value:
      value !== null && typeof value === "object"
        ? { unsupported: true, value }
        : value,
    rate: r.rate,
  };
}

export interface SamplingPolicyJson {
  enabled: boolean;
  default_rate: number;
  default_log_content: boolean;
  min_conversation_rate: number;
  rules: SamplingRuleJson[];
  outcome_rules: OutcomeRuleJson[];
}

export function samplingPolicyToJson(p: SamplingPolicy): SamplingPolicyJson {
  return {
    enabled: p.enabled,
    default_rate: p.defaultRate,
    default_log_content: p.defaultLogContent,
    min_conversation_rate: p.minConversationRate,
    rules: p.rules.map(samplingRuleToJson),
    outcome_rules: p.outcomeRules.map(outcomeRuleToJson),
  };
}
