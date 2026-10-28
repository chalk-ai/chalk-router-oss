import { headers } from "next/headers";
import { NextRequest, NextResponse } from "next/server";
import { AuthError, checkMutation, sessionFromHeaders } from "./session";

export async function currentSession() {
  return sessionFromHeaders(await headers());
}

export function authResponse(error: unknown): NextResponse {
  const known = error instanceof AuthError;
  return NextResponse.json({ error: { message: known ? error.message : "Authentication failed" } },
    { status: known ? error.status : 401, headers: { "Cache-Control": "no-store" } });
}

// Route-level guard, independent of Next Proxy. No request body or upstream
// operation is reached before session verification and CSRF checks.
export function withSession<Args extends unknown[], Result extends Response>(
  handler: (request: NextRequest, ...args: Args) => Result | Promise<Result>,
) {
  return async (request: NextRequest, ...args: Args): Promise<Result | NextResponse> => {
    try { sessionFromHeaders(request.headers); checkMutation(request); }
    catch (error) { return authResponse(error); }
    const response = await handler(request, ...args);
    response.headers.set("Cache-Control", "no-store");
    return response;
  };
}
