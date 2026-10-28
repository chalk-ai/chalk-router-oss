import { describe, expect, it } from "vitest";
import {
  connectionScopeModelError,
  directModelIds,
  EMPTY_KEY_ACCESS_DRAFT,
  keyAccessDraftFromPermission,
  keyAccessDraftGrantsNothing,
  keyAccessError,
  keyAccessKind,
  keyAccessPayload,
  keyReachesRoute,
  routeAccessForKey,
  routeGrantLabels,
  type KeyAccessDraft,
} from "./keyAccess";

const restricted = (draft: Partial<KeyAccessDraft>): KeyAccessDraft => ({ mode: "restricted", routeIds: [], scopes: [], ...draft });

describe("connection scope model rule", () => {
  it("accepts an exact direct model id", () => {
    expect(connectionScopeModelError("gpt-4o")).toBeNull();
    expect(connectionScopeModelError("openai/gpt-4o")).toBeNull();
  });

  it("rejects wildcards", () => {
    expect(connectionScopeModelError("openai/*")).toMatch(/wildcard/);
  });

  it("rejects chalk/* model routes, case-sensitively like the server", () => {
    expect(connectionScopeModelError("chalk/assistant")).toMatch(/model route/);
    expect(connectionScopeModelError("Chalk/assistant")).toBeNull();
  });

  it("drops route aliases from a connection's model choices", () => {
    expect(directModelIds(["chalk/a", "gpt-4o", "claude"])).toEqual(["gpt-4o", "claude"]);
  });
});

describe("keyAccessError", () => {
  it("accepts all models regardless of leftover restricted selections", () => {
    expect(keyAccessError({ mode: "all", routeIds: [], scopes: [{ id: 0, connectionId: "", restrictModels: true, models: [] }] })).toBeNull();
  });

  it("accepts restricted with nothing selected", () => {
    expect(keyAccessError(restricted({}))).toBeNull();
  });

  it("accepts routes and valid connection scopes", () => {
    expect(keyAccessError(restricted({
      routeIds: ["route-1"],
      scopes: [
        { id: 0, connectionId: "openai", restrictModels: false, models: [] },
        { id: 1, connectionId: "anthropic", restrictModels: true, models: ["claude"] },
      ],
    }))).toBeNull();
  });

  it("requires a connection on every scope", () => {
    expect(keyAccessError(restricted({ scopes: [{ id: 0, connectionId: "", restrictModels: false, models: [] }] }))).toMatch(/connection/);
  });

  it("requires a model on every specific-model scope", () => {
    expect(keyAccessError(restricted({ scopes: [{ id: 0, connectionId: "c", restrictModels: true, models: [] }] }))).toMatch(/at least one model/);
  });

  it("rejects wildcards and chalk/* ids in a specific-model scope", () => {
    expect(keyAccessError(restricted({ scopes: [{ id: 0, connectionId: "c", restrictModels: true, models: ["openai/*"] }] }))).toMatch(/wildcard/);
    expect(keyAccessError(restricted({ scopes: [{ id: 0, connectionId: "c", restrictModels: true, models: ["chalk/a"] }] }))).toMatch(/model route/);
  });

  it("ignores stale models on an all-direct-models scope, which sends none", () => {
    expect(keyAccessError(restricted({ scopes: [{ id: 0, connectionId: "c", restrictModels: false, models: ["chalk/a"] }] }))).toBeNull();
  });
});

describe("keyAccessPayload", () => {
  it("sends neither routes nor scopes for all models", () => {
    expect(keyAccessPayload({ mode: "all", routeIds: ["r"], scopes: [{ id: 0, connectionId: "c", restrictModels: false, models: [] }] })).toEqual({});
  });

  it("always sends routes when restricted, even with nothing selected", () => {
    expect(keyAccessPayload(restricted({}))).toEqual({ routes: { refs: [] }, connection_scopes: [] });
  });

  it("sends route ids once and only the models of specific scopes", () => {
    expect(keyAccessPayload(restricted({
      routeIds: ["r1", "r2", "r1"],
      scopes: [
        { id: 0, connectionId: "a", restrictModels: false, models: ["stale"] },
        { id: 1, connectionId: "b", restrictModels: true, models: ["m"] },
      ],
    }))).toEqual({
      routes: { refs: [{ route_id: "r1" }, { route_id: "r2" }] },
      connection_scopes: [
        { connection_id: "a", models: [] },
        { connection_id: "b", models: ["m"] },
      ],
    });
  });
});

describe("reading a key's access", () => {
  it("classifies unrestricted, no-access and restricted keys", () => {
    expect(keyAccessKind({})).toBe("all");
    expect(keyAccessKind({ routes: null, connection_scopes: [] })).toBe("all");
    expect(keyAccessKind({ routes: { refs: [] }, connection_scopes: [] })).toBe("none");
    expect(keyAccessKind({ routes: { refs: [{ route_id: "r" }] } })).toBe("restricted");
    expect(keyAccessKind({ routes: { refs: [] }, connection_scopes: [{ connection_id: "c" }] })).toBe("restricted");
  });

  it("round-trips a no-access key without widening it", () => {
    const draft = keyAccessDraftFromPermission({ routes: { refs: [] }, connection_scopes: [] });
    expect(draft.mode).toBe("restricted");
    expect(keyAccessDraftGrantsNothing(draft)).toBe(true);
    expect(keyAccessPayload(draft)).toEqual({ routes: { refs: [] }, connection_scopes: [] });
  });

  it("round-trips an unrestricted key as all models", () => {
    const draft = keyAccessDraftFromPermission({ routes: null, connection_scopes: [] });
    expect(draft).toEqual(EMPTY_KEY_ACCESS_DRAFT);
    expect(keyAccessPayload(draft)).toEqual({});
  });

  it("keeps granted route ids, including deleted ones, and scope models", () => {
    const draft = keyAccessDraftFromPermission({
      routes: { refs: [{ route_id: "live" }, { route_id: "deleted" }] },
      connection_scopes: [{ connection_id: "c", models: ["m"] }, { connection_id: "d", models: [] }],
    });
    expect(keyAccessPayload(draft)).toEqual({
      routes: { refs: [{ route_id: "live" }, { route_id: "deleted" }] },
      connection_scopes: [{ connection_id: "c", models: ["m"] }, { connection_id: "d", models: [] }],
    });
  });

  it("labels granted routes by public id and missing ones as deleted", () => {
    const routes = new Map([["live", { id: "live", public_model_id: "chalk/live" }]]);
    expect(routeGrantLabels(["live", "gone"], routes)).toEqual([
      { id: "live", publicModelId: "chalk/live" },
      { id: "gone", publicModelId: null },
    ]);
  });
});

describe("reachable keys for a route", () => {
  it("reaches the route when unrestricted", () => {
    expect(keyReachesRoute({}, "r")).toBe(true);
    expect(routeAccessForKey({}, "r")).toEqual({ allowed: true, reason: "All models" });
  });

  it("reaches the route when the route id is granted", () => {
    expect(routeAccessForKey({ routes: { refs: [{ route_id: "r" }] } }, "r")).toEqual({ allowed: true, reason: "Route granted" });
  });

  it("does not reach the route through a connection scope", () => {
    const permission = { routes: { refs: [] }, connection_scopes: [{ connection_id: "c", models: [] }] };
    expect(keyReachesRoute(permission, "r")).toBe(false);
    expect(routeAccessForKey(permission, "r")).toEqual({ allowed: false, reason: "Route not granted" });
  });

  it("does not reach any route with no access", () => {
    expect(routeAccessForKey({ routes: { refs: [] } }, "r")).toEqual({ allowed: false, reason: "No access" });
  });

  it("matches by route id, not public model id", () => {
    expect(keyReachesRoute({ routes: { refs: [{ route_id: "chalk/r" }] } }, "r")).toBe(false);
  });
});
