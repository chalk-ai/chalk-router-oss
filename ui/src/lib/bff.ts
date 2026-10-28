import { NextResponse } from "next/server";
import { connectionScopeModelError } from "./keyAccess";

/**
 * Field-presence check shared by the `/api/admin/*` PATCH routes: `true` iff
 * `key` is an own property of `body` (regardless of its value, including
 * `null`), matching the "field mask" semantics those routes implement
 * (present-but-null means "clear").
 */
export function hasOwn(body: object, key: string): boolean {
  return Object.hasOwn(body, key);
}

/** A JSON value that behaves like a plain object for field-presence checks. */
export type JsonObject = Record<string, unknown>;

/**
 * Parse `req.json()` and reject anything that isn't a plain JSON object
 * (`null`, an array, or a primitive) before any field-presence checks run.
 * Returns either the parsed body or a ready-to-return 400 `NextResponse`.
 */
export async function parseJsonObjectBody(
  req: Request,
): Promise<{ body: JsonObject } | { error: NextResponse }> {
  const body: unknown = await req.json();
  if (body === null || typeof body !== "object" || Array.isArray(body)) {
    return {
      error: NextResponse.json(
        { error: { message: "body must be a JSON object" } },
        { status: 400 },
      ),
    };
  }
  return { body: body as JsonObject };
}

/** Result shape shared by the parsers/mappers below: a parsed value, or a ready-to-return 400. */
export type BffResult<T> = { value: T } | { error: NextResponse };

export function invalidField(field: string, message: string): { error: NextResponse } {
  return {
    error: NextResponse.json({ error: { message: `${field} ${message}` } }, { status: 400 }),
  };
}

/**
 * Largest value the server's PostgreSQL BIGINT (Rust `i64`) columns can
 * store. Mirrors the `> i64::MAX as u64` checks in `src/service/rate_limits.rs`.
 */
export const I64_MAX = 9223372036854775807n;

/**
 * Parse a required positive integer from a JSON number or numeric string,
 * bounded to `I64_MAX`. Consolidates the several incompatible ad hoc
 * bigint parsers previously duplicated across the `/api/admin/*` routes.
 */
export function parsePositiveBigInt(value: unknown, field: string): BffResult<bigint> {
  if (typeof value !== "string" && typeof value !== "number") {
    return invalidField(field, "must be a positive integer");
  }
  if (typeof value === "number" && !Number.isSafeInteger(value)) {
    return invalidField(field, "must be a safe integer");
  }
  if (!/^\d+$/.test(String(value))) {
    return invalidField(field, "must be a positive integer");
  }
  const parsed = BigInt(value);
  if (parsed <= 0n || parsed > I64_MAX) {
    return invalidField(field, "must be a positive integer");
  }
  return { value: parsed };
}

/** Same as {@link parsePositiveBigInt}, but `null`/`undefined` pass through as `undefined`. */
export function parseOptionalPositiveBigInt(
  value: unknown,
  field: string,
): BffResult<bigint | undefined> {
  if (value == null) return { value: undefined };
  return parsePositiveBigInt(value, field);
}

/** The three `limit_type` values the server accepts (`src/service/rate_limits.rs`). */
export const RATE_LIMIT_TYPES = ["requests_per_minute", "tokens_per_minute", "concurrent_requests"] as const;
export type RateLimitTypeJson = (typeof RATE_LIMIT_TYPES)[number];

export function isRateLimitType(value: unknown): value is RateLimitTypeJson {
  return typeof value === "string" && (RATE_LIMIT_TYPES as readonly string[]).includes(value);
}

/** Mirrors the exact model-selector validation shared by Rust rate-limit editors. */
export function validateRateLimitModels(models: string[], field = "models"): BffResult<string[]> {
  const wildcard = models.find((model) => model.includes("*"));
  if (wildcard !== undefined) {
    return invalidField(
      field,
      `entry '${wildcard}' contains a wildcard; model lists match exact public model ids (omit models to cover the whole connection)`,
    );
  }
  return { value: models };
}

interface RateLimitJson {
  connection_id?: string | null;
  models?: string[];
  requests_per_minute?: number | string | null;
  request_burst?: number | string | null;
  tokens_per_minute?: number | string | null;
  token_burst?: number | string | null;
  concurrent_requests?: number | string | null;
  enabled?: boolean;
}

export interface RateLimitProtoInput {
  connectionId?: string;
  models: string[];
  requestsPerMinute?: bigint;
  requestBurst?: bigint;
  tokensPerMinute?: bigint;
  tokenBurst?: bigint;
  concurrentRequests?: bigint;
  enabled?: boolean;
}

/**
 * Parse the JSON shape of a nested `rate_limits` array (used by API keys and
 * usage pools) into the gRPC-client shape, validating each numeric field.
 * Consolidates the three previously-duplicated copies of this mapping
 * (`usage-pools/payload.ts`, and the inline versions in `keys/route.ts` and
 * `keys/[key]/route.ts`).
 */
export function rateLimitsFromJson(value: unknown, field = "rate_limits"): BffResult<RateLimitProtoInput[]> {
  if (value === undefined) return { value: [] };
  if (!Array.isArray(value)) {
    return invalidField(field, "must be an array");
  }
  const result: RateLimitProtoInput[] = [];
  for (let index = 0; index < value.length; index++) {
    const raw = value[index];
    const entryField = `${field}[${index}]`;
    if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
      return invalidField(entryField, "must be an object");
    }
    const limit = raw as RateLimitJson;
    if (limit.connection_id != null && typeof limit.connection_id !== "string") {
      return invalidField(`${entryField}.connection_id`, "must be a string");
    }
    if (
      limit.models != null &&
      (!Array.isArray(limit.models) || !limit.models.every((model) => typeof model === "string"))
    ) {
      return invalidField(`${entryField}.models`, "must be an array of strings");
    }
    const models = validateRateLimitModels(limit.models ?? [], `${entryField}.models`);
    if ("error" in models) return models;
    if (limit.enabled != null && typeof limit.enabled !== "boolean") {
      return invalidField(`${entryField}.enabled`, "must be a boolean");
    }
    const requestsPerMinute = parseOptionalPositiveBigInt(limit.requests_per_minute, `${entryField}.requests_per_minute`);
    if ("error" in requestsPerMinute) return requestsPerMinute;
    const requestBurst = parseOptionalPositiveBigInt(limit.request_burst, `${entryField}.request_burst`);
    if ("error" in requestBurst) return requestBurst;
    const tokensPerMinute = parseOptionalPositiveBigInt(limit.tokens_per_minute, `${entryField}.tokens_per_minute`);
    if ("error" in tokensPerMinute) return tokensPerMinute;
    const tokenBurst = parseOptionalPositiveBigInt(limit.token_burst, `${entryField}.token_burst`);
    if ("error" in tokenBurst) return tokenBurst;
    const concurrentRequests = parseOptionalPositiveBigInt(limit.concurrent_requests, `${entryField}.concurrent_requests`);
    if ("error" in concurrentRequests) return concurrentRequests;
    result.push({
      connectionId: typeof limit.connection_id === "string" && limit.connection_id.trim() ? limit.connection_id.trim() : undefined,
      models: models.value,
      requestsPerMinute: requestsPerMinute.value,
      requestBurst: requestBurst.value,
      tokensPerMinute: tokensPerMinute.value,
      tokenBurst: tokenBurst.value,
      concurrentRequests: concurrentRequests.value,
      enabled: limit.enabled,
    });
  }
  return { value: result };
}

/**
 * Legacy `permission` fields the server rejects (INVALID_ARGUMENT). The BFF
 * rejects them first, with the same guidance, so an older client cannot
 * create an unrestricted key by having them dropped on the way through.
 */
export const LEGACY_PERMISSION_FIELDS = ["provider", "models", "connection_id"] as const;

export interface KeyAccessProtoInput {
  connectionScopes: { connectionId: string; models: string[] }[];
  /** Present on every restricted key, even with no route ids. */
  routes?: { refs: { identifier: { case: "routeId" | "publicModelId"; value: string } }[] };
}

/**
 * Parse the access fields of an API-key `permission` body
 * (`connection_scopes` and `routes.refs`) into the gRPC-client shape.
 * Rejects the legacy fields and applies the server's connection-scope model
 * rule (no wildcards, no `chalk/*` route ids). `routes` passes through as
 * given — including empty `refs` — because its presence is what keeps
 * a key restricted.
 */
export function keyAccessFromJson(permission: JsonObject, field = "permission"): BffResult<KeyAccessProtoInput> {
  for (const legacy of LEGACY_PERMISSION_FIELDS) {
    if (hasOwn(permission, legacy)) {
      return invalidField(
        `${field}.${legacy}`,
        "is no longer accepted; grant direct models with permission.connection_scopes and model routes with permission.routes",
      );
    }
  }

  const connectionScopes: KeyAccessProtoInput["connectionScopes"] = [];
  const rawScopes = permission.connection_scopes;
  if (rawScopes != null) {
    if (!Array.isArray(rawScopes)) return invalidField(`${field}.connection_scopes`, "must be an array");
    for (let index = 0; index < rawScopes.length; index++) {
      const scopeField = `${field}.connection_scopes[${index}]`;
      const raw: unknown = rawScopes[index];
      if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
        return invalidField(scopeField, "must be an object");
      }
      const scope = raw as { connection_id?: unknown; models?: unknown };
      if (typeof scope.connection_id !== "string" || !scope.connection_id.trim()) {
        return invalidField(`${scopeField}.connection_id`, "must be a non-empty string");
      }
      if (
        scope.models != null &&
        (!Array.isArray(scope.models) || !scope.models.every((model) => typeof model === "string" && model.length > 0))
      ) {
        return invalidField(`${scopeField}.models`, "must be an array of non-empty strings");
      }
      const models = (scope.models as string[] | null | undefined) ?? [];
      for (const model of models) {
        const error = connectionScopeModelError(model);
        if (error) return invalidField(`${scopeField}.models`, error);
      }
      connectionScopes.push({ connectionId: scope.connection_id, models });
    }
  }

  let routes: KeyAccessProtoInput["routes"];
  const rawRoutes = permission.routes;
  if (rawRoutes != null) {
    if (typeof rawRoutes !== "object" || Array.isArray(rawRoutes)) {
      return invalidField(`${field}.routes`, "must be an object");
    }
    const scope = rawRoutes as JsonObject;
    if (hasOwn(scope, "route_ids")) {
      return invalidField(`${field}.routes.route_ids`, "is no longer accepted; use refs with route_id or public_model_id");
    }
    const rawRefs = scope.refs ?? [];
    if (!Array.isArray(rawRefs)) return invalidField(`${field}.routes.refs`, "must be an array");
    routes = { refs: [] };
    for (let index = 0; index < rawRefs.length; index++) {
      const path = `${field}.routes.refs[${index}]`;
      const raw: unknown = rawRefs[index];
      if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return invalidField(path, "must be an object");
      const identifier = raw as JsonObject;
      const byId = hasOwn(identifier, "route_id");
      const byName = hasOwn(identifier, "public_model_id");
      if (byId === byName) return invalidField(path, "select exactly one of route_id or public_model_id");
      const name = byId ? "route_id" : "public_model_id";
      const value = identifier[name];
      if (typeof value !== "string" || !value.trim() || value.includes("*")) {
        return invalidField(`${path}.${name}`, "must be a non-empty exact route identifier");
      }
      routes.refs.push({ identifier: { case: byId ? "routeId" : "publicModelId", value } });
    }
  }

  return { value: { connectionScopes, routes } };
}
