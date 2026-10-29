"use client";

import { useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { EntityHeader, ErrorBanner, inputStyle, Modal, SkeletonRows, StatusBadge } from "@/components/router/RouterPrimitives";
import { fetchAllAdminPages } from "@/lib/adminPagination";
import {
  ConnectionModelPicker,
  KeyQuotaEditor,
  rateLimitIdentityKey,
  validateQuota,
  type KeyBudgetDraft,
  type KeyRateLimitDraft,
  type RateLimitType,
} from "@/app/tokens/KeyQuotaEditor";

interface UsagePool { id: string; name: string; created_at: string; }
interface Connection { id: string; name: string; provider_kind: string; }
interface ApiKey { key_hash: string; key_prefix: string; description: string; usage_pool_id: string | null; usage_budgets: unknown[]; rate_limits: unknown[]; }
interface UsageBudget { id: string; scope_id: string; token_limit: number; period: "daily" | "weekly" | "monthly"; enabled: boolean; current_usage: number | null; next_reset_at: string; }
interface RateLimitPolicy { id: string; limit_type: "tokens_per_minute" | "requests_per_minute" | "concurrent_requests"; limit_value: number; burst_value: number | null; connection_id: string | null; models: string[]; scope: "api_key" | "usage_pool"; scope_id: string; enabled: boolean; }
interface RateLimitEditState {
  enabled: boolean;
  draft: KeyRateLimitDraft;
}

function periodLabel(period: UsageBudget["period"]): string {
  return period === "daily" ? "day" : period === "weekly" ? "week" : "month";
}

function rateTypeLabel(type: RateLimitPolicy["limit_type"]): string {
  if (type === "requests_per_minute") return "Requests / min";
  if (type === "tokens_per_minute") return "Tokens / min";
  return "Concurrent requests";
}

export function UsagePoolDetailClient({ poolId, setup }: { poolId: string; setup?: string }) {
  const [pool, setPool] = useState<UsagePool | null>(null);
  const [budgets, setBudgets] = useState<UsageBudget[]>([]);
  const [rateLimits, setRateLimits] = useState<RateLimitPolicy[]>([]);
  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [connections, setConnections] = useState<Connection[]>([]);
  const [draftBudgets, setDraftBudgets] = useState<KeyBudgetDraft[]>([]);
  const [draftRateLimits, setDraftRateLimits] = useState<KeyRateLimitDraft[]>([]);
  const [editingName, setEditingName] = useState<string | null>(null);
  const [editingBudget, setEditingBudget] = useState<UsageBudget | null>(null);
  const [editingRateLimit, setEditingRateLimit] = useState<RateLimitPolicy | null>(null);
  const [rateLimitEdit, setRateLimitEdit] = useState<RateLimitEditState | null>(null);
  const [addingLimits, setAddingLimits] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const connectionNames = useMemo(() => new Map(connections.map((connection) => [connection.id, connection.name])), [connections]);

  async function load() {
    setLoading(true);
    try {
      const [poolResponse, budgetResponse, allRateLimits, allKeys, allConnections] = await Promise.all([
        fetch(`/api/admin/usage-pools/${encodeURIComponent(poolId)}`),
        fetch(`/api/admin/usage-budgets?scope=usage_pool&scope_id=${encodeURIComponent(poolId)}`),
        fetchAllAdminPages<RateLimitPolicy>("/api/admin/rate-limits"),
        fetchAllAdminPages<ApiKey>("/api/admin/keys"),
        fetchAllAdminPages<Connection>("/api/admin/providers"),
      ]);
      if (!poolResponse.ok) throw new Error(poolResponse.status === 404 ? "Usage pool not found" : await poolResponse.text());
      if (!budgetResponse.ok) throw new Error(await budgetResponse.text());
      const [poolJson, budgetJson] = await Promise.all([poolResponse.json(), budgetResponse.json()]);
      setPool(poolJson);
      setBudgets((budgetJson.data ?? []) as UsageBudget[]);
      setRateLimits(allRateLimits.filter((limit) => limit.scope === "usage_pool" && limit.scope_id === poolId));
      setKeys(allKeys.filter((key) => key.usage_pool_id === poolId));
      setConnections(allConnections);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void load(); }, [poolId]);

  async function saveName(event: React.FormEvent) {
    event.preventDefault();
    if (!editingName?.trim()) return;
    setBusy("name");
    setError(null);
    try {
      const response = await fetch(`/api/admin/usage-pools/${encodeURIComponent(poolId)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: editingName.trim() }),
      });
      if (!response.ok) throw new Error(await response.text());
      setPool(await response.json());
      setEditingName(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  function updatePayload(nextBudgets: UsageBudget[], nextRateLimits: RateLimitPolicy[]) {
    return {
      usage_budgets: nextBudgets.map((budget) => ({
        token_limit: budget.token_limit,
        period: budget.period,
        enabled: budget.enabled,
      })),
      rate_limits: nextRateLimits.map((limit) => ({
        connection_id: limit.connection_id ?? undefined,
        models: limit.models,
        enabled: limit.enabled,
        ...(limit.limit_type === "tokens_per_minute" ? { tokens_per_minute: limit.limit_value, token_burst: limit.burst_value ?? undefined } : {}),
        ...(limit.limit_type === "requests_per_minute" ? { requests_per_minute: limit.limit_value, request_burst: limit.burst_value ?? undefined } : {}),
        ...(limit.limit_type === "concurrent_requests" ? { concurrent_requests: limit.limit_value } : {}),
      })),
    };
  }

  async function savePolicies(nextBudgets: UsageBudget[], nextRateLimits: RateLimitPolicy[]) {
    const response = await fetch(`/api/admin/usage-pools/${encodeURIComponent(poolId)}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(updatePayload(nextBudgets, nextRateLimits)),
    });
    if (!response.ok) throw new Error(await response.text());
    await load();
  }

  async function addLimits(event: React.FormEvent) {
    event.preventDefault();
    if (!pool) return;
    if (draftBudgets.length === 0 && draftRateLimits.length === 0) {
      setError("Add at least one token budget or rate limit.");
      return;
    }
    const quotaError = validateQuota(draftBudgets, draftRateLimits);
    if (quotaError) {
      setError(quotaError);
      return;
    }
    const existingPeriods = new Set(budgets.map((budget) => budget.period));
    const duplicate = draftBudgets.find((budget) => existingPeriods.has(budget.period));
    if (duplicate) {
      setError(`This pool already has a ${duplicate.period} token budget. Edit it instead.`);
      return;
    }
    const existingRateLimits = new Set(rateLimits.map((limit) => rateLimitIdentityKey(limit.limit_type, limit.connection_id ?? "", limit.models)));
    if (draftRateLimits.some((limit) => existingRateLimits.has(rateLimitIdentityKey(limit.limitType, limit.connectionId, limit.models)))) {
      setError("This target already has this rate limit. Edit the existing rate limit instead.");
      return;
    }
    const nextBudgets = [...budgets, ...draftBudgets.map((budget, index) => ({
      id: `draft-budget-${index}`,
      scope_id: pool.id,
      token_limit: Number(budget.tokenLimit),
      period: budget.period,
      enabled: budget.enabled,
      current_usage: null,
      next_reset_at: "",
    }))];
    const nextRateLimits = [...rateLimits, ...draftRateLimits.map((limit, index) => ({
      id: `draft-rate-limit-${index}`,
      limit_type: limit.limitType,
      limit_value: Number(limit.limitValue),
      burst_value: limit.burstValue.trim() ? Number(limit.burstValue) : null,
      connection_id: limit.connectionId || null,
      models: limit.connectionId ? limit.models : [],
      scope: "usage_pool" as const,
      scope_id: pool.id,
      enabled: limit.enabled,
    }))];
    setBusy("add");
    setError(null);
    try {
      await savePolicies(nextBudgets, nextRateLimits);
      setDraftBudgets([]);
      setDraftRateLimits([]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  async function patchBudget(id: string, patch: { token_limit?: number; enabled?: boolean }) {
    setBusy(id);
    setError(null);
    try {
      await savePolicies(budgets.map((budget) => budget.id === id ? { ...budget, ...patch } : budget), rateLimits);
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(null); }
  }

  function beginRateLimitEdit(policy: RateLimitPolicy) {
    setError(null);
    setEditingRateLimit(policy);
    setRateLimitEdit({
      enabled: policy.enabled,
      draft: {
        id: 0,
        connectionId: policy.connection_id ?? "",
        modelAccess: policy.models.length > 0 ? "specific" : "all",
        models: policy.models,
        limitType: policy.limit_type,
        limitValue: String(policy.limit_value),
        burstValue: policy.burst_value == null ? "" : String(policy.burst_value),
        enabled: policy.enabled,
      },
    });
  }

  async function saveRateLimitEdit(event: React.FormEvent) {
    event.preventDefault();
    if (!editingRateLimit || !rateLimitEdit) return;
    const quotaError = validateQuota([], [rateLimitEdit.draft]);
    if (quotaError) {
      setError(quotaError);
      return;
    }
    setBusy(editingRateLimit.id);
    setError(null);
    try {
      const draft = rateLimitEdit.draft;
      await savePolicies(budgets, rateLimits.map((limit) => limit.id === editingRateLimit.id ? {
        ...limit,
        limit_type: draft.limitType,
        limit_value: Number(draft.limitValue),
        burst_value: draft.burstValue.trim() ? Number(draft.burstValue) : null,
        connection_id: draft.connectionId || null,
        models: draft.connectionId ? draft.models : [],
        enabled: rateLimitEdit.enabled,
      } : limit));
      setEditingRateLimit(null);
      setRateLimitEdit(null);
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(null); }
  }

  if (loading && !pool) return <SkeletonRows rows={5} />;
  if (!pool) return <div><Link href="/access?v=pools" className="text-sm" style={{ color: "var(--color-cds-status-green-text)" }}>← Usage pools</Link><div className="mt-6 rounded-lg px-4 py-3 text-sm" style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}>{error ?? "Usage pool not found"}</div></div>;

  return <div>
    <EntityHeader backHref="/access?v=pools" backLabel="Back to usage pools" label="Usage pool" name={pool.name} metadata={<StatusBadge tone="green">{keys.length} key{keys.length === 1 ? "" : "s"}</StatusBadge>} />
    <div className="mb-7 mt-5 flex flex-wrap gap-2"><button onClick={() => setEditingName(pool.name)} className="btn btn-secondary btn-md">Rename</button><button onClick={() => setAddingLimits(true)} className="btn btn-secondary btn-md">Add limits</button><Link href={`/access?v=keys&issue=1&usage_pool_id=${encodeURIComponent(pool.id)}`} className="rounded px-3 py-2 text-sm font-medium" style={{ background: "var(--color-cds-primary)", color: "white" }}>Create API key</Link></div>

    {setup === "created" && <div className="mb-5 rounded-lg px-4 py-3 text-sm" style={{ background: "var(--color-cds-status-green-fill)", border: "1px solid var(--color-cds-status-green-border)", color: "var(--color-cds-status-green-text)" }}>Usage pool created. Review its shared limits or create a token with this pool preselected.</div>}
    {setup === "partial" && <div className="mb-5 rounded-lg px-4 py-3 text-sm" style={{ background: "var(--color-cds-status-yellow-fill)", border: "1px solid var(--color-cds-status-yellow-border)", color: "var(--color-cds-status-yellow-text)" }}>The pool was created successfully, but at least one requested limit was not saved. Add the remaining shared limits below.</div>}
    {error && <div className="mb-5"><ErrorBanner>{error}</ErrorBanner></div>}

    <section className="mb-6 rounded-lg p-5" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" }}><div className="mb-4"><h2 className="section-title">Shared token budgets</h2><p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>UTC calendar budgets are shared by every API key in this pool.</p></div>
      {budgets.length === 0 ? <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>No shared token budgets.</p> : <div className="overflow-x-auto rounded-md border border-cds-border-strong"><table className="w-full text-sm"><thead><tr className="bg-cds-inset text-cds-ink-secondary"><th className="px-4 py-3 text-left">Period</th><th className="px-4 py-3 text-right">Limit</th><th className="px-4 py-3 text-right">Usage</th><th className="px-4 py-3 text-left">Status</th><th className="sticky right-0 bg-cds-inset px-4 py-3 text-right">Actions</th></tr></thead><tbody>{budgets.map((budget) => <tr key={budget.id} className="border-t border-cds-border-strong"><td className="px-4 py-3 capitalize">{budget.period}</td><td className="px-4 py-3 text-right font-mono">{budget.token_limit.toLocaleString()} / {periodLabel(budget.period)}</td><td className="px-4 py-3 text-right font-mono">{budget.current_usage?.toLocaleString() ?? "—"}</td><td className="px-4 py-3"><StatusBadge tone={budget.enabled ? "green" : "neutral"}>{budget.enabled ? "Enabled" : "Disabled"}</StatusBadge></td><td className="sticky right-0 bg-cds-raised px-4 py-3 text-right"><button onClick={() => setEditingBudget({ ...budget })} className="text-xs text-blue-300">Edit</button></td></tr>)}</tbody></table></div>}
    </section>

    <section className="mb-6 rounded-lg p-5" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" }}><div className="mb-4"><h2 className="section-title">Shared rate limits</h2><p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Connection and model selectors determine which requests consume each shared limit.</p></div>
      {rateLimits.length === 0 ? <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>No shared rate limits.</p> : <div className="overflow-x-auto rounded-md border border-cds-border-strong"><table className="w-full text-sm"><thead><tr className="bg-cds-inset text-cds-ink-secondary"><th className="px-4 py-3 text-left">Limit</th><th className="px-4 py-3 text-left">Scope</th><th className="px-4 py-3 text-left">Status</th><th className="sticky right-0 bg-cds-inset px-4 py-3 text-right">Actions</th></tr></thead><tbody>{rateLimits.map((limit) => <tr key={limit.id} className="border-t border-cds-border-strong"><td className="px-4 py-3">{rateTypeLabel(limit.limit_type)} · {limit.limit_value.toLocaleString()}{limit.burst_value != null ? ` · burst ${limit.burst_value.toLocaleString()}` : ""}</td><td className="px-4 py-3 text-xs text-cds-ink-secondary">{limit.connection_id ? `${connectionNames.get(limit.connection_id) ?? limit.connection_id} · ${limit.models.length ? limit.models.join(", ") : "All models"}` : "All connections · All models"}</td><td className="px-4 py-3"><StatusBadge tone={limit.enabled ? "green" : "neutral"}>{limit.enabled ? "Enabled" : "Disabled"}</StatusBadge></td><td className="sticky right-0 bg-cds-raised px-4 py-3 text-right"><button onClick={() => beginRateLimitEdit(limit)} className="text-xs text-blue-300">Edit</button></td></tr>)}</tbody></table></div>}
    </section>

    <Modal open={addingLimits} onClose={() => setAddingLimits(false)} title="Add shared quota limits" size="xl"><form onSubmit={(event) => { void addLimits(event).then(() => setAddingLimits(false)); }}><KeyQuotaEditor budgets={draftBudgets} onBudgetsChange={setDraftBudgets} limits={draftRateLimits} onLimitsChange={setDraftRateLimits} connections={connections} title="Quota policies" description="Assigned keys inherit these limits without changing their key-specific limits." /><div className="mt-4 flex justify-end"><button disabled={busy === "add"} className="btn btn-primary btn-md">{busy === "add" ? "Saving…" : "Add shared limits"}</button></div></form></Modal>

    <section className="rounded-lg p-5" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" }}><div className="mb-4 flex items-center justify-between"><div><h2 className="section-title">Assigned API keys</h2><p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>These keys inherit the shared limits above; their direct limits remain separate.</p></div><Link href={`/access?v=keys&usage_pool_id=${encodeURIComponent(pool.id)}`} className="text-sm" style={{ color: "var(--color-cds-status-green-text)" }}>Create token in this pool</Link></div>{keys.length === 0 ? <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>No API keys are assigned to this pool.</p> : <div className="overflow-hidden rounded-md" style={{ border: "1px solid var(--color-cds-border-strong)" }}><table className="w-full text-sm"><thead><tr style={{ background: "var(--color-cds-elevated)", color: "var(--color-cds-ink-secondary)" }}><th className="px-4 py-2 text-left font-medium">API key</th><th className="px-4 py-2 text-left font-medium">Key-specific limits</th></tr></thead><tbody>{keys.map((key, index) => <tr key={key.key_hash} style={{ background: index % 2 ? "var(--color-cds-raised)" : "var(--color-cds-inset)", borderTop: "1px solid var(--color-cds-border-strong)" }}><td className="px-4 py-3"><Link href={`/keys/${encodeURIComponent(key.key_hash)}`} className="text-white hover:underline">{key.description || key.key_prefix}</Link><div className="font-mono text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>{key.key_prefix}</div></td><td className="px-4 py-3 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>{key.usage_budgets.length} token budget{key.usage_budgets.length === 1 ? "" : "s"} · {key.rate_limits.length} rate limit{key.rate_limits.length === 1 ? "" : "s"}</td></tr>)}</tbody></table></div>}</section>

    {editingName !== null && <form onSubmit={saveName} className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"><div className="w-96 rounded-lg p-5" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border-strong)" }}><h2 className="mb-4 section-title">Rename usage pool</h2><label className="block text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Pool name<input required value={editingName} onChange={(event) => setEditingName(event.target.value)} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle} /></label><div className="mt-5 flex justify-end gap-2"><button type="button" onClick={() => setEditingName(null)} className="px-3 py-2 text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>Cancel</button><button disabled={busy === "name" || !editingName.trim()} className="btn btn-primary btn-md">{busy === "name" ? "Saving…" : "Save"}</button></div></div></form>}
    {editingBudget && <form onSubmit={(event) => { event.preventDefault(); void patchBudget(editingBudget.id, { token_limit: editingBudget.token_limit, enabled: editingBudget.enabled }).then(() => setEditingBudget(null)); }} className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"><div className="w-96 rounded-lg p-5" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border-strong)" }}><h2 className="mb-4 section-title">Edit {editingBudget.period} token budget</h2><label className="block text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Token limit<input required min="1" type="number" value={editingBudget.token_limit} onChange={(event) => setEditingBudget({ ...editingBudget, token_limit: Number(event.target.value) })} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle} /></label><label className="mt-3 flex gap-2 text-xs" style={{ color: "var(--color-cds-ink-medium)" }}><input type="checkbox" checked={editingBudget.enabled} onChange={(event) => setEditingBudget({ ...editingBudget, enabled: event.target.checked })} /> Enabled</label><div className="mt-5 flex justify-end gap-2"><button type="button" onClick={() => setEditingBudget(null)} className="px-3 py-2 text-sm" style={{ color: "var(--color-cds-ink-secondary)" }}>Cancel</button><button disabled={busy === editingBudget.id} className="btn btn-primary btn-md">Save</button></div></div></form>}
    {editingRateLimit && rateLimitEdit && <form onSubmit={saveRateLimitEdit} className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"><div className="w-[42rem] rounded-lg p-5" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-status-blue-border)" }}><div className="mb-4 flex items-center justify-between"><div><h2 className="section-title">Edit shared rate limit</h2><p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>This policy remains owned by {pool.name}.</p></div><button type="button" onClick={() => { setEditingRateLimit(null); setRateLimitEdit(null); }} disabled={busy === editingRateLimit.id} className="text-xs disabled:opacity-50" style={{ color: "var(--color-cds-ink-secondary)" }}>Cancel</button></div><div className="space-y-4"><ConnectionModelPicker connectionId={rateLimitEdit.draft.connectionId} modelAccess={rateLimitEdit.draft.modelAccess} models={rateLimitEdit.draft.models} connections={connections} onChange={({ connectionId, modelAccess, models }) => setRateLimitEdit({ ...rateLimitEdit, draft: { ...rateLimitEdit.draft, connectionId, modelAccess, models } })} /><div className="grid grid-cols-3 gap-3"><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Rate limit type<select value={rateLimitEdit.draft.limitType} onChange={(event) => setRateLimitEdit({ ...rateLimitEdit, draft: { ...rateLimitEdit.draft, limitType: event.target.value as RateLimitType, burstValue: event.target.value === "concurrent_requests" ? "" : rateLimitEdit.draft.burstValue } })} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle}><option value="tokens_per_minute">Tokens / min</option><option value="requests_per_minute">Requests / min</option><option value="concurrent_requests">Concurrent requests</option></select></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Limit<input value={rateLimitEdit.draft.limitValue} onChange={(event) => setRateLimitEdit({ ...rateLimitEdit, draft: { ...rateLimitEdit.draft, limitValue: event.target.value } })} inputMode="numeric" className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle} /></label><label className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Burst capacity <span style={{ color: "var(--color-cds-ink-muted)" }}>(optional)</span><input value={rateLimitEdit.draft.burstValue} onChange={(event) => setRateLimitEdit({ ...rateLimitEdit, draft: { ...rateLimitEdit.draft, burstValue: event.target.value } })} inputMode="numeric" disabled={rateLimitEdit.draft.limitType === "concurrent_requests"} placeholder={rateLimitEdit.draft.limitType === "concurrent_requests" ? "not applicable" : "defaults to limit"} className="mt-1 w-full rounded-md px-3 py-2 text-sm disabled:opacity-50" style={inputStyle} /></label></div><label className="flex gap-2 text-xs" style={{ color: "var(--color-cds-ink-medium)" }}><input type="checkbox" checked={rateLimitEdit.enabled} onChange={(event) => setRateLimitEdit({ ...rateLimitEdit, enabled: event.target.checked })} /> Enabled</label>{error && <p className="rounded px-3 py-2 text-xs" style={{ background: "var(--color-cds-status-red-fill)", color: "var(--color-cds-status-red-text)" }}>{error}</p>}<div className="flex justify-end"><button disabled={busy === editingRateLimit.id || !rateLimitEdit.draft.limitValue.trim()} className="btn btn-primary btn-md">{busy === editingRateLimit.id ? "Saving…" : "Save changes"}</button></div></div></div></form>}
  </div>;
}
