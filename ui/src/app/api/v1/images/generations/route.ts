import { withSession } from "@/lib/auth";
import { NextRequest } from "next/server";
import { proxyToRouter } from "@/lib/proxy";

async function handlePOST(req: NextRequest) {
  const body = await req.json();
  const res = await proxyToRouter("/v1/images/generations", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const data = await res.json();
  return Response.json(data, { status: res.status });
}

export const POST = withSession(handlePOST);
