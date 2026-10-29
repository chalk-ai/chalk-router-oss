"use client";

import { useEffect, useMemo, useState } from "react";
import { ConfirmationModal } from "@/components/ConfirmationModal";
import { InfiniteScroll, inputStyle, Modal, PageHeader } from "@/components/router/RouterPrimitives";
import { fetchAdminPage } from "@/lib/adminPagination";
import { formatNumber } from "@/lib/format";
import { usePagination } from "@/lib/usePagination";
import { ConnectionModelPicker, rateLimitIdentityKey } from "@/app/tokens/KeyQuotaEditor";

type LimitType = "tokens_per_minute" | "requests_per_minute" | "concurrent_requests";
type ScopeType = "usage_pool" | "api_key";

interface RateLimitDraft {
  limit_type: LimitType;
  limit_value: string;
  burst_value: string;
  connection_id: string;
  model_access: "all" | "specific";
  models: string[];
  scope: ScopeType;
  scope_id: string;
  enabled: boolean;
}

interface RateLimitPolicy {
  id: string;
  limit_type: LimitType;
  limit_value: number;
  burst_value: number | null;
  connection_id: string | null;
  models: string[];
  scope: ScopeType;
  scope_id: string;
  enabled: boolean;
  created_at: string;
}

interface Pool {
  id: string;
  name: string;
}

interface Token {
  key: string;
  description: string;
}

interface Connection {
  id: string;
  name: string;
  provider_kind: string;
}

const LIMIT_TYPE_LABELS: Record<LimitType, string> = {
  tokens_per_minute: "Tokens / min",
  requests_per_minute: "Requests / min",
  concurrent_requests: "Concurrent requests",
};

const SCOPE_LABELS: Record<ScopeType, string> = {
  usage_pool: "Usage pool",
  api_key: "API token",
};

function emptyRateLimitDraft(): RateLimitDraft {
  return { limit_type: "tokens_per_minute", limit_value: "", burst_value: "", connection_id: "", model_access: "all", models: [], scope: "usage_pool", scope_id: "", enabled: true };
}

function truncateKey(key: string): string {
  if (key.length <= 20) return key;
  return key.slice(0, 8) + "..." + key.slice(-8);
}

export default function RateLimitsPage({ embedded = false }: { embedded?: boolean }) {
  const { items: policies, setItems: setPolicies, loading, setLoading, nextCursor, applyPage } = usePagination<RateLimitPolicy>((policy) => policy.id);
  const [teams, setTeams] = useState<Pool[]>([]);
  const [tokens, setTokens] = useState<Token[]>([]);
  const [connections, setConnections] = useState<Connection[]>([]);
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [policyPendingRemoval, setPolicyPendingRemoval] = useState<RateLimitPolicy | null>(null);
  const [removingPolicyId, setRemovingPolicyId] = useState<string | null>(null);
  const [form, setForm] = useState<RateLimitDraft>(emptyRateLimitDraft);
  const [editing, setEditing] = useState<RateLimitPolicy | null>(null);
  const [editForm, setEditForm] = useState<RateLimitDraft>(emptyRateLimitDraft);
  const [savingEdit, setSavingEdit] = useState(false);

  /** Resolves to the policies loaded, or null when the fetch failed. */
  async function fetchAll(cursor?: string): Promise<RateLimitPolicy[] | null> {
    setLoading(true);
    try {
      const [policyPage, poolPage, tokenPage, connectionPage] = await Promise.all([
        fetchAdminPage<RateLimitPolicy>("/api/admin/rate-limits", cursor),
        fetchAdminPage<Pool>("/api/admin/usage-pools", undefined, 1000),
        fetchAdminPage<{ key_hash: string; description: string }>(
          "/api/admin/keys",
          undefined,
          1000,
        ),
        fetchAdminPage<Connection>("/api/admin/providers", undefined, 1000),
      ]);
      applyPage(policyPage, cursor);
      setTeams(poolPage.data);
      setTokens(tokenPage.data.map((key) => ({
        key: key.key_hash,
        description: key.description,
      })));
      setConnections(connectionPage.data);
      return policyPage.data;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void fetchAll(); }, []);

  // `scopeLabel`/`connectionLabel` run once per policy row (potentially many
  // rows) each render; index the reference lists into `Map`s once so each
  // row's lookup is O(1) instead of an O(teams|tokens|connections) `.find()`.
  const teamsById = useMemo(() => new Map(teams.map((t) => [t.id, t])), [teams]);
  const tokensByKey = useMemo(() => new Map(tokens.map((t) => [t.key, t])), [tokens]);
  const connectionsById = useMemo(() => new Map(connections.map((c) => [c.id, c])), [connections]);

  async function handleNextPage() {
    if (!nextCursor) return;
    await fetchAll(nextCursor);
  }

  async function resetToFirstPage() {
    if (await fetchAll()) {
    }
  }

  // Deleting the last row of a page leaves the table blank; step back so the
  // user lands on rows instead of on an empty page with a live "Previous".
  async function reloadAfterRemoval() {
    await fetchAll();
  }

  async function handleAdd() {
    const val = parseInt(form.limit_value, 10);
    const burst = form.burst_value.trim() ? parseInt(form.burst_value, 10) : undefined;
    if (!form.scope_id || isNaN(val) || val <= 0 || (burst !== undefined && (isNaN(burst) || burst <= 0))) return;
    if (form.model_access === "specific" && form.models.length === 0) {
      setError("Choose at least one model, or use All models.");
      return;
    }
    const identity = rateLimitIdentityKey(form.limit_type, form.connection_id, form.models);
    if (policies.some((policy) => policy.scope === form.scope && policy.scope_id === form.scope_id && rateLimitIdentityKey(policy.limit_type, policy.connection_id ?? "", policy.models) === identity)) {
      setError("This target already has this rate limit. Edit the existing rate limit instead.");
      return;
    }
    setError(null);
    try {
      const res = await fetch("/api/admin/rate-limits", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          limit_type: form.limit_type,
          limit_value: val,
          burst_value: burst,
          connection_id: form.connection_id || null,
          models: form.connection_id ? form.models : [],
          scope: form.scope,
          scope_id: form.scope_id,
          enabled: form.enabled,
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      await res.json();
      await resetToFirstPage();
      setAdding(false);
      setForm(emptyRateLimitDraft());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  function beginEdit(policy: RateLimitPolicy) {
    setError(null);
    setAdding(false);
    setEditing(policy);
    setEditForm({ limit_type: policy.limit_type, limit_value: String(policy.limit_value), burst_value: policy.burst_value == null ? "" : String(policy.burst_value), connection_id: policy.connection_id ?? "", model_access: policy.models.length ? "specific" : "all", models: policy.models, scope: policy.scope, scope_id: policy.scope_id, enabled: policy.enabled });
  }

  async function handleEdit() {
    const value = parseInt(editForm.limit_value, 10);
    const burst = editForm.burst_value.trim() ? parseInt(editForm.burst_value, 10) : undefined;
    if (!editing || isNaN(value) || value <= 0 || (burst !== undefined && (isNaN(burst) || burst <= 0))) return;
    if (editForm.model_access === "specific" && editForm.models.length === 0) {
      setError("Choose at least one model, or use All models.");
      return;
    }
    const identity = rateLimitIdentityKey(editForm.limit_type, editForm.connection_id, editForm.models);
    if (policies.some((policy) => policy.id !== editing.id && policy.scope === editing.scope && policy.scope_id === editing.scope_id && rateLimitIdentityKey(policy.limit_type, policy.connection_id ?? "", policy.models) === identity)) {
      setError("This target already has this rate limit. Edit the existing rate limit instead.");
      return;
    }
    setError(null);
    setSavingEdit(true);
    try {
      const res = await fetch(`/api/admin/rate-limits/${encodeURIComponent(editing.id)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ limit_type: editForm.limit_type, limit_value: value, burst_value: burst, connection_id: editForm.connection_id || null, models: editForm.connection_id ? editForm.models : [], enabled: editForm.enabled }),
      });
      if (!res.ok) throw new Error(await res.text());
      const updated: RateLimitPolicy = await res.json();
      setPolicies((current) => current.map((policy) => policy.id === updated.id ? updated : policy));
      setEditing(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSavingEdit(false);
    }
  }

  async function handleRemove(id: string): Promise<boolean> {
    setRemovingPolicyId(id);
    try {
      const res = await fetch(`/api/admin/rate-limits/${encodeURIComponent(id)}`, { method: "DELETE" });
      if (!res.ok) throw new Error(await res.text());
      await reloadAfterRemoval();
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setRemovingPolicyId(null);
    }
  }

  function scopeLabel(p: RateLimitPolicy): string {
    if (p.scope === "usage_pool") {
      const team = teamsById.get(p.scope_id);
      return team ? team.name : p.scope_id;
    }
    const token = tokensByKey.get(p.scope_id);
    return token?.description || truncateKey(p.scope_id);
  }

  function connectionLabel(connectionId: string): string {
    const connection = connectionsById.get(connectionId);
    return connection ? `${connection.name} (${connection.provider_kind})` : connectionId;
  }

  function selectorLabel(policy: RateLimitPolicy): string {
    if (!policy.connection_id) return "All connections · All models";
    return `${connectionLabel(policy.connection_id)} · ${policy.models.length ? policy.models.join(", ") : "All models"}`;
  }

  function policyDescription(policy: RateLimitPolicy): string {
    return `${scopeLabel(policy)} · ${LIMIT_TYPE_LABELS[policy.limit_type]} · ${selectorLabel(policy)}`;
  }

  if (loading) return <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>Loading...</p>;

  return (
    <div>
      <div className="mb-6"><PageHeader title="Rate limits" description="Limit an API key or usage pool globally or by connection and model." action={<button onClick={() => setAdding(true)} className="btn btn-primary btn-md">Create rate limit</button>} /></div>

      {error && (
        <div className="mb-4 px-4 py-3 rounded-lg text-sm" style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}>
          <span className="font-medium">Error:</span> {error}
        </div>
      )}

      {policies.length > 0 && (
        <div className="rounded-lg overflow-hidden mb-6" style={{ border: "1px solid var(--color-cds-border)" }}>
          <table className="w-full text-sm">
            <thead>
              <tr style={{ background: "var(--color-cds-raised)", borderBottom: "1px solid var(--color-cds-border)" }}>
                <th className="text-left px-5 py-3 font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Applies to</th>
                <th className="text-left px-5 py-3 font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Target</th>
                <th className="text-left px-5 py-3 font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Type</th>
                <th className="text-right px-5 py-3 font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Limit</th>
                <th className="text-right px-5 py-3 font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Burst</th>
                <th className="text-left px-5 py-3 font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Status</th>
                <th className="sticky right-0 bg-cds-raised px-5 py-3 text-right font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {policies.map((p, i) => (
                <tr key={p.id} style={{ background: i % 2 === 0 ? "var(--color-cds-elevated)" : "var(--color-cds-raised)", borderBottom: i < policies.length - 1 ? "1px solid var(--color-cds-border)" : undefined, opacity: p.enabled ? 1 : 0.5, transition: "opacity 0.15s" }}>
                  <td className="px-5 py-3">
                    <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: p.scope === "usage_pool" ? "var(--color-cds-status-orange-fill)" : "var(--color-cds-status-green-fill)", color: p.scope === "usage_pool" ? "var(--color-cds-status-orange-text)" : "var(--color-cds-status-green-text)" }}>{SCOPE_LABELS[p.scope]}</span>
                  </td>
                  <td className="px-5 py-3">
                    <div className="font-medium text-white">{scopeLabel(p)}</div>
                    <div className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>{selectorLabel(p)}</div>
                  </td>
                  <td className="px-5 py-3">
                    <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "var(--color-cds-status-neutral-fill)", color: "var(--color-cds-ink-secondary)" }}>{LIMIT_TYPE_LABELS[p.limit_type]}</span>
                  </td>
                  <td className="px-5 py-3 text-right tabular-nums font-medium" style={{ color: "var(--color-cds-ink)" }}>
                    {formatNumber(p.limit_value)}
                  </td>
                  <td className="px-5 py-3 text-right tabular-nums" style={{ color: "var(--color-cds-ink-medium)" }}>{p.limit_type === "concurrent_requests" ? "—" : formatNumber(p.burst_value ?? p.limit_value)}</td>
                  <td className="px-5 py-3">
                    {p.enabled ? (
                      <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "var(--color-cds-status-green-fill)", color: "var(--color-cds-status-green-text)" }}>Enabled</span>
                    ) : (
                      <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "var(--color-cds-status-neutral-fill)", color: "var(--color-cds-ink-subtle)" }}>Disabled</span>
                    )}
                  </td>
                  <td className="sticky right-0 bg-inherit px-5 py-3 text-right">
                    <div className="flex items-center justify-end gap-2">
                      <button onClick={() => beginEdit(p)} className="text-xs px-2.5 py-1 rounded transition-colors" style={{ background: "var(--color-cds-status-blue-fill)", color: "var(--color-cds-status-blue-text)", border: "1px solid var(--color-cds-status-blue-border)" }}>
                        Edit
                      </button>
                      <button onClick={() => setPolicyPendingRemoval(p)} disabled={removingPolicyId === p.id} className="text-xs px-2.5 py-1 rounded transition-colors disabled:opacity-50" style={{ background: "var(--color-cds-status-red-fill)", color: "var(--color-cds-status-red-text)", border: "1px solid var(--color-cds-status-red-border)" }}>
                        Remove
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {policies.length === 0 && !adding && (
        <div className="px-6 py-12 rounded-lg text-center mb-6" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" }}>
          <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>No rate limit policies configured.</p>
        </div>
      )}

      <InfiniteScroll hasMore={Boolean(nextCursor)} loading={loading} onLoadMore={() => void handleNextPage()} />

      {editing && <Modal open onClose={() => setEditing(null)} title="Edit rate limit" size="xl">
        <div>
          <div className="flex items-center justify-between mb-4"><h3 className="text-cds-md font-medium text-cds-ink-strong">Edit rate limit</h3><button onClick={() => setEditing(null)} disabled={savingEdit} className="text-xs disabled:opacity-50" style={{ color: "var(--color-cds-ink-subtle)" }}>Cancel</button></div>
          <div className="space-y-4">
            <div className="grid grid-cols-2 gap-4"><div><span className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Applies to</span><div className="flex h-10 items-center rounded-md px-3 text-sm" style={{ ...inputStyle, color: "var(--color-cds-ink-secondary)" }}>{SCOPE_LABELS[editing.scope]}</div></div><div><span className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Target</span><div className="flex h-10 items-center rounded-md px-3 text-sm" style={{ ...inputStyle, color: "var(--color-cds-ink-secondary)" }}>{scopeLabel(editing)}</div></div></div>
            <ConnectionModelPicker connectionId={editForm.connection_id} modelAccess={editForm.model_access} models={editForm.models} connections={connections} onChange={({ connectionId, modelAccess, models }) => setEditForm((current) => ({ ...current, connection_id: connectionId, model_access: modelAccess, models }))} />
            <div className="grid grid-cols-3 gap-4"><div><label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Rate limit type</label><select value={editForm.limit_type} onChange={(e) => setEditForm((current) => ({ ...current, limit_type: e.target.value as LimitType, burst_value: e.target.value === "concurrent_requests" ? "" : current.burst_value }))} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle}><option value="tokens_per_minute">Tokens / min</option><option value="requests_per_minute">Requests / min</option><option value="concurrent_requests">Concurrent requests</option></select></div><div><label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Limit</label><input type="text" value={editForm.limit_value} onChange={(e) => setEditForm((current) => ({ ...current, limit_value: e.target.value }))} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle} /></div><div><label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Burst capacity <span style={{ color: "var(--color-cds-ink-muted)" }}>(optional)</span></label><input type="text" value={editForm.burst_value} onChange={(e) => setEditForm((current) => ({ ...current, burst_value: e.target.value }))} disabled={editForm.limit_type === "concurrent_requests"} placeholder={editForm.limit_type === "concurrent_requests" ? "not applicable" : "defaults to limit"} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none disabled:opacity-50" style={inputStyle} /></div></div>
            <label className="flex items-center gap-2 text-xs" style={{ color: "var(--color-cds-ink-medium)" }}><input type="checkbox" checked={editForm.enabled} onChange={(e) => setEditForm((current) => ({ ...current, enabled: e.target.checked }))} /> Enabled</label>
            <div className="flex justify-end"><button onClick={handleEdit} disabled={savingEdit || !editForm.limit_value.trim()} className="btn btn-primary btn-md">{savingEdit ? "Saving…" : "Save changes"}</button></div>
          </div>
        </div>
      </Modal>}
      {adding && <Modal open onClose={() => setAdding(false)} title="Create rate limit" size="xl">
        <div>
          <div className="flex items-center justify-between mb-4">
            <h3 className="text-cds-md font-medium text-cds-ink-strong">Create rate limit</h3>
            <button onClick={() => setAdding(false)} className="text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>Cancel</button>
          </div>
          <div className="space-y-4">
            <div className="grid grid-cols-2 gap-4">
              <div>
                <label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Applies to</label>
                <select value={form.scope} onChange={(e) => setForm((f) => ({ ...f, scope: e.target.value as ScopeType, scope_id: "" }))} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle}><option value="usage_pool">Usage pool</option><option value="api_key">API token</option></select>
              </div>
              <div>
                <label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Target</label>
                <select value={form.scope_id} onChange={(e) => setForm((f) => ({ ...f, scope_id: e.target.value }))} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle}><option value="">Choose…</option>{form.scope === "usage_pool" ? teams.map((team) => <option key={team.id} value={team.id}>{team.name}</option>) : tokens.map((token) => <option key={token.key} value={token.key}>{token.description || truncateKey(token.key)}</option>)}</select>
              </div>
            </div>
            <ConnectionModelPicker connectionId={form.connection_id} modelAccess={form.model_access} models={form.models} connections={connections} onChange={({ connectionId, modelAccess, models }) => setForm((current) => ({ ...current, connection_id: connectionId, model_access: modelAccess, models }))} />
            <div className="grid grid-cols-3 gap-4"><div><label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Rate limit type</label><select value={form.limit_type} onChange={(e) => setForm((f) => ({ ...f, limit_type: e.target.value as LimitType }))} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle}><option value="tokens_per_minute">Tokens / min</option><option value="requests_per_minute">Requests / min</option><option value="concurrent_requests">Concurrent requests</option></select></div><div><label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Limit</label><input type="text" value={form.limit_value} onChange={(e) => setForm((f) => ({ ...f, limit_value: e.target.value }))} placeholder="e.g. 100000" className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle} /></div><div><label className="block text-xs font-medium mb-1.5" style={{ color: "var(--color-cds-ink-secondary)" }}>Burst capacity <span style={{ color: "var(--color-cds-ink-muted)" }}>(optional)</span></label><input type="text" value={form.burst_value} onChange={(e) => setForm((f) => ({ ...f, burst_value: e.target.value }))} disabled={form.limit_type === "concurrent_requests"} placeholder={form.limit_type === "concurrent_requests" ? "not applicable" : "defaults to limit"} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none disabled:opacity-50" style={inputStyle} /></div></div>
            <label className="flex items-center gap-2 text-xs" style={{ color: "var(--color-cds-ink-medium)" }}><input type="checkbox" checked={form.enabled} onChange={(e) => setForm((current) => ({ ...current, enabled: e.target.checked }))} /> Enabled</label>
            <div className="flex justify-end">
              <button onClick={handleAdd} disabled={!form.scope_id || !form.limit_value.trim()} className="btn btn-primary btn-md">
                Create rate limit
              </button>
            </div>
          </div>
        </div>
      </Modal>}
      <ConfirmationModal
        open={policyPendingRemoval !== null}
        title="Remove rate limit policy?"
        description={`This permanently removes ${policyPendingRemoval ? policyDescription(policyPendingRemoval) : "this rate limit policy"}. Requests will no longer be limited by it.`}
        confirmLabel="Remove policy"
        confirming={policyPendingRemoval?.id === removingPolicyId}
        onCancel={() => setPolicyPendingRemoval(null)}
        onConfirm={async () => {
          if (policyPendingRemoval && await handleRemove(policyPendingRemoval.id)) {
            setPolicyPendingRemoval(null);
          }
        }}
      />
    </div>
  );
}
