import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { usagePoolClient } from "@/lib/grpc";
import { adminPageRequest } from "@/lib/adminPagination";
import { usagePoolToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { parseJsonObjectBody } from "@/lib/bff";
import { rateLimitsFromJson, usageBudgetsFromJson } from "./payload";

async function handleGET(req: NextRequest) {
  try {
    const page = await usagePoolClient().listUsagePools(adminPageRequest(req));
    return NextResponse.json({
      data: page.pools.map(usagePoolToJson),
      next_cursor: page.nextCursor,
    });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handlePOST(req: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const rateLimits = rateLimitsFromJson(body.rate_limits);
    if ("error" in rateLimits) return rateLimits.error;
    const usageBudgets = usageBudgetsFromJson(body.usage_budgets);
    if ("error" in usageBudgets) return usageBudgets.error;
    const res = await usagePoolClient().createUsagePool({
      name: (body.name as string | undefined) ?? "",
      rateLimits: rateLimits.value,
      usageBudgets: usageBudgets.value,
    });
    if (!res.pool) {
      return NextResponse.json(
        { error: { message: "Backend returned no usage pool" } },
        { status: 500 },
      );
    }
    return NextResponse.json(usagePoolToJson(res.pool), { status: 201 });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const POST = withSession(handlePOST);
