import { beforeEach, describe, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { POST } from "./route";

const { proxyToRouter } = vi.hoisted(() => ({ proxyToRouter: vi.fn() }));
vi.mock("@/lib/proxy", () => ({ proxyToRouter }));
beforeEach(() => { proxyToRouter.mockReset(); });

describe("native Gemini playground proxy", () => {
  it("forwards native JSON untouched and streams the response unchanged", async () => {
    const body = '{"model":"google/gemini","input":[{"type":"thought","signature":"sig==","number":1.2300}]}';
    const frames = 'id: 7\nevent: step.start\ndata: {"step":{"type":"thought","signature":"sig=="}}\n\n';
    proxyToRouter.mockResolvedValue(new Response(frames, { headers: { "content-type": "text/event-stream" } }));
    const controller = new AbortController();
    const req = new NextRequest("http://localhost/api/v1beta/interactions", {
      method: "POST", body, signal: controller.signal,
      headers: { "x-goog-api-key": "client-key-must-not-be-forwarded" },
    });
    const response = await POST(req);
    expect(proxyToRouter).toHaveBeenCalledWith("/v1beta/interactions", {
      method: "POST", headers: { "Content-Type": "application/json" }, body, signal: req.signal,
    });
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("text/event-stream");
    expect(await response.text()).toBe(frames);
    controller.abort();
    expect(proxyToRouter.mock.calls[0][1].signal.aborted).toBe(true);
  });

  it("preserves JSON error status and content type", async () => {
    const error = '{"error":{"message":"token limit exceeded"}}';
    proxyToRouter.mockResolvedValue(new Response(error, { status: 429, headers: { "content-type": "application/json" } }));
    const response = await POST(new NextRequest("http://localhost/api/v1beta/interactions", { method: "POST", body: "{}" }));
    expect(response.status).toBe(429);
    expect(response.headers.get("content-type")).toBe("application/json");
    expect(await response.text()).toBe(error);
  });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
