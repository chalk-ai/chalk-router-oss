import { beforeEach, describe, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { create } from "@bufbuild/protobuf";
import { ModelRouteSchema } from "@/gen/chalk/router/v1/model_route_pb";
import { GET, POST } from "./route";
import { PATCH } from "./[id]/route";
const client = vi.hoisted(() => ({ listModelRoutes: vi.fn(), createModelRoute: vi.fn(), updateModelRoute: vi.fn() }));
vi.mock("@/lib/grpc", () => ({ modelRouteClient: () => client }));
const grid = { default_target: { connection_id: "connection", upstream_model_id: "astra" }, fallbacks: { "connection/astra": ["connection/gpt-5.5"] }, judge: null };
const request = (body: unknown, method = "POST") => new NextRequest("http://localhost/api/admin/model-routes", { method, body: JSON.stringify(body), headers: { "content-type": "application/json" } });
const params = { params: Promise.resolve({ id: "route-id" }) };
beforeEach(() => {
  vi.resetAllMocks();
  client.createModelRoute.mockImplementation(async input => ({ route: create(ModelRouteSchema, { ...input, id: "route-id" }) }));
  client.updateModelRoute.mockImplementation(async input => ({ route: create(ModelRouteSchema, { ...input.update, id: input.id }) }));
});
describe("route BFF configuration", () => {
  it("creates and lists a custom-only route with routing intact", async () => {
    const response = await POST(request({ public_model_id: "chalk/assistant", routing: grid }));
    expect(response.status).toBe(201);
    expect((await response.json()).routing).toEqual(grid);
    const input = client.createModelRoute.mock.calls[0][0];
    client.listModelRoutes.mockResolvedValue({ routes: [create(ModelRouteSchema, input)], nextCursor: "" });
    const listed = await GET(new NextRequest("http://localhost/api/admin/model-routes"));
    expect((await listed.json()).data[0].routing).toEqual(grid);
  });
  it("still requires connection and model on simple routes", async () => {
    expect((await POST(request({ public_model_id: "chalk/a" }))).status).toBe(400);
    expect(client.createModelRoute).not.toHaveBeenCalled();
  });
  it("updates routing with a field mask and preserves it on unrelated edits", async () => {
    expect((await PATCH(request({ routing: grid }, "PATCH"), params)).status).toBe(200);
    expect(client.updateModelRoute.mock.calls[0][0].updateMask.paths).toEqual(["routing"]);
    await PATCH(request({ published: false }, "PATCH"), params);
    expect(client.updateModelRoute.mock.calls[1][0].updateMask.paths).toEqual(["published"]);
    expect(client.updateModelRoute.mock.calls[1][0].update).not.toHaveProperty("routing");
  });
  it("clears routing with a masked absent message and supplies the new simple target", async () => {
    await PATCH(request({ routing: null, connection_id: "a", upstream_model_id: "b" }, "PATCH"), params);
    expect(client.updateModelRoute.mock.calls[0][0]).toMatchObject({ update: { connectionId: "a", upstreamModelId: "b", routing: undefined }, updateMask: { paths: ["connection_id", "upstream_model_id", "routing"] } });
  });
  it("rejects an empty grid before calling gRPC", async () => {
    expect((await POST(request({ public_model_id: "chalk/a", routing: {} }))).status).toBe(400);
    expect((await PATCH(request({ routing: {} }, "PATCH"), params)).status).toBe(400);
    expect(client.createModelRoute).not.toHaveBeenCalled(); expect(client.updateModelRoute).not.toHaveBeenCalled();
  });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
