import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { parseJsonObjectBody } from "@/lib/bff";
import { connectErrorResponse } from "@/lib/connectError";
import { modelRouteClient } from "@/lib/grpc";
import { judgeBackendError, judgeBackendToProto, type JudgeBackend } from "@/lib/routeGrid";

async function handlePOST(req: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const issue = judgeBackendError(parsed.body.backend);
    if (issue) return NextResponse.json({ error: { message: issue } }, { status: 400 });
    await modelRouteClient().validateRouteJudgeBackend({ backend: judgeBackendToProto(parsed.body.backend as JudgeBackend) });
    return NextResponse.json({ valid: true });
  } catch (error) { return connectErrorResponse(error); }
}

export const POST = withSession(handlePOST);
