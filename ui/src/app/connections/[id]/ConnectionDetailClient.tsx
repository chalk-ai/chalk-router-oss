"use client";

import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { UpstreamModelSelect } from "@/components/routes/UpstreamModelSelect";
import { DetailTabs } from "@/components/DetailTabs";
import { DetailRows, EntityHeader, ErrorBanner, inputStyle, SkeletonRows, StatusBadge } from "@/components/router/RouterPrimitives";
import { effectiveTarget, type RouteConfiguration } from "@/lib/routeGrid";
import { fetchAllAdminPages } from "@/lib/adminPagination";

type Exposure = "dynamic" | "unlisted" | "explicit" | "unspecified";
type Connection = { id: string; name: string; provider_kind: string; prefix: string | null; base_url: string | null; exposure: Exposure; routing_enabled: boolean; declared_capabilities: Record<string, unknown> | null; state: { configured: boolean; unreadable: boolean; healthy: boolean | null } };
type Route = RouteConfiguration & { id: string; public_model_id: string; upstream_model_id: string; connection_id: string; published: boolean };
type Model = { upstream_model_id: string; public_model_id: string };
type TestResult = { ok: boolean; error: string | null; discovered_models: number | null; latency_ms: number | null };
type RouteDraft = { upstreamModelId: string; publicModelId: string };

function initialForm(connection: Connection) {
  return {
    name: connection.name,
    prefix: connection.prefix ?? "",
    baseUrl: connection.base_url ?? "",
    exposure: connection.exposure === "unspecified" ? "dynamic" as Exposure : connection.exposure,
    routingEnabled: connection.routing_enabled,
    apiKey: "",
    declaredCapabilities: connection.declared_capabilities ? JSON.stringify(connection.declared_capabilities, null, 2) : "",
  };
}

async function errorText(response: Response): Promise<string> {
  try {
    const body = await response.json();
    return body?.error?.message ?? `${response.status} ${response.statusText}`;
  } catch { return `${response.status} ${response.statusText}`; }
}

export function ConnectionDetailClient({ connectionId }: { connectionId: string }) {
  const [connection, setConnection] = useState<Connection | null>(null);
  const [routes, setRoutes] = useState<Route[]>([]);
  const [models, setModels] = useState<Model[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);
  const [routeDraft, setRouteDraft] = useState<RouteDraft | null>(null);
  const [creatingRoute, setCreatingRoute] = useState(false);
  const [form, setForm] = useState({ name: "", prefix: "", baseUrl: "", exposure: "dynamic" as Exposure, routingEnabled: true, apiKey: "", declaredCapabilities: "" });

  const load = useCallback(async () => {
    try {
      setError(null);
      const [connections, allRoutes, modelResponse] = await Promise.all([
        fetchAllAdminPages<Connection>("/api/admin/providers"),
        fetchAllAdminPages<Route>("/api/admin/model-routes"),
        fetch(`/api/admin/providers/${encodeURIComponent(connectionId)}/models`),
      ]);
      const found = connections.find((item) => item.id === connectionId) ?? null;
      if (!found) throw new Error("Connection not found");
      if (!modelResponse.ok) throw new Error(await errorText(modelResponse));
      const body: { data?: Model[] } = await modelResponse.json();
      setConnection(found);
      setForm(initialForm(found));
      setRoutes(allRoutes.filter((route) => effectiveTarget(route).connection_id === connectionId));
      setModels(body.data ?? []);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  }, [connectionId]);

  useEffect(() => { void load(); }, [load]);

  async function save(event: React.FormEvent) {
    event.preventDefault();
    if (!connection || saving) return;
    if (!form.name.trim()) { setError("A connection name is required."); return; }
    if (form.exposure !== "explicit" && !form.prefix.trim()) { setError("A prefix is required unless exposure is Explicit."); return; }
    let declaredCapabilities: Record<string, unknown> | null = null;
    if (form.declaredCapabilities.trim()) {
      try {
        const parsed: unknown = JSON.parse(form.declaredCapabilities);
        if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") throw new Error("Capabilities must be a JSON object.");
        declaredCapabilities = parsed as Record<string, unknown>;
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : "Capabilities must be valid JSON.");
        return;
      }
    }
    setSaving(true); setError(null);
    try {
      const payload: Record<string, unknown> = { name: form.name.trim(), exposure: form.exposure, routing_enabled: form.routingEnabled, declared_capabilities: declaredCapabilities };
      if (form.prefix.trim()) payload.prefix = form.prefix.trim(); else payload.clear_prefix = true;
      if (form.baseUrl.trim()) payload.base_url = form.baseUrl.trim(); else payload.clear_base_url = true;
      if (form.apiKey.trim()) payload.api_key = form.apiKey.trim();
      const response = await fetch(`/api/admin/providers/${encodeURIComponent(connection.id)}`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload) });
      if (!response.ok) throw new Error(await errorText(response));
      const updated: Connection = await response.json();
      setConnection(updated); setForm(initialForm(updated));
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSaving(false); }
  }

  async function testConnection() {
    if (!connection || testing) return;
    setTesting(true); setTestResult(null);
    try {
      const response = await fetch("/api/admin/providers/test", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ connection_id: connection.id }) });
      if (!response.ok) throw new Error(await errorText(response));
      setTestResult(await response.json());
    } catch (cause) {
      setTestResult({ ok: false, error: cause instanceof Error ? cause.message : String(cause), discovered_models: null, latency_ms: null });
    } finally { setTesting(false); }
  }

  function openRouteCreator(upstreamModelId = "", publicModelId?: string) {
    setRouteDraft({ upstreamModelId, publicModelId: publicModelId ?? (upstreamModelId ? `chalk/${upstreamModelId}` : "chalk/") });
  }

  async function createRoute(event: React.FormEvent) {
    event.preventDefault();
    if (!connection || !routeDraft || creatingRoute) return;
    if (!routeDraft.upstreamModelId.trim() || !routeDraft.publicModelId.trim() || routeDraft.publicModelId.trim() === "chalk/") { setError("Both the upstream model and a chalk/* public alias are required."); return; }
    setCreatingRoute(true); setError(null);
    try {
      const response = await fetch("/api/admin/model-routes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ connection_id: connection.id, upstream_model_id: routeDraft.upstreamModelId.trim(), public_model_id: routeDraft.publicModelId.trim(), published: true }) });
      if (!response.ok) throw new Error(await errorText(response));
      setRouteDraft(null); await load();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setCreatingRoute(false); }
  }

  if (!connection && !error) return <SkeletonRows rows={5} />;
  if (!connection) return <div><Link href="/routing?v=connections" className="text-sm" style={{ color: "var(--color-cds-status-green-text)" }}>← Connections</Link><p className="mt-5 rounded px-4 py-3 text-sm" style={{ background: "var(--color-cds-status-red-fill)", color: "var(--color-cds-status-red-text)" }}>{error}</p></div>;

  const card = { background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" };
  const modelRoutes = (model: Model) => routes.filter((route) => effectiveTarget(route).upstream_model_id === model.upstream_model_id);
  const routeCreator = routeDraft && <form onSubmit={createRoute} className="mt-5 rounded-lg p-5" style={{ background: "var(--color-cds-elevated)", border: "1px solid var(--color-cds-status-green-border)" }}>
    <div className="mb-4 flex items-center justify-between"><div><h2 className="text-cds-md font-medium text-cds-ink-strong">Create model route</h2><p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>This creates a stable <code>chalk/*</code> alias for this connection.</p></div><button type="button" onClick={() => setRouteDraft(null)} className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Cancel</button></div>
    <div className="grid gap-4 sm:grid-cols-2"><UpstreamModelSelect label="Upstream model" connectionId={connection.id} value={routeDraft.upstreamModelId} onChange={upstreamModelId => setRouteDraft({ ...routeDraft, upstreamModelId })} /><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Public alias<input value={routeDraft.publicModelId} onChange={(event) => setRouteDraft({ ...routeDraft, publicModelId: event.target.value })} placeholder="chalk/my-model" className="mt-1 w-full rounded-md px-3 py-2 font-mono text-sm" style={inputStyle} /></label></div>
    <div className="mt-4 flex justify-end"><button disabled={creatingRoute} className="btn btn-primary btn-md">{creatingRoute ? "Creating…" : "Create route"}</button></div>
  </form>;

  return <div>
    <EntityHeader backHref="/routing?v=connections" backLabel="Back to connections" label="Connection" name={connection.name} metadata={<StatusBadge tone={connection.state.healthy === false ? "red" : connection.state.configured ? "green" : "yellow"}>{connection.state.healthy === false ? "Unhealthy" : connection.state.configured ? "Configured" : "Needs credential"}</StatusBadge>} />
    {error && <div className="mt-5"><ErrorBanner>{error}</ErrorBanner></div>}
    <DetailTabs tabs={[
      { id: "models", label: "Models", content: <section className="rounded-lg p-5" style={card}>
        <div className="mb-4 flex items-start justify-between gap-4"><p className="text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>Models discovered from this connection. Create a route when a model needs a stable public <code>chalk/*</code> alias.</p><button onClick={() => openRouteCreator()} className="btn btn-primary shrink-0">New route</button></div>
        {models.length ? <div className="overflow-hidden rounded" style={{ border: "1px solid var(--color-cds-border)" }}><table className="w-full text-sm"><thead><tr style={{ background: "var(--color-cds-elevated)", color: "var(--color-cds-ink-secondary)" }}><th className="px-4 py-3 text-left font-medium">Upstream model</th><th className="px-4 py-3 text-left font-medium">Connection model id</th><th className="px-4 py-3 text-left font-medium">Explicit routes</th><th className="px-4 py-3 text-right font-medium">Action</th></tr></thead><tbody>{models.map((model, index) => { const linkedRoutes = modelRoutes(model); return <tr key={model.upstream_model_id} style={{ background: index % 2 ? "var(--color-cds-raised)" : "var(--color-cds-elevated)", borderTop: "1px solid var(--color-cds-border)" }}><td className="px-4 py-3 font-mono text-xs text-white">{model.upstream_model_id}</td><td className="px-4 py-3 font-mono text-xs" style={{ color: "var(--color-cds-status-green-text)" }}>{model.public_model_id || "—"}</td><td className="px-4 py-3">{linkedRoutes.length ? <div className="flex flex-wrap gap-1.5">{linkedRoutes.map((route) => <Link key={route.id} href={`/routes/${encodeURIComponent(route.id)}`} className="rounded px-1.5 py-0.5 font-mono text-xs" style={{ background: "var(--color-cds-status-green-fill)", color: "var(--color-cds-status-green-text)" }}>{route.public_model_id}</Link>)}</div> : <span className="text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>No explicit route</span>}</td><td className="px-4 py-3 text-right"><button onClick={() => openRouteCreator(model.upstream_model_id)} className="btn btn-secondary btn-xs">{linkedRoutes.length ? "Add route" : "Create route"}</button></td></tr>; })}</tbody></table></div> : <p className="text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>No models have been discovered for this connection. Test the connection, or create a route with an upstream model id.</p>}
        {routeCreator}
      </section> },
      { id: "routes", label: "Routes", content: <section className="rounded-lg p-5" style={card}><div className="mb-4 flex items-center justify-between"><p className="text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>Routes whose default target uses this connection.</p><button onClick={() => openRouteCreator()} className="btn btn-primary">New route</button></div>{routes.length ? <div className="space-y-2">{routes.map((route) => <Link key={route.id} href={`/routes/${encodeURIComponent(route.id)}`} className="flex items-center justify-between rounded px-3 py-2 hover:bg-cds-hover"><span className="font-mono text-sm" style={{ color: "var(--color-cds-status-green-text)" }}>{route.public_model_id}</span><span className="font-mono text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>{effectiveTarget(route).upstream_model_id}</span></Link>)}</div> : <p className="text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>No explicit routes use this connection yet.</p>}{routeCreator}</section> },
      { id: "settings", label: "Settings", content: <form onSubmit={save} className="max-w-2xl rounded-lg p-5" style={card}>
        <div className="mb-5 flex items-center justify-between"><div><h2 className="text-cds-md font-medium text-cds-ink-strong">Connection settings</h2><p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Provider kind is immutable; credentials are stored server-side.</p></div><button type="button" onClick={testConnection} disabled={testing} className="btn btn-secondary">{testing ? "Testing…" : "Test connection"}</button></div>
        {testResult && <div className="mb-4 rounded px-3 py-2 text-xs" style={{ background: testResult.ok ? "var(--color-cds-status-green-fill)" : "var(--color-cds-status-red-fill)", color: testResult.ok ? "var(--color-cds-status-green-text)" : "var(--color-cds-status-red-text)", border: `1px solid ${testResult.ok ? "var(--color-cds-status-green-border)" : "var(--color-cds-status-red-border)"}` }}>{testResult.ok ? `Connection OK${testResult.latency_ms != null ? ` · ${testResult.latency_ms}ms` : ""}${testResult.discovered_models != null ? ` · ${testResult.discovered_models} models discovered` : ""}` : `Test failed: ${testResult.error ?? "unknown error"}`}</div>}
        <div className="grid gap-4 sm:grid-cols-2"><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Name<input required value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} className="mt-1 w-full rounded px-3 py-2 text-sm" style={inputStyle} /></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Provider kind<input disabled value={connection.provider_kind} className="mt-1 w-full rounded px-3 py-2 text-sm opacity-60" style={inputStyle} /></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Base URL <span style={{ color: "var(--color-cds-ink-muted)" }}>(blank uses provider default)</span><input value={form.baseUrl} onChange={(event) => setForm({ ...form, baseUrl: event.target.value })} placeholder="https://…" className="mt-1 w-full rounded px-3 py-2 text-sm" style={inputStyle} /></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Exposure<select value={form.exposure} onChange={(event) => setForm({ ...form, exposure: event.target.value as Exposure })} className="mt-1 w-full rounded px-3 py-2 text-sm" style={inputStyle}><option value="dynamic">Dynamic (prefix routing + listed)</option><option value="unlisted">Unlisted (prefix routing, hidden)</option><option value="explicit">Explicit (routes only)</option></select></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Model prefix <span style={{ color: "var(--color-cds-ink-muted)" }}>({form.exposure === "explicit" ? "optional" : "required"})</span><input value={form.prefix} onChange={(event) => setForm({ ...form, prefix: event.target.value })} placeholder="e.g. anthropic/" className="mt-1 w-full rounded px-3 py-2 text-sm" style={inputStyle} /></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>{connection.provider_kind === "vertex" ? "Replace service-account key" : "Replace credential"} <span style={{ color: "var(--color-cds-ink-muted)" }}>(blank keeps current)</span><textarea rows={connection.provider_kind === "vertex" ? 5 : 1} value={form.apiKey} onChange={(event) => setForm({ ...form, apiKey: event.target.value })} className="mt-1 w-full rounded px-3 py-2 text-sm" style={inputStyle} /></label></div>
        <label className="mt-4 flex items-center gap-2 text-sm" style={{ color: "var(--color-cds-ink-medium)" }}><input type="checkbox" checked={form.routingEnabled} onChange={(event) => setForm({ ...form, routingEnabled: event.target.checked })} />Enable routing</label>
        <details className="mt-5 rounded-lg p-4" style={{ background: "var(--color-cds-elevated)", border: "1px solid var(--color-cds-border-strong)" }}><summary className="cursor-pointer text-sm font-medium" style={{ color: "var(--color-cds-ink-medium)" }}>Advanced capability declaration</summary><p className="mt-2 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>Leave blank to use built-in and discovered capabilities. A declaration overrides them for this connection.</p><textarea value={form.declaredCapabilities} onChange={(event) => setForm({ ...form, declaredCapabilities: event.target.value })} rows={9} spellCheck={false} placeholder={'{\n  "schema_version": 1\n}'} className="mt-3 w-full rounded px-3 py-2 font-mono text-xs" style={inputStyle} /></details>
        <div className="mt-5 flex justify-end"><button disabled={saving} className="btn btn-primary btn-md">{saving ? "Saving…" : "Save changes"}</button></div>
      </form> },
    ]} sidebar={<DetailRows rows={[
      { label: "Connection ID", value: <span className="font-mono">{connection.id}</span> },
      { label: "Provider", value: connection.provider_kind },
      { label: "Base URL", value: connection.base_url ?? "Provider default" },
      { label: "Model prefix", value: connection.prefix ?? "None" },
      { label: "Exposure", value: connection.exposure },
      { label: "Routing", value: connection.routing_enabled ? "Enabled" : "Disabled" },
      { label: "Discovered models", value: models.length.toLocaleString() },
      { label: "Explicit routes", value: routes.length.toLocaleString() },
    ]} />} />
  </div>;
}
