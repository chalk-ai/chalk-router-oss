"use client";

import { useEffect, useMemo, useState } from "react";
import { ConfirmationModal } from "@/components/ConfirmationModal";
import { inputStyle, Modal, PageHeader } from "@/components/router/RouterPrimitives";
import { formatUtcDateTime } from "@/lib/format";

interface UsageBudget {
  id: string;
  scope: "api_key" | "usage_pool";
  scope_id: string;
  token_limit: number;
  period: "daily" | "weekly" | "monthly";
  enabled: boolean;
  created_at: string;
  current_usage: number | null;
  next_reset_at: string;
}
interface ApiKey { key_hash: string; key_prefix: string; description: string; }
interface UsagePool { id: string; name: string; }

export default function UsageBudgetsPage({ embedded = false }: { embedded?: boolean }) {
  const [budgets, setBudgets] = useState<UsageBudget[]>([]);
  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [pools, setPools] = useState<UsagePool[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [scope, setScope] = useState<"api_key" | "usage_pool">("usage_pool");
  const [scopeId, setScopeId] = useState("");
  const [amount, setAmount] = useState("");
  const [enabled, setEnabled] = useState(true);
  const [period, setPeriod] = useState<"daily" | "weekly" | "monthly">("daily");
  const [busy, setBusy] = useState<string | null>(null);
  const [editing, setEditing] = useState<UsageBudget | null>(null);
  const [pendingDelete, setPendingDelete] = useState<UsageBudget | null>(null);

  const names = useMemo(() => ({
    api_key: new Map(keys.map((key) => [key.key_hash, key.description || key.key_prefix])),
    usage_pool: new Map(pools.map((pool) => [pool.id, pool.name])),
  }), [keys, pools]);

  async function load() {
    setLoading(true);
    try {
      const filter = new URLSearchParams(window.location.search);
      const scopeFilter = filter.get("scope");
      const scopeIdFilter = filter.get("scope_id");
      const budgetParams = new URLSearchParams();
      if (scopeFilter) budgetParams.set("scope", scopeFilter);
      if (scopeIdFilter) budgetParams.set("scope_id", scopeIdFilter);
      const [budgetResponse, keyResponse, poolResponse] = await Promise.all([
        fetch(`/api/admin/usage-budgets${budgetParams.size ? `?${budgetParams}` : ""}`), fetch("/api/admin/keys?limit=1000"), fetch("/api/admin/usage-pools?limit=1000"),
      ]);
      if (!budgetResponse.ok || !keyResponse.ok || !poolResponse.ok) throw new Error("Unable to load usage budgets");
      const [budgetJson, keyJson, poolJson] = await Promise.all([budgetResponse.json(), keyResponse.json(), poolResponse.json()]);
      setBudgets(budgetJson.data ?? []); setKeys(keyJson.data ?? []); setPools(poolJson.data ?? []);
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setLoading(false); }
  }
  useEffect(() => { void load(); }, []);

  async function create(event: React.FormEvent) {
    event.preventDefault(); setError(null); setBusy("create");
    try {
      const response = await fetch("/api/admin/usage-budgets", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ scope, scope_id: scopeId, token_limit: Number(amount), period, enabled }) });
      if (!response.ok) throw new Error(await response.text());
      setCreating(false); setScopeId(""); setAmount(""); await load();
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(null); }
  }
  async function saveEdit(event: React.FormEvent) {
    event.preventDefault(); if (!editing) return; setError(null); setBusy(editing.id);
    try {
      const response = await fetch(`/api/admin/usage-budgets/${encodeURIComponent(editing.id)}`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token_limit: editing.token_limit, enabled: editing.enabled }) });
      if (!response.ok) throw new Error(await response.text()); setEditing(null); await load();
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(null); }
  }
  async function remove(budget: UsageBudget) {
    setBusy(budget.id); setError(null);
    try {
      const response = await fetch(`/api/admin/usage-budgets/${encodeURIComponent(budget.id)}`, { method: "DELETE" });
      if (!response.ok) throw new Error(await response.text()); setPendingDelete(null); await load();
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(null); }
  }
  const options = scope === "api_key" ? keys.map((key) => ({ id: key.key_hash, label: key.description || key.key_prefix })) : pools.map((pool) => ({ id: pool.id, label: pool.name }));

  return <div>
    <div className="mb-6"><PageHeader title="Usage budgets" description="UTC calendar token allowances for API keys and usage pools." action={<button onClick={() => setCreating(true)} className="btn btn-primary btn-md">Create usage budget</button>} /></div>
    {error && <div className="mb-4 rounded-lg px-4 py-3 text-sm" style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}>{error}</div>}
    <Modal open={creating} onClose={() => setCreating(false)} title="Create usage budget"><form onSubmit={create}><div className="grid gap-3 sm:grid-cols-2"><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Applies to<select value={scope} onChange={(event) => { setScope(event.target.value as typeof scope); setScopeId(""); }} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle}><option value="usage_pool">Usage pool</option><option value="api_key">API token</option></select></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Target<select required value={scopeId} onChange={(event) => setScopeId(event.target.value)} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle}><option value="">Choose {scope === "api_key" ? "API token" : "usage pool"}…</option>{options.map((option) => <option key={option.id} value={option.id}>{option.label}</option>)}</select></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Period<select value={period} onChange={(event) => setPeriod(event.target.value as typeof period)} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle}><option value="daily">Daily</option><option value="weekly">Weekly</option><option value="monthly">Monthly</option></select></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Token limit<input required min="1" type="number" value={amount} onChange={(event) => setAmount(event.target.value)} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle} /></label><label className="flex items-end gap-2 pb-2 text-xs" style={{ color: "var(--color-cds-ink-medium)" }}><input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} /> Enabled</label></div><div className="mt-4 flex gap-2"><button disabled={busy === "create"} className="btn btn-primary btn-md">{busy === "create" ? "Creating…" : "Create usage budget"}</button><button type="button" onClick={() => setCreating(false)} className="rounded px-4 py-2 text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>Cancel</button></div></form></Modal>
    {loading ? <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>Loading…</p> : budgets.length === 0 ? <div className="rounded-lg px-6 py-12 text-center" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink-subtle)" }}>No token budgets yet.</div> : <div className="overflow-hidden rounded-lg" style={{ border: "1px solid var(--color-cds-border)" }}><table className="w-full text-sm"><thead><tr style={{ background: "var(--color-cds-raised)", color: "var(--color-cds-ink-secondary)" }}><th className="px-5 py-3 text-left font-medium">Applies to</th><th className="px-5 py-3 text-left font-medium">Target</th><th className="px-5 py-3 text-left font-medium">Period</th><th className="px-5 py-3 text-left font-medium">Token limit</th><th className="px-5 py-3 text-left font-medium">Current usage</th><th className="px-5 py-3 text-left font-medium">Next UTC reset</th><th className="px-5 py-3 text-left font-medium">Status</th><th className="px-5 py-3 text-right font-medium">Actions</th></tr></thead><tbody>{budgets.map((budget, index) => <tr key={budget.id} style={{ background: index % 2 ? "var(--color-cds-raised)" : "var(--color-cds-elevated)", borderTop: "1px solid var(--color-cds-border)" }}><td className="px-5 py-3">{budget.scope === "api_key" ? "API token" : "Usage pool"}</td><td className="px-5 py-3 text-white">{names[budget.scope].get(budget.scope_id) ?? budget.scope_id}</td><td className="px-5 py-3 capitalize">{budget.period}</td><td className="px-5 py-3 font-mono">{budget.token_limit.toLocaleString()}</td><td className="px-5 py-3 font-mono">{budget.current_usage == null ? <span className="font-sans" style={{ color: "var(--color-cds-ink-subtle)" }}>Unavailable</span> : budget.current_usage.toLocaleString()}</td><td className="px-5 py-3 text-xs" style={{ color: "var(--color-cds-ink-medium)" }}>{formatUtcDateTime(budget.next_reset_at)}</td><td className="px-5 py-3"><span className="rounded px-2 py-0.5 text-xs" style={budget.enabled ? { background: "var(--color-cds-status-green-fill)", color: "var(--color-cds-status-green-text)" } : { background: "var(--color-cds-status-neutral-fill)", color: "var(--color-cds-ink-secondary)" }}>{budget.enabled ? "Enabled" : "Disabled"}</span></td><td className="px-5 py-3 text-right"><button onClick={() => setEditing({ ...budget })} className="mr-2 text-xs" style={{ color: "var(--color-cds-status-blue-text)" }}>Edit</button><button onClick={() => setPendingDelete(budget)} className="text-xs" style={{ color: "var(--color-cds-status-red-text)" }}>Delete</button></td></tr>)}</tbody></table></div>}
    {editing && <Modal open onClose={() => setEditing(null)} title={`Edit ${editing.period} usage budget`} size="sm"><form onSubmit={saveEdit}><label className="block text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Token limit<input min="1" type="number" value={editing.token_limit} onChange={(event) => setEditing({ ...editing, token_limit: Number(event.target.value) })} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle} /></label><label className="mt-3 flex gap-2 text-xs" style={{ color: "var(--color-cds-ink-medium)" }}><input type="checkbox" checked={editing.enabled} onChange={(event) => setEditing({ ...editing, enabled: event.target.checked })} /> Enabled</label><div className="mt-5 flex justify-end gap-2"><button type="button" onClick={() => setEditing(null)} className="px-3 py-2 text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>Cancel</button><button disabled={busy === editing.id} className="btn btn-primary btn-md">Save</button></div></form></Modal>}
    <ConfirmationModal open={pendingDelete !== null} title="Delete usage budget?" description="This removes the selected calendar-period enforcement definition immediately." confirmLabel="Delete usage budget" confirming={pendingDelete ? busy === pendingDelete.id : false} onCancel={() => setPendingDelete(null)} onConfirm={() => pendingDelete ? remove(pendingDelete) : Promise.resolve()} />
  </div>;
}
