import { withSession } from "@/lib/auth";
import { NextRequest } from "next/server";
import { proxyToRouter } from "@/lib/proxy";

async function handlePOST(req: NextRequest) {
  const body = await req.text();
  const response = await proxyToRouter("/v1/alpha/decisions", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body,
    signal: req.signal,
  });
  return new Response(response.body, {
    status: response.status,
    headers: { "Content-Type": response.headers.get("content-type") ?? "application/json" },
  });
}

export const POST = withSession(handlePOST);
