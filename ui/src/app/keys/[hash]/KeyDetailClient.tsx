"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { DetailTabs } from "@/components/DetailTabs";
import { DetailRows, EntityHeader, ErrorBanner, SkeletonRows, StatusBadge } from "@/components/router/RouterPrimitives";
import UsageExplorer from "@/app/usage/UsageExplorer";
import { fetchAllAdminPages } from "@/lib/adminPagination";
import { grantedRouteIds, keyAccessKind, routeGrantLabels, type KeyAccessJson, type RouteOption } from "@/lib/keyAccess";

type Permission = KeyAccessJson & { downgradable?: boolean | null; model_floor?: string | null };
type UsageBudget = { id: string; token_limit: number; period: "daily" | "weekly" | "monthly"; enabled: boolean };
type RateLimit = { connection_id?: string; models?: string[]; requests_per_minute?: number | null; request_burst?: number | null; tokens_per_minute?: number | null; token_burst?: number | null; concurrent_requests?: number | null; enabled?: boolean };
type Key = { key_hash: string; key_prefix: string; description: string; created_at: string; labels: Record<string, string>; permission: Permission; usage_pool_id: string | null; usage_budgets: UsageBudget[]; rate_limits: RateLimit[]; usage: { prompt_tokens: number; completion_tokens: number; total_tokens: number; request_count: number } };
type Connection = { id: string; name: string };

function rateLimitSummary(limit: RateLimit): string[] {
  const pieces: string[] = [];
  if (limit.requests_per_minute != null) pieces.push(`${limit.requests_per_minute.toLocaleString()} requests / min${limit.request_burst != null ? ` · burst ${limit.request_burst.toLocaleString()}` : ""}`);
  if (limit.tokens_per_minute != null) pieces.push(`${limit.tokens_per_minute.toLocaleString()} tokens / min${limit.token_burst != null ? ` · burst ${limit.token_burst.toLocaleString()}` : ""}`);
  if (limit.concurrent_requests != null) pieces.push(`${limit.concurrent_requests.toLocaleString()} concurrent requests`);
  return pieces;
}

export function KeyDetailClient({ keyHash }: { keyHash: string }) {
  const [key, setKey] = useState<Key | null>(null);
  const [connections, setConnections] = useState<Connection[]>([]);
  const [routes, setRoutes] = useState<RouteOption[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => { void (async () => {
    try {
      const [keys, allConnections, allRoutes] = await Promise.all([fetchAllAdminPages<Key>("/api/admin/keys"), fetchAllAdminPages<Connection>("/api/admin/providers"), fetchAllAdminPages<RouteOption>("/api/admin/model-routes")]);
      const found = keys.find((candidate) => candidate.key_hash === keyHash) ?? null;
      if (!found) throw new Error("API key not found");
      setKey(found); setConnections(allConnections); setRoutes(allRoutes);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  })(); }, [keyHash]);

  if (!key && !error) return <SkeletonRows rows={5} />;
  if (!key) return <div><Link href="/access?v=keys" className="text-sm" style={{ color: "#70CD96" }}>← API keys</Link><p className="mt-5 rounded px-4 py-3 text-sm" style={{ background: "#1c0a0a", color: "#fca5a5" }}>{error}</p></div>;

  const card = { background: "#111827", border: "1px solid #1f2937" };
  const usage = key.usage ?? { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, request_count: 0 };
  const accessKind = keyAccessKind(key.permission);
  const scopes = key.permission.connection_scopes ?? [];
  const routeGrants = routeGrantLabels(grantedRouteIds(key.permission), new Map(routes.map((route) => [route.id, route])));
  const connectionName = (id: string) => connections.find((connection) => connection.id === id)?.name ?? id;
  const tile = { background: "#0f172a", border: "1px solid #334155" };
  const permissions = accessKind === "all"
    ? <p className="rounded-md px-4 py-3 text-sm" style={{ background: "#0f172a", color: "#d1d5db" }}>All models: every model route and every model on every connection.</p>
    : accessKind === "none"
      ? <p className="rounded-md px-4 py-3 text-sm" style={{ background: "#1c0a0a", color: "#fca5a5" }}>No access: this key is restricted but grants no model routes or connection scopes, so it cannot call any model.</p>
      : <div className="space-y-5">
        <div><h3 className="mb-2 text-xs font-medium" style={{ color: "#9ca3af" }}>Model routes</h3>{routeGrants.length ? <div className="space-y-2">{routeGrants.map((route) => route.publicModelId ? <Link key={route.id} href={`/routes/${encodeURIComponent(route.id)}`} className="block rounded-md px-4 py-3 hover:bg-slate-800" style={tile}><p className="font-mono text-sm text-white">{route.publicModelId}</p><p className="mt-1 text-xs" style={{ color: "#9ca3af" }}>Its default target, judge selections and fallbacks</p></Link> : <div key={route.id} className="rounded-md px-4 py-3" style={tile}><p className="text-sm" style={{ color: "#fca5a5" }}>Deleted route</p><p className="mt-1 font-mono text-xs" style={{ color: "#9ca3af" }}>{route.id} · remove it by editing the key</p></div>)}</div> : <p className="text-sm" style={{ color: "#6b7280" }}>No model routes granted.</p>}</div>
        <div><h3 className="mb-2 text-xs font-medium" style={{ color: "#9ca3af" }}>Connection scopes</h3>{scopes.length ? <div className="space-y-2">{scopes.map((scope, index) => <div key={`${scope.connection_id}-${index}`} className="rounded-md px-4 py-3" style={tile}><p className="text-sm text-white">{connectionName(scope.connection_id)}</p><p className="mt-1 text-xs" style={{ color: "#9ca3af" }}>{scope.models?.length ? `Models: ${scope.models.join(", ")}` : "All direct models on this connection"}</p></div>)}</div> : <p className="text-sm" style={{ color: "#6b7280" }}>No connection scopes granted.</p>}</div>
      </div>;
  const editHref = `/access?v=keys&edit=${encodeURIComponent(key.key_hash)}`;

  return <div>
    <EntityHeader backHref="/access?v=keys" backLabel="Back to API keys" label="API key" name={key.description || key.key_prefix} metadata={<StatusBadge tone="green">Active</StatusBadge>} actions={<Link href={editHref} className="rounded px-3 py-1.5 text-xs font-medium" style={{ background: "#172554", color: "#93c5fd", border: "1px solid #1e3a5f" }}>Edit API key</Link>} />
    {error && <div className="mt-5"><ErrorBanner>{error}</ErrorBanner></div>}
    <DetailTabs tabs={[
      { id: "permissions", label: "Permissions", content: <section className="rounded-lg p-5" style={card}><div className="mb-4 flex items-center justify-between"><div><h2 className="text-sm font-semibold text-white">Model access</h2><p className="mt-1 text-xs" style={{ color: "#9ca3af" }}>A restricted key can call the model routes it is granted plus the direct models its connection scopes allow.</p></div><Link href={editHref} className="text-xs" style={{ color: "#70CD96" }}>Edit permissions</Link></div>{permissions}{key.usage_pool_id && <p className="mt-4 text-xs" style={{ color: "#9ca3af" }}>Usage pool: <Link href={`/usage-pools/${encodeURIComponent(key.usage_pool_id)}`} style={{ color: "#70CD96" }}>{key.usage_pool_id}</Link></p>}</section> },
      { id: "limits", label: "Limits", content: <section className="rounded-lg p-5" style={card}><div className="mb-4 flex items-center justify-between"><div><h2 className="text-sm font-semibold text-white">Key-specific limits</h2><p className="mt-1 text-xs" style={{ color: "#9ca3af" }}>Direct limits are additive to any limits inherited from the usage pool.</p></div><Link href={editHref} className="text-xs" style={{ color: "#70CD96" }}>Edit limits</Link></div><div className="grid gap-4 sm:grid-cols-2"><div><h3 className="mb-2 text-xs font-medium" style={{ color: "#9ca3af" }}>Usage budgets</h3>{key.usage_budgets.length ? <div className="space-y-2">{key.usage_budgets.map((budget) => <div key={budget.id} className="rounded-md px-3 py-2 text-sm" style={{ background: "#0f172a", border: "1px solid #334155", color: budget.enabled ? "#e5e7eb" : "#6b7280" }}><span className="capitalize">{budget.period}</span> · {budget.token_limit.toLocaleString()} tokens{budget.enabled ? "" : " · Disabled"}</div>)}</div> : <p className="text-sm" style={{ color: "#6b7280" }}>No key-specific usage budgets.</p>}</div><div><h3 className="mb-2 text-xs font-medium" style={{ color: "#9ca3af" }}>Rate limits</h3>{key.rate_limits.length ? <div className="space-y-2">{key.rate_limits.flatMap((limit, index) => rateLimitSummary(limit).map((summary) => <div key={`${index}-${summary}`} className="rounded-md px-3 py-2 text-sm" style={{ background: "#0f172a", border: "1px solid #334155", color: limit.enabled === false ? "#6b7280" : "#e5e7eb" }}><p>{summary}{limit.enabled === false ? " · Disabled" : ""}</p><p className="mt-1 text-xs" style={{ color: "#9ca3af" }}>{limit.connection_id ? connectionName(limit.connection_id) : "All connections"}{limit.models?.length ? ` · ${limit.models.join(", ")}` : " · all models"}</p></div>))}</div> : <p className="text-sm" style={{ color: "#6b7280" }}>No key-specific rate limits.</p>}</div></div></section> },
      { id: "usage", label: "Usage", content: <UsageExplorer fixedApiKey={key.key_hash} /> },
      { id: "traces", label: "Traces", content: <section className="rounded-lg p-6" style={card}><h2 className="text-sm font-semibold text-white">Trace export</h2><p className="mt-2 max-w-2xl text-sm leading-6 text-slate-400">This standalone router exports OpenInference spans but does not store or query them locally. Filter your configured tracing destination by API key hash <code className="text-slate-200">{key.key_hash}</code> to inspect requests made with this key.</p><Link href="/telemetry" className="mt-4 inline-flex text-sm" style={{ color: "#70CD96" }}>Configure telemetry exports →</Link></section> },
    ]} sidebar={<DetailRows rows={[
      { label: "Prefix", value: <span className="font-mono">{key.key_prefix}</span> },
      { label: "Key hash", value: <span className="font-mono text-[10px]">{key.key_hash}</span> },
      { label: "Created", value: new Date(key.created_at).toLocaleString() },
      { label: "Access", value: accessKind === "all" ? "All models" : accessKind === "none" ? "No access" : `${routeGrants.length} route${routeGrants.length === 1 ? "" : "s"} · ${scopes.length} connection scope${scopes.length === 1 ? "" : "s"}` },
      { label: "Usage pool", value: key.usage_pool_id ? <Link href={`/usage-pools/${encodeURIComponent(key.usage_pool_id)}`} style={{ color: "#70CD96" }}>{key.usage_pool_id}</Link> : "None" },
      { label: "Requests", value: usage.request_count.toLocaleString() },
      { label: "Total tokens", value: usage.total_tokens.toLocaleString() },
    ]} />} />
  </div>;
}
