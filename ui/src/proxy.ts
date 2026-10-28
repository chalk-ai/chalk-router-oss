import { NextRequest, NextResponse } from "next/server";
import { AuthError, authConfig, checkMutation, sessionFromHeaders } from "@/lib/session";

export function proxy(request: NextRequest) {
  const path = request.nextUrl.pathname;
  if (path === "/healthz") return NextResponse.next();
  try {
    authConfig();
    checkMutation(request);
    if (path !== "/login" && path !== "/api/session") sessionFromHeaders(request.headers);
    const response = NextResponse.next();
    response.headers.set("Cache-Control", "no-store");
    response.headers.set("Referrer-Policy", "no-referrer");
    response.headers.set("X-Frame-Options", "DENY");
    return response;
  } catch (error) {
    const status = error instanceof AuthError ? error.status : 401;
    if (status === 401 && request.method === "GET" && !path.startsWith("/api/")) {
      return NextResponse.redirect(new URL("/login", authConfig().origin));
    }
    return NextResponse.json({ error: { message: error instanceof AuthError ? error.message : "Authentication failed" } },
      { status, headers: { "Cache-Control": "no-store" } });
  }
}

export const config = { matcher: ["/((?!_next/static|_next/image|favicon.ico).*)"] };
