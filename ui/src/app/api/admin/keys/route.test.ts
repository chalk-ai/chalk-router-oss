import { beforeEach, describe, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { create } from "@bufbuild/protobuf";
import { ApiKeySchema } from "@/gen/chalk/router/v1/api_key_pb";
import { POST } from "./route";
import { PATCH } from "./[key]/route";

const client = vi.hoisted(() => ({ createKey: vi.fn(), updateKey: vi.fn() }));
vi.mock("@/lib/grpc", () => ({ apiKeyClient: () => client }));

const request = (body: unknown, method = "POST") =>
  new NextRequest("http://localhost/api/admin/keys", { method, body: JSON.stringify(body), headers: { "content-type": "application/json" } });
const params = { params: Promise.resolve({ key: "hash" }) };

beforeEach(() => {
  vi.resetAllMocks();
  client.createKey.mockImplementation(async (input) => ({ key: create(ApiKeySchema, { keyHash: "hash", description: input.description, permission: input.permission }), rawKey: "raw" }));
  client.updateKey.mockImplementation(async (input) => ({ key: create(ApiKeySchema, { keyHash: input.key, permission: input.update.permission }) }));
});

describe("API key BFF access", () => {
  it("passes model routes and connection scopes through on create", async () => {
    const response = await POST(request({
      description: "key",
      permission: { routes: { refs: [{ route_id: "route-1" }] }, connection_scopes: [{ connection_id: "c", models: ["gpt-4o"] }] },
    }));
    expect(response.status).toBe(201);
    const input = client.createKey.mock.calls[0][0];
    expect(input.permission.routes).toEqual({ refs: [{ identifier: { case: "routeId", value: "route-1" } }] });
    expect(input.permission.connectionScopes).toEqual([{ connectionId: "c", models: ["gpt-4o"] }]);
    expect(input.permission).not.toHaveProperty("provider");
    expect(input.permission).not.toHaveProperty("connectionId");
    const body = await response.json();
    expect(body.permission.routes).toEqual({ refs: [{ route_id: "route-1" }] });
    expect(body.permission.connection_scopes).toEqual([{ connection_id: "c", models: ["gpt-4o"] }]);
    for (const legacy of ["provider", "connection_id", "models"]) expect(body.permission).not.toHaveProperty(legacy);
  });

  it("passes public model route references through on create and update and returns canonical ids", async () => {
    const canonical = create(ApiKeySchema, { keyHash: "hash", permission: { routes: { refs: [{ identifier: { case: "routeId", value: "route-1" } }] } } });
    client.createKey.mockResolvedValueOnce({ key: canonical, rawKey: "raw" });
    client.updateKey.mockResolvedValueOnce({ key: canonical });
    const permission = { routes: { refs: [{ public_model_id: "chalk/assistant-pro" }] } };
    const created = await POST(request({ description: "key", permission }));
    expect(created.status).toBe(201);
    const updated = await PATCH(request({ permission }, "PATCH"), params);
    expect(updated.status).toBe(200);
    const expected = { refs: [{ identifier: { case: "publicModelId", value: "chalk/assistant-pro" } }] };
    expect(client.createKey.mock.calls[0][0].permission.routes).toEqual(expected);
    expect(client.updateKey.mock.calls[0][0].update.permission.routes).toEqual(expected);
    expect((await created.json()).permission.routes).toEqual({ refs: [{ route_id: "route-1" }] });
    expect((await updated.json()).permission.routes).toEqual({ refs: [{ route_id: "route-1" }] });
  });

  it("rejects missing and conflicting refs on create and update", async () => {
    for (const identifier of [{}, { route_id: "r", public_model_id: "chalk/a" }]) {
      const permission = { routes: { refs: [identifier] } };
      expect((await POST(request({ description: "key", permission }))).status).toBe(400);
      expect((await PATCH(request({ permission }, "PATCH"), params)).status).toBe(400);
    }
    expect(client.createKey).not.toHaveBeenCalled();
    expect(client.updateKey).not.toHaveBeenCalled();
  });

  it("creates an unrestricted key without routes", async () => {
    const response = await POST(request({ description: "key" }));
    expect(response.status).toBe(201);
    expect(client.createKey.mock.calls[0][0].permission).toBeUndefined();
    expect((await response.json()).permission.routes).toBeNull();
  });

  it("keeps a restricted key with nothing granted restricted", async () => {
    const response = await PATCH(request({ permission: { routes: { refs: [] }, connection_scopes: [] } }, "PATCH"), params);
    expect(response.status).toBe(200);
    const input = client.updateKey.mock.calls[0][0];
    expect(input.updateMask).toEqual({ paths: ["permission"] });
    expect(input.update.permission.routes).toEqual({ refs: [] });
    expect(input.update.permission.connectionScopes).toEqual([]);
    expect((await response.json()).permission).toMatchObject({ routes: { refs: [] }, connection_scopes: [] });
  });

  it.each([
    ["provider", { provider: "openai" }],
    ["connection_id", { connection_id: "c" }],
    ["models", { models: ["gpt-4o"] }],
  ])("rejects the legacy %s field on create and update", async (field, permission) => {
    const created = await POST(request({ description: "key", permission }));
    expect(created.status).toBe(400);
    expect((await created.json()).error.message).toContain(`permission.${field}`);
    const updated = await PATCH(request({ permission }, "PATCH"), params);
    expect(updated.status).toBe(400);
    expect(client.createKey).not.toHaveBeenCalled();
    expect(client.updateKey).not.toHaveBeenCalled();
  });

  it("rejects chalk/* and wildcard ids in connection scopes", async () => {
    for (const model of ["chalk/assistant", "openai/*"]) {
      const permission = { routes: { refs: [] }, connection_scopes: [{ connection_id: "c", models: [model] }] };
      expect((await POST(request({ description: "key", permission }))).status).toBe(400);
      expect((await PATCH(request({ permission }, "PATCH"), params)).status).toBe(400);
    }
    expect(client.createKey).not.toHaveBeenCalled();
    expect(client.updateKey).not.toHaveBeenCalled();
  });

  it("keeps cache and model-as-judge overrides alongside access on update", async () => {
    const response = await PATCH(request({
      permission: {
        routes: { refs: [{ route_id: "route-1" }] },
        downgradable: true,
        model_floor: "gpt-4o-mini",
        cache: { enabled: true, scope: "team", ttl_secs: 60, max_entry_bytes: null },
      },
    }, "PATCH"), params);
    expect(response.status).toBe(200);
    const permission = client.updateKey.mock.calls[0][0].update.permission;
    expect(permission.routes).toEqual({ refs: [{ identifier: { case: "routeId", value: "route-1" } }] });
    expect(permission.downgradable).toBe(true);
    expect(permission.modelFloor).toBe("gpt-4o-mini");
    expect(permission.cache).toMatchObject({ enabled: true, ttlSecs: 60n });
  });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
