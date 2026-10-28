import { withSession } from "@/lib/auth";
import { NextRequest } from "next/server";
import { proxyToRouter } from "@/lib/proxy";
import { BodyReadError, readBoundedBody } from "@/lib/bounded-body";
import { EMBEDDING_REQUEST_MAX_BYTES, EMBEDDING_REQUEST_LIMIT_LABEL } from "@/lib/embedding-request-limits";

async function handlePOST(req: NextRequest) {
  // The gateway validates JSON; parsing here would hold extra copies of a 4 MiB body.
  let body: Uint8Array<ArrayBuffer>;
  try {
    body = await readBoundedBody(req, EMBEDDING_REQUEST_MAX_BYTES);
  } catch (error) {
    if (!(error instanceof BodyReadError)) throw error;
    return Response.json({ error: {
      message: error.status === 413
        ? `The embeddings playground accepts at most ${EMBEDDING_REQUEST_LIMIT_LABEL} of encoded request data.`
        : error.message,
      type: "invalid_request_error",
      code: error.status === 413 ? "request_too_large" : "unreadable_body",
    } }, { status: error.status });
  }
  const res = await proxyToRouter("/v1/embeddings", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body,
    signal: req.signal,
  });
  const headers = new Headers();
  headers.set("Content-Type", res.headers.get("Content-Type") ?? "application/octet-stream");
  headers.set("Cache-Control", "no-store");
  for (const [name, value] of res.headers) {
    if (name === "retry-after" || name === "x-request-id"
      || name.startsWith("x-ratelimit-") || name === "ratelimit" || name.startsWith("ratelimit-")) {
      headers.set(name, value);
    }
  }
  return new Response(res.body, { status: res.status, headers });
}

export const POST = withSession(handlePOST);
