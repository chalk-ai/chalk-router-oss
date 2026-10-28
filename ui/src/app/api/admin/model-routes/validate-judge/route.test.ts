import { afterEach, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { Code, ConnectError } from "@connectrpc/connect";
import { POST } from "./route";
const validateRouteJudgeBackend = vi.hoisted(() => vi.fn());
vi.mock("@/lib/grpc", () => ({ modelRouteClient: () => ({ validateRouteJudgeBackend }) }));
afterEach(() => vi.resetAllMocks());
const backend = { type: "decisions", target: { connection_id: "openai-id", upstream_model_id: "gpt-4o" }, timeout_ms: 1000 };
const request = (value: unknown) => new NextRequest("http://localhost/api/admin/model-routes/validate-judge", { method: "POST", body: JSON.stringify({ backend: value }) });
it("returns compatibility errors before saving any route", async () => {
  validateRouteJudgeBackend.mockRejectedValue(new ConnectError("Does not support Decisions", Code.InvalidArgument));
  const response = await POST(request(backend));
  expect(response.status).toBe(400);
  expect((await response.json()).error.message).toBe("Does not support Decisions");
  expect(validateRouteJudgeBackend).toHaveBeenCalledWith({ backend: expect.objectContaining({ kind: { case: "decisions", value: expect.objectContaining({ target: expect.objectContaining({ connectionId: "openai-id", upstreamModelId: "gpt-4o" }) }) } }) });
});
it("accepts compatible backends and rejects incomplete targets without an RPC", async () => {
  validateRouteJudgeBackend.mockResolvedValue({});
  expect(await (await POST(request(backend))).json()).toEqual({ valid: true });
  validateRouteJudgeBackend.mockClear();
  expect((await POST(request({ ...backend, target: { connection_id: "", upstream_model_id: "gpt-4o" } }))).status).toBe(400);
  expect(validateRouteJudgeBackend).not.toHaveBeenCalled();
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
