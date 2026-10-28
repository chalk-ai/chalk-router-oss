import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { apiKeyClient } from "@/lib/grpc";
import { apiKeyToJson, cacheScopeFromString } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, keyAccessFromJson, parseJsonObjectBody, parseOptionalPositiveBigInt, rateLimitsFromJson, type JsonObject, type KeyAccessProtoInput, type RateLimitProtoInput } from "@/lib/bff";
import { usageBudgetsFromJson, type UsageBudgetProtoInput } from "../../usage-pools/payload";

async function handleDELETE(
  _req: NextRequest,
  { params }: { params: Promise<{ key: string }> },
) {
  const { key } = await params;
  try {
    const res = await apiKeyClient().revokeKey({ key });
    return NextResponse.json({ key: res.key });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handlePATCH(
  req: NextRequest,
  { params }: { params: Promise<{ key: string }> },
) {
  const { key } = await params;
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;

    // The HTTP/JSON shape encodes "do not touch" as a missing field, and
    // "clear this field" as an explicit null. Translate into update + FieldMask.
    const hasField = (name: string) => hasOwn(body, name);

    const update: {
      description?: string;
      dailyTokenBudget?: bigint;
      usagePoolId?: string;
      costTags?: Record<string, string>;
      permission?: {
        connectionScopes: KeyAccessProtoInput["connectionScopes"];
        routes?: KeyAccessProtoInput["routes"];
        downgradable?: boolean;
        modelFloor?: string;
        cache?: {
          enabled: boolean;
          scope: ReturnType<typeof cacheScopeFromString>;
          ttlSecs?: bigint;
          maxEntryBytes?: bigint;
        };
      };
      rateLimits?: RateLimitProtoInput[];
      usageBudgets?: UsageBudgetProtoInput[];
    } = {};
    const paths: string[] = [];

    if (hasField("description")) {
      if (body.description !== null && typeof body.description !== "string") {
        return NextResponse.json(
          { error: { message: "description must be a string or null" } },
          { status: 400 },
        );
      }
      paths.push("description");
      update.description = body.description == null ? "" : body.description;
    }
    if (hasField("daily_token_budget")) {
      const dailyTokenBudget = parseOptionalPositiveBigInt(body.daily_token_budget, "daily_token_budget");
      if ("error" in dailyTokenBudget) return dailyTokenBudget.error;
      update.dailyTokenBudget = dailyTokenBudget.value;
      paths.push("daily_token_budget");
    }
    if (hasField("usage_pool_id")) {
      if (body.usage_pool_id !== null && typeof body.usage_pool_id !== "string") {
        return NextResponse.json(
          { error: { message: "usage_pool_id must be a string or null" } },
          { status: 400 },
        );
      }
      paths.push("usage_pool_id");
      if (body.usage_pool_id != null) {
        update.usagePoolId = body.usage_pool_id;
      }
    }
    if (hasField("cost_tags")) {
      if (
        body.cost_tags !== null &&
        (typeof body.cost_tags !== "object" || Array.isArray(body.cost_tags))
      ) {
        return NextResponse.json(
          { error: { message: "cost_tags must be an object or null" } },
          { status: 400 },
        );
      }
      paths.push("cost_tags");
      update.costTags = (body.cost_tags as Record<string, string> | null) ?? {};
    }
    if (hasField("permission")) {
      if (
        body.permission !== null &&
        (typeof body.permission !== "object" || Array.isArray(body.permission))
      ) {
        return NextResponse.json(
          { error: { message: "permission must be an object or null" } },
          { status: 400 },
        );
      }
      paths.push("permission");
      // A null permission clears it: no routes and no connection scopes,
      // which makes the key unrestricted.
      const permission = (body.permission ?? {}) as JsonObject & {
        downgradable?: unknown;
        model_floor?: unknown;
        cache?: unknown;
      };
      const access = keyAccessFromJson(permission);
      if ("error" in access) return access.error;
      if (permission.downgradable != null && typeof permission.downgradable !== "boolean") {
        return NextResponse.json({ error: { message: "permission.downgradable must be a boolean" } }, { status: 400 });
      }
      if (permission.model_floor != null && typeof permission.model_floor !== "string") {
        return NextResponse.json({ error: { message: "permission.model_floor must be a string" } }, { status: 400 });
      }
      if (
        permission.cache != null &&
        (typeof permission.cache !== "object" || Array.isArray(permission.cache))
      ) {
        return NextResponse.json({ error: { message: "permission.cache must be an object" } }, { status: 400 });
      }
      const cache = permission.cache as {
        enabled?: unknown;
        scope?: unknown;
        ttl_secs?: unknown;
        max_entry_bytes?: unknown;
      } | undefined;
      if (cache && (typeof cache.enabled !== "boolean" || typeof cache.scope !== "string")) {
        return NextResponse.json({ error: { message: "permission.cache.enabled and scope are required" } }, { status: 400 });
      }
      let cacheTtlSecs: bigint | undefined;
      let cacheMaxEntryBytes: bigint | undefined;
      if (cache) {
        const ttl = parseOptionalPositiveBigInt(cache.ttl_secs, "permission.cache.ttl_secs");
        if ("error" in ttl) return ttl.error;
        cacheTtlSecs = ttl.value;
        const maxBytes = parseOptionalPositiveBigInt(cache.max_entry_bytes, "permission.cache.max_entry_bytes");
        if ("error" in maxBytes) return maxBytes.error;
        cacheMaxEntryBytes = maxBytes.value;
      }
      update.permission = {
        connectionScopes: access.value.connectionScopes,
        routes: access.value.routes,
        downgradable: typeof permission.downgradable === "boolean" ? permission.downgradable : undefined,
        modelFloor: typeof permission.model_floor === "string" ? permission.model_floor : undefined,
        cache: cache ? {
          enabled: cache.enabled as boolean,
          scope: cacheScopeFromString(cache.scope as string),
          ttlSecs: cacheTtlSecs,
          maxEntryBytes: cacheMaxEntryBytes,
        } : undefined,
      };
    }
    if (hasField("rate_limits")) {
      const rateLimits = rateLimitsFromJson(body.rate_limits);
      if ("error" in rateLimits) return rateLimits.error;
      update.rateLimits = rateLimits.value;
      paths.push("rate_limits");
    }
    if (hasField("usage_budgets")) {
      const usageBudgets = usageBudgetsFromJson(body.usage_budgets);
      if ("error" in usageBudgets) return usageBudgets.error;
      update.usageBudgets = usageBudgets.value;
      paths.push("usage_budgets");
    }

    if (paths.length === 0) {
      return NextResponse.json(
        { error: { message: "no updatable fields in request" } },
        { status: 400 },
      );
    }

    const res = await apiKeyClient().updateKey({
      key,
      update,
      updateMask: { paths },
    });
    if (!res.key) {
      return NextResponse.json(
        { error: { message: "Backend returned no key" } },
        { status: 500 },
      );
    }
    return NextResponse.json(apiKeyToJson(res.key));
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const DELETE = withSession(handleDELETE);
export const PATCH = withSession(handlePATCH);
