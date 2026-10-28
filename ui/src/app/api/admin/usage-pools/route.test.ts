import { beforeEach, describe, expect, it, vi } from "vitest";
import { Code, ConnectError } from "@connectrpc/connect";
import { NextRequest } from "next/server";
import { POST } from "./route";
import { GET, PATCH } from "./[id]/route";

const client = vi.hoisted(() => ({ createUsagePool: vi.fn(), updateUsagePool: vi.fn(), getUsagePool: vi.fn() }));
vi.mock("@/lib/grpc", () => ({ usagePoolClient: () => client }));

beforeEach(() => vi.resetAllMocks());

describe("usage-pool name conflicts", () => {
  it.each(["POST", "PATCH"])("returns HTTP 409 for a conflicting %s", async method => {
    const message = "A usage pool with this name already exists.";
    const conflict = new ConnectError(message, Code.AlreadyExists);
    client.createUsagePool.mockRejectedValue(conflict);
    client.updateUsagePool.mockRejectedValue(conflict);
    const request = new NextRequest("http://localhost/api/admin/usage-pools/pool-id", {
      method,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "Production" }),
    });
    const response = method === "POST"
      ? await POST(request)
      : await PATCH(request, { params: Promise.resolve({ id: "pool-id" }) });
    expect(response.status).toBe(409);
    expect(await response.json()).toEqual({
      error: { message, error_type: "already_exists", param: null, code: null },
    });
    if (method === "PATCH") {
      expect(client.updateUsagePool).toHaveBeenCalledWith(expect.objectContaining({
        id: "pool-id", updateMask: { paths: ["name"] },
      }));
    } else {
      expect(client.createUsagePool).toHaveBeenCalledWith(expect.objectContaining({ name: "Production" }));
    }
  });
});

it("uses the ID oneof selector when loading a usage pool", async () => {
  client.getUsagePool.mockResolvedValue({ pool: { id: "pool-id", name: "Production" } });
  const response = await GET(
    new NextRequest("http://localhost/api/admin/usage-pools/pool-id"),
    { params: Promise.resolve({ id: "pool-id" }) },
  );
  expect(response.status).toBe(200);
  expect(client.getUsagePool).toHaveBeenCalledWith({ lookup: { case: "id", value: "pool-id" } });
  expect(await response.json()).toMatchObject({ id: "pool-id", name: "Production" });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
