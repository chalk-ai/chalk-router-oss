import { describe, expect, it } from "vitest";
import {
  I64_MAX,
  hasOwn,
  isRateLimitType,
  keyAccessFromJson,
  parseJsonObjectBody,
  parseOptionalPositiveBigInt,
  parsePositiveBigInt,
  rateLimitsFromJson,
} from "./bff";

describe("parsePositiveBigInt", () => {
  it("accepts a positive numeric string", () => {
    const result = parsePositiveBigInt("42", "field");
    expect(result).toEqual({ value: 42n });
  });

  it("accepts a positive number", () => {
    const result = parsePositiveBigInt(42, "field");
    expect(result).toEqual({ value: 42n });
  });

  it("accepts the exact I64_MAX boundary", () => {
    const result = parsePositiveBigInt(I64_MAX.toString(), "field");
    expect(result).toEqual({ value: I64_MAX });
  });

  it("rejects a value one past I64_MAX", () => {
    const result = parsePositiveBigInt((I64_MAX + 1n).toString(), "field");
    expect("error" in result).toBe(true);
  });

  it("rejects zero", () => {
    const result = parsePositiveBigInt(0, "field");
    expect("error" in result).toBe(true);
  });

  it("rejects a negative number", () => {
    const result = parsePositiveBigInt(-5, "field");
    expect("error" in result).toBe(true);
  });

  it("rejects a non-integer number", () => {
    const result = parsePositiveBigInt(4.5, "field");
    expect("error" in result).toBe(true);
  });

  it("rejects a non-numeric string", () => {
    const result = parsePositiveBigInt("abc", "field");
    expect("error" in result).toBe(true);
  });

  it("rejects booleans and objects", () => {
    expect("error" in parsePositiveBigInt(true, "field")).toBe(true);
    expect("error" in parsePositiveBigInt({}, "field")).toBe(true);
  });

  it("includes the field name in the error message", async () => {
    const result = parsePositiveBigInt(-1, "token_limit");
    if (!("error" in result)) throw new Error("expected an error result");
    const body = (await result.error.json()) as { error: { message: string } };
    expect(body.error.message).toContain("token_limit");
  });
});

describe("parseOptionalPositiveBigInt", () => {
  it("passes null and undefined through as undefined", () => {
    expect(parseOptionalPositiveBigInt(null, "field")).toEqual({ value: undefined });
    expect(parseOptionalPositiveBigInt(undefined, "field")).toEqual({ value: undefined });
  });

  it("still validates a present value", () => {
    expect(parseOptionalPositiveBigInt("10", "field")).toEqual({ value: 10n });
    expect("error" in parseOptionalPositiveBigInt("-10", "field")).toBe(true);
  });
});

describe("isRateLimitType", () => {
  it("accepts the three known limit types", () => {
    expect(isRateLimitType("requests_per_minute")).toBe(true);
    expect(isRateLimitType("tokens_per_minute")).toBe(true);
    expect(isRateLimitType("concurrent_requests")).toBe(true);
  });

  it("rejects anything else", () => {
    expect(isRateLimitType("requests_per_second")).toBe(false);
    expect(isRateLimitType(42)).toBe(false);
    expect(isRateLimitType(undefined)).toBe(false);
  });
});

describe("hasOwn", () => {
  it("is true for an own property even when its value is null", () => {
    expect(hasOwn({ enabled: null }, "enabled")).toBe(true);
  });

  it("is false for a missing key", () => {
    expect(hasOwn({}, "enabled")).toBe(false);
  });
});

describe("parseJsonObjectBody", () => {
  function req(body: unknown): Request {
    return new Request("http://localhost/x", {
      method: "POST",
      body: JSON.stringify(body),
      headers: { "content-type": "application/json" },
    });
  }

  it("accepts a plain object body", async () => {
    const result = await parseJsonObjectBody(req({ a: 1 }));
    expect("body" in result && result.body).toEqual({ a: 1 });
  });

  it("rejects a null body", async () => {
    const result = await parseJsonObjectBody(req(null));
    expect("error" in result).toBe(true);
  });

  it("rejects an array body", async () => {
    const result = await parseJsonObjectBody(req([1, 2, 3]));
    expect("error" in result).toBe(true);
  });

  it("rejects a primitive body", async () => {
    const result = await parseJsonObjectBody(req("just a string"));
    expect("error" in result).toBe(true);
  });
});

describe("rateLimitsFromJson", () => {
  it("defaults to an empty array when the field is absent", () => {
    expect(rateLimitsFromJson(undefined)).toEqual({ value: [] });
  });

  it("rejects a non-array value", () => {
    expect("error" in rateLimitsFromJson({})).toBe(true);
  });

  it("parses a full valid entry, trimming a padded connection_id", () => {
    const result = rateLimitsFromJson([
      {
        connection_id: " conn-1 ",
        models: ["gpt-4"],
        requests_per_minute: 60,
        request_burst: "10",
        tokens_per_minute: null,
        token_burst: undefined,
        concurrent_requests: 5,
        enabled: false,
      },
    ]);
    expect(result).toEqual({
      value: [
        {
          connectionId: "conn-1",
          models: ["gpt-4"],
          requestsPerMinute: 60n,
          requestBurst: 10n,
          tokensPerMinute: undefined,
          tokenBurst: undefined,
          concurrentRequests: 5n,
          enabled: false,
        },
      ],
    });
  });

  it("treats a blank connection_id as absent", () => {
    const result = rateLimitsFromJson([{ connection_id: "   " }]);
    if (!("value" in result)) throw new Error("expected a value result");
    expect(result.value[0].connectionId).toBeUndefined();
  });

  it("rejects a non-object array entry", () => {
    expect("error" in rateLimitsFromJson([null])).toBe(true);
    expect("error" in rateLimitsFromJson(["nope"])).toBe(true);
  });

  it("rejects a non-string connection_id", () => {
    expect("error" in rateLimitsFromJson([{ connection_id: 5 }])).toBe(true);
  });

  it("rejects a models array with a non-string entry", () => {
    expect("error" in rateLimitsFromJson([{ models: ["ok", 5] }])).toBe(true);
  });

  it("rejects a wildcard model with its indexed path", async () => {
    const result = rateLimitsFromJson([
      { connection_id: "openai", models: ["openai/*"], requests_per_minute: 1 },
    ]);
    if (!("error" in result)) throw new Error("expected an error result");
    expect(result.error.status).toBe(400);
    expect((await result.error.json()).error.message).toContain("rate_limits[0].models");
  });

  it("rejects a non-boolean enabled", () => {
    expect("error" in rateLimitsFromJson([{ enabled: "yes" }])).toBe(true);
  });

  it("propagates a numeric field validation error with its indexed path", async () => {
    const result = rateLimitsFromJson([{ requests_per_minute: -1 }], "rate_limits");
    if (!("error" in result)) throw new Error("expected an error result");
    const body = (await result.error.json()) as { error: { message: string } };
    expect(body.error.message).toContain("rate_limits[0].requests_per_minute");
  });
});

describe("keyAccessFromJson", () => {
  async function errorMessage(result: ReturnType<typeof keyAccessFromJson>): Promise<string> {
    if (!("error" in result)) throw new Error("expected an error result");
    expect(result.error.status).toBe(400);
    const body = (await result.error.json()) as { error: { message: string } };
    return body.error.message;
  }

  it("rejects each legacy field, even when empty", async () => {
    expect(await errorMessage(keyAccessFromJson({ provider: "openai" }))).toContain("permission.provider");
    expect(await errorMessage(keyAccessFromJson({ connection_id: "c" }))).toContain("permission.connection_id");
    expect(await errorMessage(keyAccessFromJson({ models: [] }))).toContain("permission.models");
  });

  it("rejects wildcards and chalk/* ids in connection scopes", async () => {
    expect(await errorMessage(keyAccessFromJson({ connection_scopes: [{ connection_id: "c", models: ["openai/*"] }] })))
      .toContain("permission.connection_scopes[0].models");
    expect(await errorMessage(keyAccessFromJson({ connection_scopes: [{ connection_id: "c", models: ["chalk/a"] }] })))
      .toMatch(/model route/);
  });

  it("rejects malformed scopes and route ids", async () => {
    expect(await errorMessage(keyAccessFromJson({ connection_scopes: {} }))).toContain("connection_scopes");
    expect(await errorMessage(keyAccessFromJson({ connection_scopes: [{ connection_id: "" }] }))).toContain("connection_id");
    expect(await errorMessage(keyAccessFromJson({ routes: [] }))).toContain("permission.routes");
    expect(await errorMessage(keyAccessFromJson({ routes: { refs: [{ route_id: "ok" }, { route_id: "" }] } }))).toContain("route_id");
    expect(await errorMessage(keyAccessFromJson({ routes: { refs: [{ route_id: 1 }] } }))).toContain("route_id");
  });

  it("requires exactly one non-empty exact identifier per route", async () => {
    for (const identifier of [{}, null, [], "r", { route_id: "r", public_model_id: "chalk/a" }, { route_id: "" }, { public_model_id: " " }, { public_model_id: "chalk/*" }]) {
      expect(await errorMessage(keyAccessFromJson({ routes: { refs: [identifier] } }))).toContain("permission.routes.refs[0]");
    }
    expect(await errorMessage(keyAccessFromJson({ routes: { refs: {} } }))).toContain("permission.routes.refs");
    expect(await errorMessage(keyAccessFromJson({ routes: { route_ids: ["r"] } }))).toContain("is no longer accepted");
  });

  it("preserves both identifier variants", () => {
    expect(keyAccessFromJson({ routes: { refs: [{ public_model_id: "chalk/assistant-pro" }, { route_id: "r" }] } })).toEqual({
      value: { routes: { refs: [{ identifier: { case: "publicModelId", value: "chalk/assistant-pro" } }, { identifier: { case: "routeId", value: "r" } }] }, connectionScopes: [] },
    });
  });

  it("passes routes and scopes through", () => {
    expect(keyAccessFromJson({
      routes: { refs: [{ route_id: "r1" }, { route_id: "r2" }] },
      connection_scopes: [{ connection_id: "c", models: ["m"] }, { connection_id: "d" }],
    })).toEqual({
      value: {
        routes: { refs: [{ identifier: { case: "routeId", value: "r1" } }, { identifier: { case: "routeId", value: "r2" } }] },
        connectionScopes: [{ connectionId: "c", models: ["m"] }, { connectionId: "d", models: [] }],
      },
    });
  });

  it("keeps empty routes, which is how a key with no access stays restricted", () => {
    expect(keyAccessFromJson({ routes: { refs: [] } })).toEqual({ value: { routes: { refs: [] }, connectionScopes: [] } });
    expect(keyAccessFromJson({ routes: {} })).toEqual({ value: { routes: { refs: [] }, connectionScopes: [] } });
  });

  it("leaves routes absent for an unrestricted key", () => {
    expect(keyAccessFromJson({})).toEqual({ value: { routes: undefined, connectionScopes: [] } });
  });
});
