"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { ConfirmationModal } from "@/components/ConfirmationModal";
import { InfiniteScroll, Modal, PageHeader } from "@/components/router/RouterPrimitives";
import { fetchAdminPage, fetchAllAdminPages } from "@/lib/adminPagination";
import { formatCompactTokenLimit, formatDate } from "@/lib/format";
import { usePagination } from "@/lib/usePagination";
import {
  KeyQuotaEditor,
  budgetPayload,
  rateLimitPayload,
  validateQuota,
  type KeyBudgetDraft,
  type KeyRateLimitDraft,
} from "@/app/tokens/KeyQuotaEditor";

interface UsagePool {
  id: string;
  name: string;
  created_at: string;
}

interface UsageBudget {
  id: string;
  scope_id: string;
  token_limit: number;
  period: "daily" | "weekly" | "monthly";
  enabled: boolean;
}

interface RateLimitPolicy {
  id: string;
  scope: "api_key" | "usage_pool";
  scope_id: string;
  enabled: boolean;
}

interface Connection {
  id: string;
  name: string;
  provider_kind: string;
}

function sharedQuotaSummary(poolId: string, budgets: UsageBudget[], rateLimits: RateLimitPolicy[]): string | null {
  const periodOrder = { daily: 0, weekly: 1, monthly: 2 };
  const parts = budgets
    .filter((budget) => budget.scope_id === poolId)
    .sort((left, right) => periodOrder[left.period] - periodOrder[right.period])
    .map((budget) => `${formatCompactTokenLimit(budget.token_limit)}/${budget.period === "daily" ? "day" : budget.period === "weekly" ? "week" : "month"}${budget.enabled ? "" : " (disabled)"}`);
  const poolRateLimits = rateLimits.filter((limit) => limit.scope === "usage_pool" && limit.scope_id === poolId);
  if (poolRateLimits.length > 0) parts.push(`${poolRateLimits.length} rate limit${poolRateLimits.length === 1 ? "" : "s"}`);
  return parts.length > 0 ? parts.join(" · ") : null;
}

export default function UsagePoolsPage({ embedded = false }: { embedded?: boolean }) {
  const router = useRouter();
  const { items: pools, setItems: setPools, loading, setLoading, nextCursor, applyPage } = usePagination<UsagePool>((pool) => pool.id);
  const [budgets, setBudgets] = useState<UsageBudget[]>([]);
  const [rateLimits, setRateLimits] = useState<RateLimitPolicy[]>([]);
  const [connections, setConnections] = useState<Connection[]>([]);
  const [quotaLoading, setQuotaLoading] = useState(true);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [draftBudgets, setDraftBudgets] = useState<KeyBudgetDraft[]>([]);
  const [draftRateLimits, setDraftRateLimits] = useState<KeyRateLimitDraft[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [poolPendingDeletion, setPoolPendingDeletion] = useState<UsagePool | null>(null);
  const [deletingPoolId, setDeletingPoolId] = useState<string | null>(null);

  async function fetchPools(cursor?: string): Promise<UsagePool[] | null> {
    setLoading(true);
    try {
      const page = await fetchAdminPage<UsagePool>("/api/admin/usage-pools", cursor);
      applyPage(page, cursor);
      return page.data;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setLoading(false);
    }
  }

  async function fetchSharedQuotaData() {
    setQuotaLoading(true);
    try {
      const [budgetResponse, allRateLimits, allConnections] = await Promise.all([
        fetch("/api/admin/usage-budgets?scope=usage_pool"),
        fetchAllAdminPages<RateLimitPolicy>("/api/admin/rate-limits"),
        fetchAllAdminPages<Connection>("/api/admin/providers"),
      ]);
      if (!budgetResponse.ok) throw new Error(await budgetResponse.text());
      const budgetJson: { data?: UsageBudget[] } = await budgetResponse.json();
      setBudgets(budgetJson.data ?? []);
      setRateLimits(allRateLimits.filter((limit) => limit.scope === "usage_pool"));
      setConnections(allConnections);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setQuotaLoading(false);
    }
  }

  useEffect(() => {
    void fetchPools();
    void fetchSharedQuotaData();
  }, []);

  async function handleNextPage() {
    if (!nextCursor) return;
    await fetchPools(nextCursor);
  }

  async function reloadAfterRemoval() {
    await fetchPools();
  }

  async function handleCreate(event: React.FormEvent) {
    event.preventDefault();
    if (!name.trim()) return;
    const quotaError = validateQuota(draftBudgets, draftRateLimits);
    if (quotaError) {
      setError(quotaError);
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      const response = await fetch("/api/admin/usage-pools", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          name: name.trim(),
          usage_budgets: budgetPayload(draftBudgets),
          rate_limits: rateLimitPayload(draftRateLimits),
        }),
      });
      if (!response.ok) throw new Error(await response.text());
      const pool: UsagePool = await response.json();
      router.push(`/usage-pools/${encodeURIComponent(pool.id)}?setup=created`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  }

  async function handleDelete(id: string): Promise<boolean> {
    setDeletingPoolId(id);
    try {
      const response = await fetch(`/api/admin/usage-pools/${encodeURIComponent(id)}`, { method: "DELETE" });
      if (!response.ok) throw new Error(await response.text());
      await Promise.all([reloadAfterRemoval(), fetchSharedQuotaData()]);
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setDeletingPoolId(null);
    }
  }

  return (
    <div>
      <div className="mb-6"><PageHeader title="Usage pools" description="Shared token budgets and rate limits for groups of API keys." action={<button onClick={() => { setAdding(true); setError(null); }} className="btn btn-primary btn-md">Create usage pool</button>} /></div>

      {error && <div className="mb-4 rounded-lg px-4 py-3 text-sm" style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}><span className="font-medium">Error:</span> {error}</div>}

      {loading ? <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>Loading...</p> : pools.length === 0 ? <div className="mb-6 rounded-lg px-6 py-12 text-center" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" }}><p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>No usage pools yet.</p></div> : <div className="mb-6 overflow-hidden rounded-lg" style={{ border: "1px solid var(--color-cds-border)" }}>
        <table className="w-full text-sm">
          <thead><tr style={{ background: "var(--color-cds-raised)", borderBottom: "1px solid var(--color-cds-border)" }}><th className="px-5 py-3 text-left font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Name</th><th className="px-5 py-3 text-left font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Shared quota limits</th><th className="px-5 py-3 text-left font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Created</th><th className="sticky right-0 bg-cds-raised px-5 py-3 text-right font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>Actions</th></tr></thead>
          <tbody>{pools.map((pool, index) => {
            const summary = sharedQuotaSummary(pool.id, budgets, rateLimits);
            return <tr key={pool.id} role="link" tabIndex={0} onClick={(event) => { if (!(event.target as HTMLElement).closest("a,button,input")) router.push(`/usage-pools/${encodeURIComponent(pool.id)}`); }} onKeyDown={(event) => { if (event.key === "Enter") router.push(`/usage-pools/${encodeURIComponent(pool.id)}`); }} className="cursor-pointer hover:bg-cds-hover" style={{ background: index % 2 === 0 ? "var(--color-cds-elevated)" : "var(--color-cds-raised)", borderBottom: index < pools.length - 1 ? "1px solid var(--color-cds-border)" : undefined }}>
              <td className="px-5 py-3"><Link href={`/usage-pools/${encodeURIComponent(pool.id)}`} className="font-medium text-white hover:underline">{pool.name}</Link><div className="mt-0.5 font-mono text-[11px]" style={{ color: "var(--color-cds-ink-muted)" }}>{pool.id}</div></td>
              <td className="px-5 py-3 text-xs">{quotaLoading ? <span style={{ color: "var(--color-cds-ink-subtle)" }}>Loading…</span> : summary ? <span style={{ color: "var(--color-cds-ink-medium)" }}>{summary}</span> : <span><span style={{ color: "var(--color-cds-ink-subtle)" }}>No shared limits</span><span className="mx-1" style={{ color: "var(--color-cds-ink-muted)" }}>·</span><Link href={`/usage-pools/${encodeURIComponent(pool.id)}`} style={{ color: "var(--color-cds-status-green-text)" }}>Configure limits</Link></span>}</td>
              <td className="px-5 py-3 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>{formatDate(pool.created_at)}</td>
              <td className="sticky right-0 bg-inherit px-5 py-3 text-right"><div className="flex items-center justify-end gap-2"><Link href={`/usage-pools/${encodeURIComponent(pool.id)}`} className="rounded px-2.5 py-1 text-xs" style={{ background: "var(--color-cds-status-neutral-fill)", color: "var(--color-cds-status-green-text)", border: "1px solid var(--color-cds-border-strong)" }}>Configure limits</Link><Link href={`/access?v=keys&issue=1&usage_pool_id=${encodeURIComponent(pool.id)}`} className="rounded px-2.5 py-1 text-xs" style={{ background: "var(--color-cds-status-green-fill)", color: "var(--color-cds-status-green-text)", border: "1px solid var(--color-cds-status-green-border)" }}>Create token</Link><button onClick={() => setPoolPendingDeletion(pool)} disabled={deletingPoolId === pool.id} className="rounded px-2.5 py-1 text-xs disabled:opacity-50" style={{ background: "var(--color-cds-status-red-fill)", color: "var(--color-cds-status-red-text)", border: "1px solid var(--color-cds-status-red-border)" }}>Delete</button></div></td>
            </tr>;
          })}</tbody>
        </table>
      </div>}

      <InfiniteScroll hasMore={Boolean(nextCursor)} loading={loading} onLoadMore={() => void handleNextPage()} />

      <Modal open={adding} onClose={() => setAdding(false)} title="Create usage pool" size="xl"><form onSubmit={handleCreate}>
        <p className="mb-4 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>The pool and its shared quota policies are created together.</p>
        <label className="mb-4 block text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>Pool name<input type="text" value={name} onChange={(event) => setName(event.target.value)} placeholder="e.g. Production assistants" className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={{ background: "var(--color-cds-field)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink)" }} /></label>
        <KeyQuotaEditor budgets={draftBudgets} onBudgetsChange={setDraftBudgets} limits={draftRateLimits} onLimitsChange={setDraftRateLimits} connections={connections} title="Shared quota limits (optional)" description="Every API key assigned to this pool inherits these limits. Key-specific limits remain separate and additive." />
        <div className="mt-4 flex justify-end"><button type="submit" disabled={submitting || !name.trim()} className="btn btn-primary btn-md">{submitting ? "Creating…" : "Create usage pool"}</button></div>
      </form></Modal>

      <p className="mt-4 text-xs" style={{ color: "var(--color-cds-ink-muted)" }}>{pools.length} usage pool{pools.length !== 1 ? "s" : ""} loaded</p>
      <ConfirmationModal open={poolPendingDeletion !== null} title="Delete usage pool?" description={`This permanently removes ${poolPendingDeletion?.name ?? "this usage pool"}. Existing keys assigned to it will no longer inherit its shared token budgets or rate limits.`} confirmLabel="Delete usage pool" confirming={poolPendingDeletion?.id === deletingPoolId} onCancel={() => setPoolPendingDeletion(null)} onConfirm={async () => { if (poolPendingDeletion && await handleDelete(poolPendingDeletion.id)) setPoolPendingDeletion(null); }} />
    </div>
  );
}
