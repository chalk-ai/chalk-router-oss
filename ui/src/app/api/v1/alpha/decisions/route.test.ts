import { beforeEach, describe, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { POST } from "./route";

const { proxyToRouter } = vi.hoisted(() => ({ proxyToRouter: vi.fn() }));
vi.mock("@/lib/proxy", () => ({ proxyToRouter }));
beforeEach(() => proxyToRouter.mockReset());

describe("Decisions playground proxy", () => {
  it("forwards the body and preserves router status and JSON", async () => {
    const body = '{"model":"typesafe/jev-latest","state":"ticket","questions":{}}';
    const result = '{"error":{"message":"questions are required"}}';
    proxyToRouter.mockResolvedValue(new Response(result, { status: 400, headers: { "content-type": "application/json" } }));
    const request = new NextRequest("http://localhost/api/v1/alpha/decisions", { method: "POST", body });
    const response = await POST(request);
    expect(proxyToRouter).toHaveBeenCalledWith("/v1/alpha/decisions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
      signal: request.signal,
    });
    expect(response.status).toBe(400);
    expect(await response.text()).toBe(result);
  });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
