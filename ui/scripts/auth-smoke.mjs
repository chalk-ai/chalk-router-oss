// Exercise the built Next server and real gRPC-Web transport. All credentials
// and upstream responses are synthetic; no production services are contacted.
import http from "node:http";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readdir } from "node:fs/promises";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
const cwd = fileURLToPath(new URL("..", import.meta.url));
const calls = [];
const token = (sub) => ["eyJhbGciOiJIUzI1NiJ9", Buffer.from(JSON.stringify({ sub, exp: Math.floor(Date.now() / 1000) + 3600 })).toString("base64url"), "synthetic-signature"].join(".");
const users = [token("one"), token("two")];
function frame(payload, flag = 0) {
  const header = Buffer.alloc(5); header[0] = flag; header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}
const backend = http.createServer((req, res) => {
  calls.push({ path: req.url, auth: req.headers.authorization, apiKey: req.headers["x-api-key"], environment: req.headers["x-chalk-env-id"] });
  req.resume();
  if (req.url === "/v1/models") {
    res.writeHead(req.headers.authorization === "Bearer ck-limited" ? 200 : 403, { "content-type": "application/json" });
    return res.end(JSON.stringify({ data: [] }));
  }
  if (!users.some(t => req.headers.authorization === `Bearer ${t}`)) { res.writeHead(401); return res.end(); }
  res.writeHead(200, { "content-type": "application/grpc-web+proto" });
  res.end(Buffer.concat([frame(Buffer.alloc(0)), frame(Buffer.from("grpc-status:0\r\n"), 128)]));
});
backend.listen(0, "127.0.0.1"); await once(backend, "listening");
const holder = http.createServer(); holder.listen(0, "127.0.0.1"); await once(holder, "listening");
const port = holder.address().port; await new Promise(resolve => holder.close(resolve));
const origin = `http://127.0.0.1:${port}`;
let child, log = "";
async function start(configured) {
  log = "";
  child = spawn(process.execPath, ["node_modules/next/dist/bin/next", "start", "--hostname", "127.0.0.1", "--port", String(port)], {
    cwd, env: { ...process.env, NEXT_TELEMETRY_DISABLED: "1", CHALK_ROUTER_URL: `http://127.0.0.1:${backend.address().port}`,
      ROUTER_ADMIN_KEY: "synthetic-shared-key-must-never-be-used", ROUTER_UI_ORIGIN: configured ? origin : "", ROUTER_UI_SESSION_SECRET: configured ? "ab".repeat(32) : "" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", b => { log += b; }); child.stderr.on("data", b => { log += b; });
  for (let i = 0; i < 100; i++) {
    if (child.exitCode !== null) throw new Error(log);
    try { if ((await fetch(`${origin}/healthz`)).ok) return; } catch { /* starting */ }
    await new Promise(r => setTimeout(r, 100));
  }
  throw new Error(`UI did not start: ${log}`);
}
async function stop() { if (child && child.exitCode === null) { child.kill("SIGTERM"); await once(child, "exit"); } }
const request = (path, init = {}) => fetch(`${origin}${path}`, { redirect: "manual", ...init });
async function login(jwt, inferenceKey) {
  return request("/api/session", { method: "POST", headers: { origin, "content-type": "application/json" }, body: JSON.stringify({ token: jwt, environment: "env-a", inferenceKey }) });
}
try {
  await start(true);
  const loginPage = await request("/login");
  assert.equal(loginPage.status, 200);
  assert.ok((await loginPage.text()).includes("Sign in to Chalk Router"));
  const files = await readdir(`${cwd}/src/app/api`, { recursive: true });
  const routes = files.filter(p => p.endsWith("/route.ts") && p !== "session/route.ts")
    .map(p => `/api/${p.slice(0, -9).replace(/\[[^\]]+\]/g, "test")}`);
  for (const path of routes) {
    assert.equal((await request(path)).status, 401, path);
    assert.equal((await request(path, { method: "POST", headers: { origin, "content-type": "application/json" }, body: "{}" })).status, 401, path);
  }
  assert.equal((await request("/routing")).status, 307);
  assert.equal((await request("/api/admin/keys", { headers: { "x-middleware-subrequest": "proxy:proxy:proxy:proxy:proxy", "x-forwarded-user": "admin" } })).status, 401);
  assert.equal(calls.length, 0, "anonymous requests must not reach the backend");
  assert.equal((await login(token("invalid"))).status, 401);
  const signedIn = await login(users[0]); assert.equal(signedIn.status, 200, await signedIn.text());
  const cookie = signedIn.headers.get("set-cookie").split(";")[0];
  const before = calls.length;
  for (const headers of [{ origin: "https://evil.example", "content-type": "application/json" }, { origin, "content-type": "text/plain" }]) {
    assert.ok([403, 415].includes((await request("/api/admin/keys", { method: "POST", headers: { cookie, ...headers }, body: "{}" })).status));
  }
  assert.equal((await request("/api/v1/models", { headers: { cookie } })).status, 403);
  assert.equal(calls.length, before);
  assert.equal((await request("/api/admin/keys", { headers: { cookie } })).status, 200);
  assert.equal(calls.at(-1).auth, `Bearer ${users[0]}`);
  const second = await login(users[1], "ck-limited"); assert.equal(second.status, 200);
  const cookie2 = second.headers.get("set-cookie").split(";")[0];
  await Promise.all([request("/api/admin/keys", { headers: { cookie } }), request("/api/admin/keys", { headers: { cookie: cookie2 } })]);
  assert.deepEqual(new Set(calls.slice(-2).map(c => c.auth)), new Set(users.map(t => `Bearer ${t}`)));
  assert.equal((await request("/api/v1/models", { headers: { cookie: cookie2 } })).status, 200);
  assert.equal(calls.at(-1).auth, "Bearer ck-limited");
  assert.ok(calls.every(c => !c.apiKey && c.auth !== "Bearer synthetic-shared-key-must-never-be-used"));
  await stop(); await start(false);
  const beforeMissingConfig = calls.length;
  assert.equal((await request("/api/admin/keys")).status, 503);
  assert.equal((await request("/routing")).status, 503);
  assert.equal(calls.length, beforeMissingConfig);
  console.log(`PASS: ${routes.length} BFF paths deny anonymous GET/POST without upstream calls`);
  console.log("PASS: CSRF, direct-listener/header bypass, login, per-user forwarding, limited playground credentials");
  console.log("PASS: missing authentication configuration fails closed");
} catch (error) { console.error(log); throw error; }
finally { await stop(); backend.closeAllConnections(); await new Promise(resolve => backend.close(resolve)); }
