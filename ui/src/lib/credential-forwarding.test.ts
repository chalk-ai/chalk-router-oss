import { beforeEach, afterEach, expect, it, vi } from "vitest";
import { sealSession, SESSION_COOKIE } from "./session";
import { proxyToRouter } from "./proxy";
import { apiKeyClient } from "./grpc";
import type { Interceptor } from "@connectrpc/connect";
const state = vi.hoisted(() => ({ headers: new Headers(), interceptors: [] as Interceptor[] }));
vi.mock("next/headers", () => ({ headers: async () => state.headers }));
vi.mock("@connectrpc/connect-node", () => ({ createGrpcWebTransport: (options: { interceptors: Interceptor[] }) => {
  state.interceptors = options.interceptors; return {};
} }));
vi.mock("@connectrpc/connect", async (original) => ({ ...await original<typeof import("@connectrpc/connect")>(), createClient: () => ({}) }));
function signIn(token: string, inferenceKey?: string) {
  state.headers = new Headers({ cookie: `${SESSION_COOKIE}=${sealSession({ token, inferenceKey, environment: "env-a", expires: Date.now() + 60000 })}` });
}
beforeEach(() => {
  vi.stubEnv("ROUTER_UI_SESSION_SECRET", "ab".repeat(32));
  vi.stubEnv("ROUTER_UI_ORIGIN", "https://router.example");
  vi.stubEnv("ROUTER_ADMIN_KEY", "must-never-be-used");
  state.headers = new Headers();
});
afterEach(() => { vi.unstubAllEnvs(); vi.unstubAllGlobals(); });

it("checks the session at the gRPC boundary and never shares credentials between callers", async () => {
  apiKeyClient();
  const next = vi.fn(async () => ({}));
  const call = state.interceptors[0](next as never);
  const request = { header: new Headers({ authorization: "Bearer stale", "x-chalk-env-id": "wrong" }) };
  await expect(call(request as never)).rejects.toThrow("Sign in required");
  expect(next).not.toHaveBeenCalled();
  for (const token of ["user-one.jwt.signature", "user-two.jwt.signature"]) {
    signIn(token);
    await call(request as never);
    expect(request.header.get("authorization")).toBe(`Bearer ${token}`);
    expect(request.header.get("x-chalk-env-id")).toBe("env-a");
  }
});

it("requires a separate playground key and replaces all caller-supplied auth headers", async () => {
  const fetch = vi.fn(async () => new Response("stream"));
  vi.stubGlobal("fetch", fetch);
  expect((await proxyToRouter("/v1/models")).status).toBe(401);
  signIn("user.jwt.signature");
  expect((await proxyToRouter("/v1/models")).status).toBe(403);
  expect(fetch).not.toHaveBeenCalled();
  signIn("user.jwt.signature", "ck-limited-user-key");
  const result = await proxyToRouter("/v1/models", { headers: {
    Authorization: "Bearer injected", "X-Api-Key": "injected", "x-goog-api-key": "injected", "x-chalk-env-id": "other-env",
  } });
  expect(await result.text()).toBe("stream");
  const options = (fetch.mock.calls as unknown as [string, RequestInit][])[0][1];
  const headers = new Headers(options.headers);
  expect(headers.get("authorization")).toBe("Bearer ck-limited-user-key");
  for (const name of ["x-api-key", "x-goog-api-key", "x-chalk-env-id"]) expect(headers.has(name)).toBe(false);
  expect(options.redirect).toBe("error");
});
