import { beforeEach, describe, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { POST } from "./route";
import { PATCH } from "./[id]/route";

const client = vi.hoisted(() => ({ createRateLimit: vi.fn(), updateRateLimit: vi.fn() }));
vi.mock("@/lib/grpc", () => ({ rateLimitClient: () => client }));
const request = (body: unknown, method = "POST") => new NextRequest("http://localhost/api/admin/rate-limits", {
  method,
  body: JSON.stringify(body),
  headers: { "content-type": "application/json" },
});
const params = { params: Promise.resolve({ id: "policy-id" }) };
const input = {
  limit_type: "requests_per_minute",
  limit_value: 1,
  connection_id: "openai",
  scope: "api_key",
  scope_id: "key-id",
};

beforeEach(() => {
  vi.resetAllMocks();
  client.createRateLimit.mockImplementation(async input => ({ policy: { ...input, id: "policy-id" } }));
  client.updateRateLimit.mockImplementation(async input => ({ policy: { ...input.update, id: input.id } }));
});

describe("rate-limit model selectors", () => {
  it.each(["*", "openai/*", "bedrock/openai.*"])("rejects wildcard %s in create and update before calling gRPC", async model => {
    const created = await POST(request({ ...input, models: [model] }));
    const updated = await PATCH(request({ models: [model] }, "PATCH"), params);
    for (const response of [created, updated]) {
      expect(response.status).toBe(400);
      expect((await response.json()).error.message).toContain("models");
    }
    expect(client.createRateLimit).not.toHaveBeenCalled();
    expect(client.updateRateLimit).not.toHaveBeenCalled();
  });

  it.each([{ models: ["openai/gpt-6-luna"] }, { models: [] }])("allows exact IDs and empty connection-wide selectors %j", async ({ models }) => {
    expect((await POST(request({ ...input, models }))).status).toBe(201);
    expect(client.createRateLimit.mock.calls[0][0]).toMatchObject({ connectionId: "openai", models });
    expect((await PATCH(request({ models }, "PATCH"), params)).status).toBe(200);
    expect(client.updateRateLimit.mock.calls[0][0]).toMatchObject({
      update: { models },
      updateMask: { paths: ["models"] },
    });
  });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
