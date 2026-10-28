import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { rateLimitClient } from "@/lib/grpc";
import { adminPageRequest } from "@/lib/adminPagination";
import { rateLimitToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import {
  isRateLimitType,
  parseJsonObjectBody,
  parseOptionalPositiveBigInt,
  parsePositiveBigInt,
  RATE_LIMIT_TYPES,
  validateRateLimitModels,
} from "@/lib/bff";

const RATE_LIMIT_SCOPES = ["api_key", "usage_pool"] as const;

async function handleGET(req: NextRequest) {
  try {
    const page = await rateLimitClient().listRateLimits(adminPageRequest(req));
    return NextResponse.json({
      data: page.policies.map(rateLimitToJson),
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

    // Mirrors `validate_and_normalize` in `src/service/rate_limits.rs`. Unlike
    // the PATCH route, create() always sees a complete record, so the full
    // rule set applies here without partial-field ambiguity.
    if (!isRateLimitType(body.limit_type)) {
      return NextResponse.json(
        { error: { message: `limit_type must be ${RATE_LIMIT_TYPES.join(", ")}` } },
        { status: 400 },
      );
    }
    const limitType = body.limit_type;

    const limitValue = parsePositiveBigInt(body.limit_value, "limit_value");
    if ("error" in limitValue) return limitValue.error;

    let burstValue: bigint | undefined;
    if (limitType === "concurrent_requests") {
      if (body.burst_value != null) {
        const burst = parsePositiveBigInt(body.burst_value, "burst_value");
        if ("error" in burst) return burst.error;
        if (burst.value !== limitValue.value) {
          return NextResponse.json(
            { error: { message: "burst_value must be absent or equal limit_value for concurrent_requests" } },
            { status: 400 },
          );
        }
      }
      burstValue = undefined;
    } else {
      const burst = parseOptionalPositiveBigInt(body.burst_value, "burst_value");
      if ("error" in burst) return burst.error;
      burstValue = burst.value ?? limitValue.value;
    }

    if (body.connection_id != null && typeof body.connection_id !== "string") {
      return NextResponse.json({ error: { message: "connection_id must be a string" } }, { status: 400 });
    }
    const connectionId =
      typeof body.connection_id === "string" && body.connection_id.trim()
        ? body.connection_id.trim()
        : undefined;

    if (
      body.models != null &&
      (!Array.isArray(body.models) || !body.models.every((model) => typeof model === "string"))
    ) {
      return NextResponse.json({ error: { message: "models must be an array of strings" } }, { status: 400 });
    }
    const models = Array.isArray(body.models)
      ? body.models.map((model) => String(model).trim()).filter((model) => model.length > 0)
      : [];

    const validatedModels = validateRateLimitModels(models);
    if ("error" in validatedModels) return validatedModels.error;

    if (!connectionId && models.length > 0) {
      return NextResponse.json({ error: { message: "models requires connection_id" } }, { status: 400 });
    }

    if (
      typeof body.scope !== "string" ||
      !(RATE_LIMIT_SCOPES as readonly string[]).includes(body.scope.trim())
    ) {
      return NextResponse.json(
        { error: { message: `scope must be ${RATE_LIMIT_SCOPES.join(" or ")}` } },
        { status: 400 },
      );
    }
    const scope = body.scope.trim();

    if (typeof body.scope_id !== "string" || !body.scope_id.trim()) {
      return NextResponse.json(
        { error: { message: "scope_id must not be empty for api_key or usage_pool scope" } },
        { status: 400 },
      );
    }

    const res = await rateLimitClient().createRateLimit({
      limitType,
      limitValue: limitValue.value,
      burstValue,
      connectionId,
      models,
      scope,
      scopeId: body.scope_id,
      enabled: typeof body.enabled === "boolean" ? body.enabled : true,
    });
    if (!res.policy) {
      return NextResponse.json(
        { error: { message: "Backend returned no policy" } },
        { status: 500 },
      );
    }
    return NextResponse.json(rateLimitToJson(res.policy), { status: 201 });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const POST = withSession(handlePOST);
