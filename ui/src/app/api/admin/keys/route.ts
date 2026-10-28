import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { apiKeyClient } from "@/lib/grpc";
import { adminPageRequest } from "@/lib/adminPagination";
import { apiKeyToJson, cacheScopeFromString } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, keyAccessFromJson, parseJsonObjectBody, parseOptionalPositiveBigInt, rateLimitsFromJson, type JsonObject, type KeyAccessProtoInput } from "@/lib/bff";
import { usageBudgetsFromJson } from "../usage-pools/payload";

async function handleGET(req: NextRequest) {
  try {
    const page = await apiKeyClient().listKeys(adminPageRequest(req));
    return NextResponse.json({
      object: "list",
      data: page.keys.map(apiKeyToJson),
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
    if (hasOwn(body, "cost_tags")) {
      return NextResponse.json(
        { error: { message: "cost_tags is deprecated; use immutable labels" } },
        { status: 400 },
      );
    }
    if (
      body.permission != null &&
      (typeof body.permission !== "object" || Array.isArray(body.permission))
    ) {
      return NextResponse.json({ error: { message: "permission must be an object" } }, { status: 400 });
    }
    const permission = (body.permission ?? undefined) as
      | (JsonObject & {
          downgradable?: boolean;
          model_floor?: string;
          cache?: { enabled?: boolean; scope?: string; ttl_secs?: number | string; max_entry_bytes?: number | string };
        })
      | undefined;
    let access: KeyAccessProtoInput = { connectionScopes: [] };
    if (permission) {
      const parsedAccess = keyAccessFromJson(permission);
      if ("error" in parsedAccess) return parsedAccess.error;
      access = parsedAccess.value;
    }
    let cacheTtlSecs: bigint | undefined;
    let cacheMaxEntryBytes: bigint | undefined;
    if (permission?.cache) {
      const ttl = parseOptionalPositiveBigInt(permission.cache.ttl_secs, "permission.cache.ttl_secs");
      if ("error" in ttl) return ttl.error;
      cacheTtlSecs = ttl.value;
      const maxBytes = parseOptionalPositiveBigInt(permission.cache.max_entry_bytes, "permission.cache.max_entry_bytes");
      if ("error" in maxBytes) return maxBytes.error;
      cacheMaxEntryBytes = maxBytes.value;
    }
    const dailyTokenBudget = parseOptionalPositiveBigInt(body.daily_token_budget, "daily_token_budget");
    if ("error" in dailyTokenBudget) return dailyTokenBudget.error;
    const rateLimits = rateLimitsFromJson(body.rate_limits);
    if ("error" in rateLimits) return rateLimits.error;
    const usageBudgets = usageBudgetsFromJson(body.usage_budgets);
    if ("error" in usageBudgets) return usageBudgets.error;

    const res = await apiKeyClient().createKey({
      description: (body.description as string | undefined) ?? "",
      permission: permission
        ? {
            // Access is the union of model-route grants and connection
            // scopes; `routes` is present on every restricted key.
            connectionScopes: access.connectionScopes,
            routes: access.routes,
            // Model-as-judge per-key overrides. `downgradable` is tri-state:
            // omit to inherit the policy default, true/false to override.
            downgradable: typeof permission.downgradable === "boolean" ? permission.downgradable : undefined,
            modelFloor: permission.model_floor || undefined,
            // Exact-prefix completion caching. Omitted entirely unless the
            // caller sends a policy: an absent `cache` means the key is never
            // cached, which is not the same as a policy with `enabled: false`.
            cache: permission.cache
              ? {
                  enabled: permission.cache.enabled !== false,
                  scope: cacheScopeFromString(permission.cache.scope),
                  ttlSecs: cacheTtlSecs,
                  maxEntryBytes: cacheMaxEntryBytes,
                }
              : undefined,
          }
        : undefined,
      labels: (body.labels as Record<string, string> | undefined) ?? {},
      dailyTokenBudget: dailyTokenBudget.value,
      costTags: {},
      usagePoolId: body.usage_pool_id as string | undefined,
      rateLimits: rateLimits.value,
      usageBudgets: usageBudgets.value,
    });
    if (!res.key) {
      return NextResponse.json(
        { error: { message: "Backend returned no key" } },
        { status: 500 },
      );
    }
    // The raw secret is returned exactly once, here at creation.
    return NextResponse.json(
      { ...apiKeyToJson(res.key), raw_key: res.rawKey },
      { status: 201 },
    );
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const POST = withSession(handlePOST);
