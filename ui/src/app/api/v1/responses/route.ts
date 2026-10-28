import { withSession } from "@/lib/auth";
import { NextRequest } from "next/server";
import { proxyToRouter } from "@/lib/proxy";

async function handlePOST(req: NextRequest) {
  const body = await req.text();
  const res = await proxyToRouter("/v1/responses", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body,
  });

  return new Response(res.body, {
    status: res.status,
    headers: {
      "Content-Type": res.headers.get("Content-Type") ?? "application/json",
      "Cache-Control": res.headers.get("Cache-Control") ?? "no-cache",
    },
  });
}

export const POST = withSession(handlePOST);
