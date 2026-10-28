import { beforeEach, afterEach, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { sealSession, openSession, SESSION_COOKIE, checkMutation, authConfig } from "./session";
import { proxy } from "@/proxy";

const { upstream } = vi.hoisted(() => ({ upstream: vi.fn(() => { throw new Error("Unexpected upstream call"); }) }));
vi.mock("@/lib/grpc", () => Object.fromEntries([
  "apiKeyClient", "modelRouteClient", "providerConnectionClient", "rateLimitClient",
  "samplingPolicyClient", "telemetrySettingsClient", "usageClient", "usagePoolClient", "usageBudgetClient",
].map(name => [name, upstream])));
vi.mock("@/lib/proxy", () => ({ proxyToRouter: upstream }));
const routes = import.meta.glob(
  "../app/api/**/route.ts", { eager: true },
) as Record<string, Record<string, (request: NextRequest, context: unknown) => Promise<Response>>>;
const origin = "https://router.example";
function cookie() { return `${SESSION_COOKIE}=${sealSession({ token: "user.jwt.signature", environment: "env-a", expires: Date.now() + 60000 })}`; }
beforeEach(() => {
  vi.stubEnv("ROUTER_UI_SESSION_SECRET", "ab".repeat(32));
  vi.stubEnv("ROUTER_UI_ORIGIN", origin);
  upstream.mockClear();
});
afterEach(() => vi.unstubAllEnvs());

it("denies anonymous GET and mutations on every BFF route before upstream work", async () => {
  let count = 0;
  for (const [file, module] of Object.entries(routes)) {
    if (file.includes("/session/")) continue;
    for (const [method, handler] of Object.entries(module)) {
      const response = await handler(new NextRequest(`${origin}/api/probe`, { method }), {});
      expect(response.status, `${file}:${method}`).toBe(401);
      count++;
    }
  }
  expect(count).toBeGreaterThan(40);
  expect(upstream).not.toHaveBeenCalled();
});

it("rejects foreign or absent Origin on every authenticated mutation, independently of Proxy", async () => {
  for (const [file, module] of Object.entries(routes)) {
    for (const [method, handler] of Object.entries(module)) {
      if (["GET", "HEAD"].includes(method)) continue;
      for (const suppliedOrigin of [undefined, "https://evil.example"]) {
        const headers = new Headers({ cookie: cookie(), "content-type": "application/json" });
        if (suppliedOrigin) headers.set("origin", suppliedOrigin);
        const response = await handler(new NextRequest(`${origin}/api/probe`, { method, headers }), {});
        expect(response.status, `${file}:${method}`).toBe(403);
      }
    }
  }
  expect(upstream).not.toHaveBeenCalled();
});

it("authenticates encrypted sessions and rejects tampering, expiry, key and origin changes", () => {
  const value = cookie().split("=")[1];
  expect(value).not.toContain("user.jwt.signature");
  expect(openSession(value).environment).toBe("env-a");
  expect(() => openSession(`${value[0] === "A" ? "B" : "A"}${value.slice(1)}`)).toThrow();
  expect(() => openSession(sealSession({ token: "x", environment: "a", expires: Date.now() - 1 }))).toThrow();
  vi.stubEnv("ROUTER_UI_ORIGIN", "https://other.example");
  expect(() => openSession(value)).toThrow();
  vi.stubEnv("ROUTER_UI_ORIGIN", origin);
  vi.stubEnv("ROUTER_UI_SESSION_SECRET", "cd".repeat(32));
  expect(() => openSession(value)).toThrow();
});

it("fails closed on missing or unsafe configuration and ignores proxy identity headers", () => {
  for (const value of ["", "http://public.example", "https://router.example/path"]) {
    vi.stubEnv("ROUTER_UI_ORIGIN", value);
    expect(() => authConfig()).toThrow();
  }
  vi.stubEnv("ROUTER_UI_ORIGIN", origin);
  expect(proxy(new NextRequest(`${origin}/api/admin/keys`, { headers: { "x-forwarded-user": "admin", "x-middleware-subrequest": "proxy", authorization: "Bearer anything" } })).status).toBe(401);
  vi.stubEnv("ROUTER_UI_SESSION_SECRET", "");
  expect(proxy(new NextRequest(`${origin}/api/admin/keys`)).status).toBe(503);
});

it("rejects text/plain JSON and accepts same-origin JSON", () => {
  expect(() => checkMutation(new Request(origin, { method: "POST", headers: { origin, "content-type": "text/plain" } }))).toThrow();
  expect(() => checkMutation(new Request(origin, { method: "POST", headers: { origin, "content-type": "application/json" } }))).not.toThrow();
});
