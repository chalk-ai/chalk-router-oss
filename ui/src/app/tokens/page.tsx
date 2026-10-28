import {
  apiKeyClient,
  modelRouteClient,
  providerConnectionClient,
  rateLimitClient,
  usageBudgetClient,
  usagePoolClient,
} from "@/lib/grpc";
import { fetchAllGrpcPages } from "@/lib/grpcPagination";
import { ADMIN_PAGE_SIZE } from "@/lib/adminPagination";
import { apiKeyToJson, usageBudgetPeriodToJson } from "@/lib/serialize";
import type { KeyAccessJson, RouteOption } from "@/lib/keyAccess";
import { TokensClient } from "./TokensClient";

export const dynamic = "force-dynamic";

interface ApiKeyPermission extends KeyAccessJson {
  downgradable?: boolean | null;
  model_floor?: string | null;
}

interface ConnectionOption {
  id: string;
  name: string;
  provider_kind: string;
  prefix?: string;
}

interface ApiKeyView {
  key_hash: string;
  key_prefix: string;
  description: string;
  created_at: string;
  permission: ApiKeyPermission;
  labels: Record<string, string>;
  usage: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
    request_count: number;
  };
  daily_token_budget: number | null;
  usage_budgets: { id: string; token_limit: number; period: "daily" | "weekly" | "monthly"; enabled: boolean }[];
  usage_pool_id: string | null;
  rate_limits: {
    connection_id?: string;
    models: string[];
    requests_per_minute?: number | null;
    request_burst?: number | null;
    tokens_per_minute?: number | null;
    token_burst?: number | null;
    concurrent_requests?: number | null;
    enabled: boolean;
  }[];
}

interface UsagePool {
  id: string;
  name: string;
  created_at: string;
}

interface PoolQuotaPolicies {
  poolName: string;
  budgets: { token_limit: number; period: "daily" | "weekly" | "monthly"; enabled: boolean }[];
  rateLimits: { id: string; limit_type: "tokens_per_minute" | "requests_per_minute" | "concurrent_requests"; limit_value: number; burst_value?: number | null; connection_id?: string | null; models: string[]; enabled: boolean }[];
}

export default async function TokensPage({ searchParams, embedded = false }: { searchParams: Promise<{ usage_pool_id?: string | string[]; issue?: string | string[]; edit?: string | string[] }>; embedded?: boolean }) {
  const query = await searchParams;
  const requestedPoolId = Array.isArray(query.usage_pool_id) ? query.usage_pool_id[0] : query.usage_pool_id;
  const issue = Array.isArray(query.issue) ? query.issue[0] : query.issue;
  const edit = Array.isArray(query.edit) ? query.edit[0] : query.edit;
  let initialKeys: ApiKeyView[] = [];
  let initialNextCursor: string | undefined;
  let pools: UsagePool[] = [];
  let connections: ConnectionOption[] = [];
  let routes: RouteOption[] = [];
  let poolQuotaPolicies: Record<string, PoolQuotaPolicies> = {};
  let error: string | null = null;

  try {
    const [keyPage, rawPools, rawConnections, budgetPage, rateLimitPolicies, rawRoutes] = await Promise.all([
      apiKeyClient().listKeys({ limit: ADMIN_PAGE_SIZE }),
      fetchAllGrpcPages(
        (cursor) => usagePoolClient().listUsagePools({ limit: 1000, cursor }),
        (page) => page.pools,
      ),
      fetchAllGrpcPages(
        (cursor) => providerConnectionClient().listProviderConnections({ limit: 1000, cursor }),
        (page) => page.connections,
      ),
      usageBudgetClient().listUsageBudgets({ scope: "usage_pool" }),
      fetchAllGrpcPages(
        (cursor) => rateLimitClient().listRateLimits({ limit: 1000, cursor }),
        (page) => page.policies,
      ),
      // Every route, published or hidden: keys grant routes by id, and an id
      // with no route here is shown as a deleted route.
      fetchAllGrpcPages(
        (cursor) => modelRouteClient().listModelRoutes({ limit: 1000, cursor, filters: {} }),
        (page) => page.routes,
      ),
    ]);
    routes = rawRoutes.map((route) => ({
      id: route.id,
      public_model_id: route.publicModelId,
      published: route.published,
    }));
    initialKeys = keyPage.keys.map(apiKeyToJson);
    initialNextCursor = keyPage.nextCursor;
    pools = rawPools.map((t) => ({
      id: t.id,
      name: t.name,
      created_at: t.createdAt
        ? new Date(Number(t.createdAt.seconds) * 1000).toISOString()
        : "",
    }));
    connections = rawConnections.map((c) => ({
      id: c.id,
      name: c.name,
      provider_kind: c.providerKind,
      prefix: c.prefix,
    }));
    poolQuotaPolicies = Object.fromEntries(pools.map((pool) => [pool.id, { poolName: pool.name, budgets: [], rateLimits: [] }]));
    for (const budget of budgetPage.budgets) {
      const entry = poolQuotaPolicies[budget.scopeId] ?? { poolName: pools.find((pool) => pool.id === budget.scopeId)?.name ?? budget.scopeId, budgets: [], rateLimits: [] };
      entry.budgets.push({
        token_limit: Number(budget.tokenLimit),
        period: usageBudgetPeriodToJson(budget.period),
        enabled: budget.enabled,
      });
      poolQuotaPolicies[budget.scopeId] = entry;
    }
    for (const limit of rateLimitPolicies) {
      if (limit.scope !== "usage_pool") continue;
      const entry = poolQuotaPolicies[limit.scopeId] ?? { poolName: pools.find((pool) => pool.id === limit.scopeId)?.name ?? limit.scopeId, budgets: [], rateLimits: [] };
      entry.rateLimits.push({
        id: limit.id,
        limit_type: limit.limitType as "tokens_per_minute" | "requests_per_minute" | "concurrent_requests",
        limit_value: Number(limit.limitValue),
        burst_value: limit.burstValue != null ? Number(limit.burstValue) : null,
        connection_id: limit.connectionId,
        models: limit.models,
        enabled: limit.enabled,
      });
      poolQuotaPolicies[limit.scopeId] = entry;
    }
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
  }

  return (
    <div>
      {!embedded && <div className="mb-8">
        <h1 className="text-2xl font-semibold text-white tracking-tight">API Tokens</h1>
        <p className="mt-1 text-sm" style={{ color: "#6b7280" }}>
          Issue and manage API keys for accessing this router.
        </p>
      </div>}

      {error && (
        <div
          className="mb-6 px-4 py-3 rounded-lg text-sm"
          style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}
        >
          <span className="font-medium">Error loading tokens:</span> {error}
        </div>
      )}

      <TokensClient
        initialKeys={initialKeys}
        initialNextCursor={initialNextCursor}
        pools={pools}
        connections={connections}
        routes={routes}
        poolQuotaPolicies={poolQuotaPolicies}
        initialUsagePoolId={requestedPoolId && pools.some((pool) => pool.id === requestedPoolId) ? requestedPoolId : undefined}
        initialIssueOpen={issue === "1"}
        initialEditKeyHash={edit}
      />
    </div>
  );
}
