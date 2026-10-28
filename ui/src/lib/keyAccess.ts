/**
 * What an API key may call, as the admin UI reads, edits and validates it.
 *
 * A key's access is the union of two independent grants:
 *
 * - model routes (`chalk/*` aliases), by immutable route id: a grant covers
 *   everything the route dispatches to (default target, judge, fallbacks);
 * - connection scopes, which allow direct model ids on one connection (an
 *   empty model list means every direct model there). A connection scope
 *   never grants a `chalk/*` id.
 *
 * A key is unrestricted iff it has no `routes` and no connection scopes. The
 * server sets `routes` on every restricted key, so `routes` present with no
 * ids and no connection scopes means the key can call nothing — which is
 * also how the server presents a key whose stored permission is unreadable.
 * The UI shows that state as "No access" and saves it back unchanged.
 *
 * Both the issue form (`IssueTokenForm`) and the edit form (`TokensClient`'s
 * `EditTokenForm`) build drafts with these helpers, and the BFF
 * (`lib/bff.ts::keyAccessFromJson`) applies the same model rule, so the
 * browser, BFF and server agree on what is valid.
 */

/** Prefix the router reserves for model route aliases (`ROUTE_RESERVED_PREFIX`). */
export const MODEL_ROUTE_PREFIX = "chalk/";

/** True for a `chalk/*` model route alias (case-sensitive, like the server). */
export function isModelRouteId(model: string): boolean {
  return model.startsWith(MODEL_ROUTE_PREFIX);
}

/** Drops `chalk/*` aliases from a connection's model list: connection scopes grant direct models only. */
export function directModelIds(models: readonly string[]): string[] {
  return models.filter((model) => !isModelRouteId(model));
}

/**
 * Why `model` cannot appear in a connection scope's model list, or null.
 * Mirrors `validate_connection_scopes` in `src/service/api_keys.rs`.
 */
export function connectionScopeModelError(model: string): string | null {
  if (model.includes("*")) {
    return `model '${model}' contains a wildcard; model lists match exact public model ids. Leave models empty to allow every model on the connection`;
  }
  if (isModelRouteId(model)) {
    return `'${model}' is a model route; grant it as a model route instead of a connection scope`;
  }
  return null;
}

/* ------------------------------------------------------------------ */
/*  JSON shape served by /api/admin/keys (lib/serialize.ts)           */
/* ------------------------------------------------------------------ */

export interface ConnectionScopeJson {
  connection_id: string;
  models?: string[];
}

export interface KeyAccessJson {
  connection_scopes?: ConnectionScopeJson[];
  /** Absent or null only on an unrestricted key. */
  routes?: { refs: { route_id: string }[] } | null;
}

export type KeyAccessKind = "all" | "none" | "restricted";

export function keyAccessKind(permission: KeyAccessJson | undefined): KeyAccessKind {
  const scopes = permission?.connection_scopes ?? [];
  if (!permission?.routes && scopes.length === 0) return "all";
  if ((permission?.routes?.refs.length ?? 0) === 0 && scopes.length === 0) return "none";
  return "restricted";
}

export function grantedRouteIds(permission: KeyAccessJson | undefined): string[] {
  return permission?.routes?.refs.map((identifier) => identifier.route_id) ?? [];
}

/**
 * Whether a key may call the model route `routeId`: it is unrestricted, or it
 * grants the route by id. Connection scopes never reach a route, and a route
 * grant covers its judge selections and fallbacks on any connection.
 */
export function keyReachesRoute(permission: KeyAccessJson | undefined, routeId: string): boolean {
  return keyAccessKind(permission) === "all" || grantedRouteIds(permission).includes(routeId);
}

/** {@link keyReachesRoute} with a short reason for the route detail page. */
export function routeAccessForKey(
  permission: KeyAccessJson | undefined,
  routeId: string,
): { allowed: boolean; reason: string } {
  const kind = keyAccessKind(permission);
  if (kind === "all") return { allowed: true, reason: "All models" };
  if (grantedRouteIds(permission).includes(routeId)) return { allowed: true, reason: "Route granted" };
  if (kind === "none") return { allowed: false, reason: "No access" };
  return { allowed: false, reason: "Route not granted" };
}

export interface RouteOption {
  id: string;
  public_model_id: string;
  published?: boolean;
}

export interface RouteGrantLabel {
  id: string;
  /** The route's public id, or null when no route has this id any more. */
  publicModelId: string | null;
}

/** Resolves granted route ids to public ids; a missing route is a deleted one. */
export function routeGrantLabels(routeIds: readonly string[], routesById: ReadonlyMap<string, RouteOption>): RouteGrantLabel[] {
  return routeIds.map((id) => ({ id, publicModelId: routesById.get(id)?.public_model_id ?? null }));
}

/* ------------------------------------------------------------------ */
/*  Form drafts                                                       */
/* ------------------------------------------------------------------ */

export interface ConnectionScopeDraft {
  id: number;
  connectionId: string;
  /** True when this scope is limited to `models` rather than every direct model on the connection. */
  restrictModels: boolean;
  models: string[];
}

export interface KeyAccessDraft {
  mode: "all" | "restricted";
  routeIds: string[];
  scopes: ConnectionScopeDraft[];
}

export const EMPTY_KEY_ACCESS_DRAFT: KeyAccessDraft = { mode: "all", routeIds: [], scopes: [] };

export function keyAccessDraftFromPermission(permission: KeyAccessJson | undefined): KeyAccessDraft {
  return {
    mode: keyAccessKind(permission) === "all" ? "all" : "restricted",
    routeIds: [...grantedRouteIds(permission)],
    scopes: (permission?.connection_scopes ?? []).map((scope, id) => ({
      id,
      connectionId: scope.connection_id,
      restrictModels: (scope.models?.length ?? 0) > 0,
      models: scope.models ?? [],
    })),
  };
}

/** True when a restricted draft grants nothing: the key will not be able to call any model. */
export function keyAccessDraftGrantsNothing(draft: KeyAccessDraft): boolean {
  return draft.mode === "restricted" && draft.routeIds.length === 0 && draft.scopes.length === 0;
}

/** The first problem with a draft, worded for the form, or null when it can be saved. */
export function keyAccessError(draft: KeyAccessDraft): string | null {
  if (draft.mode === "all") return null;
  if (draft.routeIds.some((id) => !id.trim())) {
    return "Every model route grant needs a route.";
  }
  if (draft.scopes.some((scope) => !scope.connectionId)) {
    return "Choose a connection for every connection scope.";
  }
  if (draft.scopes.some((scope) => scope.restrictModels && scope.models.length === 0)) {
    return "Choose at least one model for every specific-model scope.";
  }
  for (const scope of draft.scopes) {
    if (!scope.restrictModels) continue;
    for (const model of scope.models) {
      const error = connectionScopeModelError(model);
      if (error) return `Connection scope: ${error}.`;
    }
  }
  return null;
}

/** JSON `permission` access fields for the BFF. */
export interface KeyAccessPayload {
  routes?: { refs: ({ route_id: string; public_model_id?: never } | { public_model_id: string; route_id?: never })[] };
  connection_scopes?: { connection_id: string; models: string[] }[];
}

/**
 * The access part of a `permission` body. "All models" sends neither field;
 * "Restricted" always sends `routes`, even with no ids, so a restricted key
 * with nothing selected stays restricted instead of widening to every model.
 */
export function keyAccessPayload(draft: KeyAccessDraft): KeyAccessPayload {
  if (draft.mode === "all") return {};
  return {
    routes: { refs: [...new Set(draft.routeIds)].map((route_id) => ({ route_id })) },
    connection_scopes: draft.scopes.map((scope) => ({
      connection_id: scope.connectionId,
      models: scope.restrictModels ? scope.models : [],
    })),
  };
}
