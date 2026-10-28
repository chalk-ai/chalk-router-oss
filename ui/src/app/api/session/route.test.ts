import { beforeEach, afterEach, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import { POST, DELETE } from "./route";
import { openSession, SESSION_COOKIE } from "@/lib/session";
const { listKeys } = vi.hoisted(() => ({ listKeys: vi.fn() }));
vi.mock("@connectrpc/connect", () => ({ createClient: () => ({ listKeys }) }));
vi.mock("@connectrpc/connect-node", () => ({ createGrpcWebTransport: () => ({}) }));
const origin = "https://router.example";
function token() { return `header.${Buffer.from(JSON.stringify({ exp: Math.floor(Date.now() / 1000) + 3600 })).toString("base64url")}.signature`; }
function request(body: unknown, suppliedOrigin = origin) {
  return new NextRequest(`${origin}/api/session`, { method: "POST", headers: { origin: suppliedOrigin, "content-type": "application/json" }, body: JSON.stringify(body) });
}
beforeEach(() => {
  vi.stubEnv("ROUTER_UI_SESSION_SECRET", "ab".repeat(32)); vi.stubEnv("ROUTER_UI_ORIGIN", origin);
  listKeys.mockReset().mockResolvedValue({ keys: [] });
});
afterEach(() => vi.unstubAllEnvs());
it("verifies with the backend before issuing an encrypted, short-lived secure cookie", async () => {
  const jwt = token();
  const response = await POST(request({ token: jwt, environment: "env-a", inferenceKey: "ck-limited" }));
  expect(response.status).toBe(200);
  const headers = listKeys.mock.calls[0][1].headers as Headers;
  expect(headers.get("authorization")).toBe(`Bearer ${jwt}`);
  expect(headers.get("x-chalk-env-id")).toBe("env-a");
  const cookie = response.headers.get("set-cookie")!;
  for (const flag of ["HttpOnly", "Secure", "SameSite=strict", "Path=/"]) expect(cookie).toContain(flag);
  expect(cookie).not.toContain(jwt);
  expect(openSession(response.cookies.get(SESSION_COOKIE)?.value).inferenceKey).toBe("ck-limited");
  expect(response.headers.get("cache-control")).toBe("no-store");
});
it("never issues sessions for rejected credentials or a shared opaque admin key", async () => {
  listKeys.mockRejectedValue(new Error("denied"));
  const response = await POST(request({ token: token() }));
  expect(response.status).toBe(401); expect(response.headers.has("set-cookie")).toBe(false);
  listKeys.mockClear();
  expect((await POST(request({ token: "static-admin-key" }))).status).toBe(400);
  expect(listKeys).not.toHaveBeenCalled();
});
it("protects login and logout from cross-origin requests", async () => {
  expect((await POST(request({ token: token() }, "https://evil.example"))).status).toBe(403);
  expect((await DELETE(new NextRequest(`${origin}/api/session`, { method: "DELETE" }))).status).toBe(403);
  expect(listKeys).not.toHaveBeenCalled();
  const response = await DELETE(new NextRequest(`${origin}/api/session`, { method: "DELETE", headers: { origin } }));
  expect(response.headers.get("set-cookie")).toContain("Max-Age=0");
});
