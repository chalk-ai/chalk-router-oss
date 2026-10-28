import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { rateLimitClient } from "@/lib/grpc";
import { rateLimitToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, isRateLimitType, parseJsonObjectBody, parsePositiveBigInt, RATE_LIMIT_TYPES, validateRateLimitModels } from "@/lib/bff";

async function handlePATCH(
  req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const update: {
      enabled?: boolean;
      limitType?: string;
      limitValue?: bigint;
      burstValue?: bigint;
      connectionId?: string;
      models: string[];
    } = { models: [] };
    const paths: string[] = [];
    if (hasOwn(body, "scope") || hasOwn(body, "scope_id")) {
      return NextResponse.json(
        { error: { message: "Applies to and Target cannot be changed after a rate limit is created." } },
        { status: 400 },
      );
    }
    if (hasOwn(body, "enabled")) {
      if (typeof body.enabled !== "boolean") {
        return NextResponse.json(
          { error: { message: "enabled must be a boolean" } },
          { status: 400 },
        );
      }
      paths.push("enabled");
      update.enabled = body.enabled;
    }
    if (hasOwn(body, "limit_type")) {
      if (!isRateLimitType(body.limit_type)) {
        return NextResponse.json(
          { error: { message: `limit_type must be ${RATE_LIMIT_TYPES.join(", ")}` } },
          { status: 400 },
        );
      }
      paths.push("limit_type");
      update.limitType = body.limit_type;
    }
    if (hasOwn(body, "limit_value")) {
      const value = parsePositiveBigInt(body.limit_value, "limit_value");
      if ("error" in value) return value.error;
      paths.push("limit_value");
      update.limitValue = value.value;
    }
    if (hasOwn(body, "burst_value")) {
      if (body.burst_value != null) {
        const value = parsePositiveBigInt(body.burst_value, "burst_value");
        if ("error" in value) return value.error;
        update.burstValue = value.value;
      }
      paths.push("burst_value");
    }
    if (hasOwn(body, "connection_id")) {
      if (body.connection_id != null && typeof body.connection_id !== "string") return NextResponse.json({ error: { message: "connection_id must be a string or null" } }, { status: 400 });
      paths.push("connection_id");
      if (typeof body.connection_id === "string") update.connectionId = body.connection_id;
    }
    if (hasOwn(body, "models")) {
      if (!Array.isArray(body.models) || !body.models.every((model) => typeof model === "string")) return NextResponse.json({ error: { message: "models must be an array of strings" } }, { status: 400 });
      const models = validateRateLimitModels(body.models);
      if ("error" in models) return models.error;
      paths.push("models");
      update.models = body.models;
    }
    // Best-effort mirror of `validate_and_normalize` in `src/service/rate_limits.rs`:
    // the server re-validates the whole merged record on update, but a PATCH here
    // only sees the fields present in this request. These cross-field checks catch
    // the common case where both sides of the rule are present in the same request;
    // combinations split across separate PATCH calls still fall through to the
    // server's own validation (returned as a 400 via connectErrorResponse).
    if (update.limitType === "concurrent_requests" && update.burstValue != null && update.limitValue != null && update.burstValue !== update.limitValue) {
      return NextResponse.json(
        { error: { message: "burst_value must be absent or equal limit_value for concurrent_requests" } },
        { status: 400 },
      );
    }
    if (paths.includes("models") && update.models.length > 0 && paths.includes("connection_id") && !update.connectionId) {
      return NextResponse.json(
        { error: { message: "models requires connection_id" } },
        { status: 400 },
      );
    }
    if (paths.length === 0) {
      return NextResponse.json(
        { error: { message: "no updatable fields in request" } },
        { status: 400 },
      );
    }
    const res = await rateLimitClient().updateRateLimit({
      id,
      update,
      updateMask: { paths },
    });
    if (!res.policy) {
      return NextResponse.json(
        { error: { message: "Backend returned no policy" } },
        { status: 500 },
      );
    }
    return NextResponse.json(rateLimitToJson(res.policy));
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handleDELETE(
  _req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const res = await rateLimitClient().deleteRateLimit({ id });
    return NextResponse.json({ id: res.id });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const PATCH = withSession(handlePATCH);
export const DELETE = withSession(handleDELETE);
