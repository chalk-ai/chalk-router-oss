import { beforeEach, describe, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { EMBEDDING_REQUEST_MAX_BYTES } from "@/lib/embedding-request-limits";
import { POST } from "./route";

const { proxyToRouter } = vi.hoisted(() => ({ proxyToRouter: vi.fn() }));
vi.mock("@/lib/proxy", () => ({ proxyToRouter }));
beforeEach(() => { proxyToRouter.mockReset(); });

const request = (body: string, headers?: HeadersInit) => new NextRequest("http://localhost/api/v1/embeddings", {
  method: "POST", body, headers,
});

describe("embeddings playground proxy", () => {
  it("forwards the exact request bytes through existing auth without reserializing or caller credentials", async () => {
    const body = ' {"model":"custom/alias","input":[{"type":"text","text":"🌲"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U="}}],"future":1.2300}\n';
    const responseBody = '{"data":[{"embedding":[0.5]}],"usage":{"prompt_tokens":258}}';
    proxyToRouter.mockResolvedValue(new Response(responseBody, { headers: { "content-type": "application/json" } }));
    const req = request(body, { "x-api-key": "caller-key", authorization: "Bearer caller-token" });
    const response = await POST(req);
    expect(proxyToRouter).toHaveBeenCalledWith("/v1/embeddings", {
      method: "POST", headers: { "Content-Type": "application/json" }, body: expect.any(Uint8Array), signal: req.signal,
    });
    expect(new TextDecoder().decode(proxyToRouter.mock.calls[0][1].body)).toBe(body);
    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(await response.text()).toBe(responseBody);
  });

  it("accepts exactly the shared encoded limit and rejects one byte beyond it", async () => {
    const prefix = '{"model":"custom/embedding","input":"';
    const suffix = '"}';
    const body = prefix + "x".repeat(EMBEDDING_REQUEST_MAX_BYTES - prefix.length - suffix.length) + suffix;
    proxyToRouter.mockResolvedValue(new Response('{"data":[]}'));
    expect((await POST(request(body))).status).toBe(200);
    proxyToRouter.mockClear();
    const oversized = await POST(request(body + " ", { "content-length": "1" }));
    expect(oversized.status).toBe(413);
    expect((await oversized.json()).error.code).toBe("request_too_large");
    expect(proxyToRouter).not.toHaveBeenCalled();
  });

  it("bounds chunked requests without Content-Length before forwarding", async () => {
    let next = 0;
    const cancel = vi.fn();
    const body = new ReadableStream<Uint8Array>({
      pull(controller) {
        if (next++ < 5) controller.enqueue(new Uint8Array(1024 * 1024).fill(120));
        else controller.close();
      }, cancel,
    });
    const init = { method: "POST", body, duplex: "half" as const };
    const req = new NextRequest("http://localhost/api/v1/embeddings", init);
    expect((await POST(req)).status).toBe(413);
    expect(cancel).toHaveBeenCalledOnce();
    expect(proxyToRouter).not.toHaveBeenCalled();
  });

  it("leaves JSON validation to the gateway and relays its 400", async () => {
    proxyToRouter.mockResolvedValue(new Response("Failed to parse the request body as JSON", {
      status: 400, headers: { "content-type": "text/plain; charset=utf-8" },
    }));
    const response = await POST(request('{"input":NaN}'));
    expect(new TextDecoder().decode(proxyToRouter.mock.calls[0][1].body)).toBe('{"input":NaN}');
    expect(response.status).toBe(400);
    expect(await response.text()).toBe("Failed to parse the request body as JSON");
  });

  it("returns a JSON 400 when the request stream fails", async () => {
    const body = new ReadableStream({ start(controller) { controller.error(new Error("disconnected")); } });
    const req = new NextRequest("http://localhost/api/v1/embeddings", { method: "POST", body, duplex: "half" as const });
    const response = await POST(req);
    expect(response.status).toBe(400);
    expect((await response.json()).error.code).toBe("unreadable_body");
    expect(proxyToRouter).not.toHaveBeenCalled();
  });

  it.each([429, 503])("preserves status%s, nonJSON body and safe retry headers", async (status) => {
    proxyToRouter.mockResolvedValue(new Response("provider temporarily unavailable", {
      status, headers: {
        "content-type": "text/plain", "retry-after": "7", "x-request-id": "upstream-request",
        "x-ratelimit-remaining-tokens": "0", "set-cookie": "provider-secret=hidden", "x-provider-key": "hidden",
      },
    }));
    const response = await POST(request('{"model":"alias","input":"hello"}'));
    expect(response.status).toBe(status);
    expect(response.headers.get("content-type")).toBe("text/plain");
    expect(response.headers.get("retry-after")).toBe("7");
    expect(response.headers.get("x-request-id")).toBe("upstream-request");
    expect(response.headers.get("x-ratelimit-remaining-tokens")).toBe("0");
    expect(response.headers.has("set-cookie")).toBe(false);
    expect(response.headers.has("x-provider-key")).toBe(false);
    expect(await response.text()).toBe("provider temporarily unavailable");
  });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
