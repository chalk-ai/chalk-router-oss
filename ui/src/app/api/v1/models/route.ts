import { withSession } from "@/lib/auth";
import { NextResponse } from "next/server";
import { proxyToRouter } from "@/lib/proxy";

async function handleGET() {
  const res = await proxyToRouter("/v1/models", { method: "GET" });
  const data = await res.json();
  return NextResponse.json(data, { status: res.status });
}

export const GET = withSession(handleGET);
