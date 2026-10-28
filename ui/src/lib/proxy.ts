/**
 * Shared helper for proxying playground requests to the chalk-router's own
 * inference APIs, including OpenAI-compatible endpoints and native Gemini
 * Interactions (`/v1beta/interactions`).
 *
 * This is a different surface than the gRPC-Web admin API in `lib/grpc.ts`:
 * it has no gRPC equivalent, some endpoints stream Server-Sent Events, and
 * it is literally what the playground's chat UI talks to — so it stays a
 * thin HTTP passthrough rather than growing into a second admin REST client.
 */
import { currentSession, authResponse } from "./auth";
import { AuthError } from "./session";
import { NextResponse } from "next/server";

const CHALK_ROUTER_URL = process.env.CHALK_ROUTER_URL ?? "http://localhost:4001";

export interface ProxyRequestInit {
  signal?: AbortSignal;
  method?: string;
  headers?: Record<string, string>;
  /** Already-read request body (string or null for GET/HEAD). Never a
   * ReadableStream — every caller reads the incoming request body fully
   * first, so there is nothing to stream on the way in; streaming only
   * matters for the response, whose body is returned unread below. */
  body?: BodyInit | null;
}

/** Forward using only the signed-in user's separate router API key. */
export async function proxyToRouter(
  path: string,
  init: ProxyRequestInit = {},
): Promise<Response> {
  const headers = new Headers(init.headers);
  try {
    const session = await currentSession();
    if (!session.inferenceKey) throw new AuthError("Sign in with a router API key to use the playground", 403);
    for (const name of ["authorization", "x-api-key", "x-goog-api-key", "x-chalk-env-id"]) headers.delete(name);
    headers.set("authorization", `Bearer ${session.inferenceKey}`);
  } catch (error) { return authResponse(error); }
  try {
    return await fetch(`${CHALK_ROUTER_URL}${path}`, {
      method: init.method ?? "GET",
      redirect: "error",
      cache: "no-store",
      headers,
      body: init.body ?? undefined,
      signal: init.signal,
    });
  } catch (e) {
    return NextResponse.json(
      {
        error: {
          message: e instanceof Error ? e.message : "Failed to reach chalk-router",
          type: "upstream_unreachable",
        },
      },
      { status: 502 },
    );
  }
}
