import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { usageBudgetClient } from "@/lib/grpc";
import { usageBudgetToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, parseJsonObjectBody, parsePositiveBigInt } from "@/lib/bff";

async function handlePATCH(req: NextRequest, { params }: { params: Promise<{ id: string }> }) {
  try {
    const { id } = await params;
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const paths: string[] = [];
    const update: { tokenLimit?: bigint; enabled?: boolean } = {};
    if (hasOwn(body, "token_limit")) {
      const tokenLimit = parsePositiveBigInt(body.token_limit, "token_limit");
      if ("error" in tokenLimit) return tokenLimit.error;
      update.tokenLimit = tokenLimit.value;
      paths.push("token_limit");
    }
    if (hasOwn(body, "enabled")) {
      if (typeof body.enabled !== "boolean") return NextResponse.json({ error: { message: "enabled must be boolean" } }, { status: 400 });
      update.enabled = body.enabled;
      paths.push("enabled");
    }
    if (paths.length === 0) return NextResponse.json({ error: { message: "provide token_limit or enabled" } }, { status: 400 });
    const response = await usageBudgetClient().updateUsageBudget({ id, update, updateMask: { paths } });
    if (!response.budget) return NextResponse.json({ error: { message: "Backend returned no usage budget" } }, { status: 500 });
    return NextResponse.json(usageBudgetToJson(response.budget));
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handleDELETE(_req: NextRequest, { params }: { params: Promise<{ id: string }> }) {
  try {
    const { id } = await params;
    const response = await usageBudgetClient().deleteUsageBudget({ id });
    return NextResponse.json({ id: response.id });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const PATCH = withSession(handlePATCH);
export const DELETE = withSession(handleDELETE);
