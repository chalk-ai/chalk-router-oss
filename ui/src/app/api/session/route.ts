import { NextRequest, NextResponse } from "next/server";
import { createClient } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-node";
import { ApiKeyService } from "@/gen/chalk/router/v1/api_key_pb";
import { authResponse } from "@/lib/auth";
import { AuthError, authConfig, checkMutation, sealSession, SESSION_COOKIE, SESSION_SECONDS } from "@/lib/session";
import { readBoundedBody } from "@/lib/bounded-body";

export async function POST(request: NextRequest) {
  try {
    const config = authConfig();
    checkMutation(request);
    const body = JSON.parse(new TextDecoder().decode(await readBoundedBody(request, 8192)));
    const { token, environment = "", inferenceKey } = body;
    if (typeof token !== "string" || !/^[\w-]+\.[\w-]+\.[\w-]+$/.test(token) ||
        typeof environment !== "string" || environment.length > 256 || /[\r\n]/.test(environment) ||
        (inferenceKey !== undefined && (typeof inferenceKey !== "string" || !/^ck-[\w-]+$/.test(inferenceKey)))) {
      throw new AuthError("Provide a Chalk JWT and an optional router API key", 400);
    }
    // This is NOT local JWT verification: the router verifies the signature,
    // issuer, audience, tenant and tokens.list grant. Every later RPC is also
    // authorized by the router with this same user credential.
    const client = createClient(ApiKeyService, createGrpcWebTransport({
      baseUrl: process.env.CHALK_ROUTER_URL ?? "http://localhost:4001", httpVersion: "1.1",
    }));
    const headers = new Headers({ authorization: `Bearer ${token}` });
    if (environment) headers.set("x-chalk-env-id", environment);
    await client.listKeys({ limit: 1 }, { headers, timeoutMs: 5000 });
    const claims = JSON.parse(Buffer.from(token.split(".")[1], "base64url").toString("utf8"));
    if (!Number.isFinite(claims.exp) || claims.exp * 1000 <= Date.now()) throw new AuthError("Token expired", 401);
    const expires = Math.min(claims.exp * 1000, Date.now() + SESSION_SECONDS * 1000);
    const cookie = sealSession({ token, environment, inferenceKey, expires });
    const response = NextResponse.json({ ok: true }, { headers: { "Cache-Control": "no-store" } });
    response.cookies.set(SESSION_COOKIE, cookie, {
      httpOnly: true, secure: config.secure, sameSite: "strict", path: "/",
      maxAge: Math.floor((expires - Date.now()) / 1000),
    });
    return response;
  } catch (error) { return authResponse(error); }
}

export async function DELETE(request: NextRequest) {
  try {
    const config = authConfig();
    checkMutation(request);
    const response = NextResponse.json({ ok: true }, { headers: { "Cache-Control": "no-store" } });
    response.cookies.set(SESSION_COOKIE, "", { httpOnly: true, secure: config.secure, sameSite: "strict", path: "/", maxAge: 0 });
    return response;
  } catch (error) { return authResponse(error); }
}
