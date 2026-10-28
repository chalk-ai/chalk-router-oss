import { withSession } from "@/lib/auth";
import { NextResponse } from "next/server";

function retired() {
  return NextResponse.json({ error: { message: "Global policies have been retired. Configure judging and fallbacks on a model route." } }, { status: 410 });
}
export const GET = withSession(retired);
export const PUT = withSession(retired);
