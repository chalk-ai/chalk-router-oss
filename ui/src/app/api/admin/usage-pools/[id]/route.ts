import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { ConnectError, Code } from "@connectrpc/connect";
import { usagePoolClient } from "@/lib/grpc";
import { connectErrorResponse } from "@/lib/connectError";
import { usagePoolToJson } from "@/lib/serialize";
import { hasOwn, parseJsonObjectBody } from "@/lib/bff";
import { rateLimitsFromJson, usageBudgetsFromJson } from "../payload";

async function handleGET(
  _req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const res = await usagePoolClient().getUsagePool({ lookup: { case: "id", value: id } });
    if (!res.pool) {
      return NextResponse.json(
        { error: { message: "Backend returned no usage pool" } },
        { status: 500 },
      );
    }
    return NextResponse.json(usagePoolToJson(res.pool));
  } catch (e) {
    if (e instanceof ConnectError && e.code === Code.NotFound) {
      return NextResponse.json({ error: { message: "Usage pool not found" } }, { status: 404 });
    }
    return connectErrorResponse(e);
  }
}

async function handleDELETE(
  _req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const res = await usagePoolClient().deleteUsagePool({ id });
    return NextResponse.json({ id: res.id });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handlePATCH(
  req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const paths: string[] = [];
    if (hasOwn(body, "name")) paths.push("name");
    if (hasOwn(body, "rate_limits")) paths.push("rate_limits");
    if (hasOwn(body, "usage_budgets")) paths.push("usage_budgets");

    const rateLimits = rateLimitsFromJson(body.rate_limits);
    if ("error" in rateLimits) return rateLimits.error;
    const usageBudgets = usageBudgetsFromJson(body.usage_budgets);
    if ("error" in usageBudgets) return usageBudgets.error;

    const res = await usagePoolClient().updateUsagePool({
      id,
      update: {
        name: paths.includes("name") ? (body.name as string | undefined) ?? "" : undefined,
        rateLimits: rateLimits.value,
        usageBudgets: usageBudgets.value,
      },
      updateMask: { paths },
    });
    if (!res.pool) {
      return NextResponse.json(
        { error: { message: "Backend returned no usage pool" } },
        { status: 500 },
      );
    }
    return NextResponse.json(usagePoolToJson(res.pool));
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const DELETE = withSession(handleDELETE);
export const PATCH = withSession(handlePATCH);
