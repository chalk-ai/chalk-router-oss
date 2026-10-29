"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { DetailTabs } from "@/components/DetailTabs";
import { DetailRows, EntityHeader, ErrorBanner, SkeletonRows, StatusBadge } from "@/components/router/RouterPrimitives";
import { useJudgeValidation } from "@/components/routes/useJudgeValidation";
import { RouteConfigurationEditor } from "@/components/routes/RouteConfigurationEditor";
import { effectiveTarget, routeConfigurationPayload, routeGridError, type RouteConfiguration } from "@/lib/routeGrid";
import { fetchAllAdminPages } from "@/lib/adminPagination";
import { routeAccessForKey, type KeyAccessJson } from "@/lib/keyAccess";

type Route = RouteConfiguration & { id: string; public_model_id: string; upstream_model_id: string; connection_id: string; published: boolean };
type Connection = { id: string; name: string };
type Key = { key_hash: string; key_prefix: string; description: string; permission: KeyAccessJson };
type AccessResult = { key: Key; allowed: boolean; reason: string };

async function errorText(response: Response): Promise<string> {
  try { const body = await response.json(); return body?.error?.message ?? `${response.status} ${response.statusText}`; }
  catch { return `${response.status} ${response.statusText}`; }
}

export function RouteDetailClient({ routeId }: { routeId: string }) {
  const [route, setRoute] = useState<Route | null>(null);
  const [connections, setConnections] = useState<Connection[]>([]);
  const [keys, setKeys] = useState<Key[]>([]);
  const [savedRoute, setSavedRoute] = useState<Route | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => { void (async () => {
    try {
      const [routes, allConnections, allKeys] = await Promise.all([
        fetchAllAdminPages<Route>("/api/admin/model-routes"),
        fetchAllAdminPages<Connection>("/api/admin/providers"),
        fetchAllAdminPages<Key>("/api/admin/keys"),
      ]);
      const found = routes.find((item) => item.id === routeId) ?? null;
      if (!found) throw new Error("Route not found");
      setRoute(found); setSavedRoute(found); setConnections(allConnections); setKeys(allKeys);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  })(); }, [routeId]);

  const judgeValidation = useJudgeValidation(route?.routing?.judge?.backend);

  async function save(event: React.FormEvent) {
    event.preventDefault(); if (!route || saving || judgeValidation.checking || judgeValidation.error) return;
    if (route.routing) { const issue = routeGridError(route.routing); if (issue) { setError(issue); return; } }
    setSaving(true); setError(null); setSaved(false);
    try {
      const response = await fetch(`/api/admin/model-routes/${encodeURIComponent(route.id)}`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ ...routeConfigurationPayload(route), published: route.published }) });
      if (!response.ok) throw new Error(await errorText(response));
      const updated: Route = await response.json();
      setRoute(updated); setSavedRoute(updated); setSaved(true);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSaving(false); }
  }

  if (!route && !error) return <SkeletonRows rows={5} />;
  if (!route) return <div><Link href="/routing?v=routes" className="text-sm" style={{ color: "var(--color-cds-status-green-text)" }}>← Routes</Link><p className="mt-5 rounded px-4 py-3 text-sm" style={{ background: "var(--color-cds-status-red-fill)", color: "var(--color-cds-status-red-text)" }}>{error}</p></div>;

  const card = { background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" };
  const target = effectiveTarget(savedRoute ?? route);
  const access: AccessResult[] = keys.map(key => ({ key, ...routeAccessForKey(key.permission, route.id) }));
  const allowed = access.filter((result) => result.allowed);
  const denied = access.filter((result) => !result.allowed);

  return <div>
    <EntityHeader backHref="/routing?v=routes" backLabel="Back to routes" label="Route" name={route.public_model_id} metadata={<StatusBadge tone={route.published ? "green" : "neutral"}>{route.published ? "Published" : "Hidden"}</StatusBadge>} />
    {error && <div className="mt-5"><ErrorBanner>{error}</ErrorBanner></div>}
    <DetailTabs tabs={[
      { id: "keys", label: "Key access", content: <section className="rounded-lg p-5" style={card}><div className="mb-4"><h2 className="text-cds-md font-medium text-cds-ink-strong">Reachable API keys</h2><p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Keys with every model, or granted this route.</p></div>{allowed.length ? <div className="space-y-2">{allowed.map(({ key, reason }) => <Link key={key.key_hash} href={`/keys/${encodeURIComponent(key.key_hash)}`} className="flex items-center justify-between rounded-md px-4 py-3 hover:bg-cds-hover" style={{ background: "var(--color-cds-elevated)", border: "1px solid var(--color-cds-border-strong)" }}><span className="text-sm text-white">{key.description || key.key_prefix}</span><span className="text-xs" style={{ color: "var(--color-cds-status-green-text)" }}>Reaches · {reason}</span></Link>)}</div> : <p className="text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>No API keys currently reach this route.</p>}{denied.length > 0 && <details className="mt-5"><summary className="cursor-pointer text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>{denied.length} key{denied.length === 1 ? "" : "s"} without this route</summary><div className="mt-2 space-y-1.5">{denied.map(({ key, reason }) => <div key={key.key_hash} className="flex items-center justify-between rounded px-3 py-2 text-xs" style={{ background: "var(--color-cds-elevated)", color: "var(--color-cds-ink-secondary)" }}><span>{key.description || key.key_prefix}</span><span>{reason}</span></div>)}</div></details>}</section> },
      { id: "settings", label: "Routing settings", content: <form onSubmit={save} className="space-y-5 rounded-lg p-5" style={card}>
        <p className="text-xs text-cds-ink-secondary">The public alias is permanent. Configure its default target, judge outcomes and fallback chains here.</p>
        <fieldset disabled={saving} className="space-y-5">
          <RouteConfigurationEditor judgeValidation={judgeValidation} value={route} connections={connections} onChange={configuration => { setSaved(false); setRoute({ ...route, ...configuration }); }} />
          <label className="flex gap-2 text-sm text-white"><input type="checkbox" checked={route.published} onChange={event => { setSaved(false); setRoute({ ...route, published: event.target.checked }); }} />Published in <code>/v1/models</code></label>
          <button disabled={saving || judgeValidation.checking || !!judgeValidation.error || (route.routing ? !!routeGridError(route.routing) : !route.connection_id.trim() || !route.upstream_model_id.trim())} className="rounded bg-emerald-700 px-4 py-2 text-sm font-medium text-white disabled:opacity-50">{saving ? "Saving…" : "Save changes"}</button>
        </fieldset>
        {saved && <p role="status" className="text-sm text-emerald-300">Route saved.</p>}
      </form> },
    ]} sidebar={<DetailRows rows={[
      { label: "Route ID", value: <span className="font-mono">{route.id}</span> },
      { label: "Public model", value: <span className="font-mono">{route.public_model_id}</span> },
      { label: "Upstream model", value: <span className="font-mono">{target.upstream_model_id}</span> },
      { label: "Connection", value: connections.find((connection) => connection.id === target.connection_id)?.name ?? target.connection_id },
      { label: "API keys with access", value: allowed.length.toLocaleString() },
      { label: "Routing", value: savedRoute?.routing ? "Custom" : "Simple" },
    ]} />} />
  </div>;
}
