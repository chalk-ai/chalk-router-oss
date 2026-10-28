import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { usageBudgetClient } from "@/lib/grpc";
import { usageBudgetToJson, usageBudgetPeriodFromJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { parseJsonObjectBody, parsePositiveBigInt } from "@/lib/bff";

async function handleGET(req: NextRequest) {
  try {
    const scope = req.nextUrl.searchParams.get("scope") ?? undefined;
    const scopeId = req.nextUrl.searchParams.get("scope_id") ?? undefined;
    if (scope != null && scope !== "api_key" && scope !== "usage_pool") {
      return NextResponse.json({ error: { message: "scope must be api_key or usage_pool" } }, { status: 400 });
    }
    const response = await usageBudgetClient().listUsageBudgets({ scope, scopeId });
    return NextResponse.json({ data: response.budgets.map(usageBudgetToJson) });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handlePOST(req: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    if (body.scope !== "api_key" && body.scope !== "usage_pool") {
      return NextResponse.json({ error: { message: "scope must be api_key or usage_pool" } }, { status: 400 });
    }
    if (typeof body.scope_id !== "string" || !body.scope_id.trim()) {
      return NextResponse.json({ error: { message: "scope_id must not be empty" } }, { status: 400 });
    }
    const amount = parsePositiveBigInt(body.token_limit, "token_limit");
    if ("error" in amount) return amount.error;
    const period = usageBudgetPeriodFromJson(body.period as string | undefined);
    if (period === undefined) {
      return NextResponse.json({ error: { message: "period must be daily, weekly, or monthly" } }, { status: 400 });
    }
    const response = await usageBudgetClient().createUsageBudget({
      scope: body.scope,
      scopeId: body.scope_id.trim(),
      tokenLimit: amount.value,
      period,
      enabled: typeof body.enabled === "boolean" ? body.enabled : undefined,
    });
    if (!response.budget) return NextResponse.json({ error: { message: "Backend returned no usage budget" } }, { status: 500 });
    return NextResponse.json(usageBudgetToJson(response.budget), { status: 201 });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const POST = withSession(handlePOST);
