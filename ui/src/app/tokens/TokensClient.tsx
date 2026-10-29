"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { ConfirmationModal } from "@/components/ConfirmationModal";
import { IconButton, InfiniteScroll, inputStyle, Modal, PageHeader } from "@/components/router/RouterPrimitives";
import {
  apiKeyDescriptionError,
  MAX_API_KEY_DESCRIPTION_LENGTH,
  normalizeApiKeyDescription,
} from "@/lib/apiKeyDescription";
import { fetchAdminPage } from "@/lib/adminPagination";
import { formatCompactTokenLimit, formatDateTime } from "@/lib/format";
import { usePagination } from "@/lib/usePagination";
import {
  grantedRouteIds,
  keyAccessDraftFromPermission,
  keyAccessError,
  keyAccessKind,
  keyAccessPayload,
  routeGrantLabels,
  type KeyAccessDraft,
  type KeyAccessJson,
  type RouteOption,
} from "@/lib/keyAccess";
import { IssueTokenForm } from "./IssueTokenForm";
import { KeyAccessEditor } from "./KeyAccessEditor";
import { Quickstart } from "./Quickstart";
import { KeyQuotaEditor, budgetPayload, rateLimitDraftsFromKey, rateLimitPayload, validateQuota, type InheritedQuotaPolicies, type KeyBudgetDraft, type KeyRateLimitDraft } from "./KeyQuotaEditor";

interface ApiKeyPermission extends KeyAccessJson {
  downgradable?: boolean | null;
  model_floor?: string | null;
  cache?: {
    enabled: boolean;
    scope: "model" | "team" | "token";
    ttl_secs: number | null;
    max_entry_bytes: number | null;
  } | null;
}

interface ApiKeyView {
  key_hash: string;
  key_prefix: string;
  // Present only on a freshly-created key (returned once); never in list/get.
  raw_key?: string;
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

interface PoolOption {
  id: string;
  name: string;
}

interface ConnectionOption {
  id: string;
  name: string;
  provider_kind: string;
  prefix?: string;
}

type PoolQuotaPolicies = InheritedQuotaPolicies;

interface TokensClientProps {
  pools: PoolOption[];
  connections: ConnectionOption[];
  routes: RouteOption[];
  initialKeys: ApiKeyView[];
  initialNextCursor?: string;
  poolQuotaPolicies: Record<string, PoolQuotaPolicies>;
  initialUsagePoolId?: string;
  initialIssueOpen?: boolean;
  initialEditKeyHash?: string;
}

function quotaSummary(
  budgets: { token_limit: number; period: "daily" | "weekly" | "monthly"; enabled: boolean }[],
  rateLimitCount: number,
): string | null {
  const summary = budgets
    .filter((budget) => budget.enabled)
    .map((budget) => `${formatCompactTokenLimit(budget.token_limit)}/${budget.period === "daily" ? "day" : budget.period === "weekly" ? "week" : "month"}`);
  if (rateLimitCount > 0) summary.push(`${rateLimitCount} rate limit${rateLimitCount === 1 ? "" : "s"}`);
  return summary.length ? summary.join(" · ") : null;
}

interface EditState {
  description: string;
  budgets: KeyBudgetDraft[];
  usagePoolId: string;
  access: KeyAccessDraft;
  rateLimits: KeyRateLimitDraft[];
}

function EditTokenForm({
  token,
  pools,
  connections,
  routes,
  poolQuotaPolicies,
  onSave,
  onCancel,
}: {
  token: ApiKeyView;
  pools: PoolOption[];
  connections: ConnectionOption[];
  routes: RouteOption[];
  poolQuotaPolicies: Record<string, PoolQuotaPolicies>;
  onSave: (updated: ApiKeyView) => void;
  onCancel: () => void;
}) {
  const [state, setState] = useState<EditState>(() => ({
    description: token.description,
    budgets: token.usage_budgets.map((budget, id) => ({ id, period: budget.period, tokenLimit: budget.token_limit.toString(), enabled: budget.enabled })),
    usagePoolId: token.usage_pool_id ?? "",
    access: keyAccessDraftFromPermission(token.permission),
    rateLimits: rateLimitDraftsFromKey(token.rate_limits),
  }));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSave() {
    setSaving(true);
    setError(null);

    const descriptionError = apiKeyDescriptionError(state.description);
    if (descriptionError) {
      setError(descriptionError);
      setSaving(false);
      return;
    }
    const accessError = keyAccessError(state.access);
    if (accessError) {
      setError(accessError);
      setSaving(false);
      return;
    }
    const quotaError = validateQuota(state.budgets, state.rateLimits);
    if (quotaError) {
      setError(quotaError);
      setSaving(false);
      return;
    }

    try {
      const res = await fetch(
        `/api/admin/keys/${encodeURIComponent(token.key_hash)}`,
        {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            description: normalizeApiKeyDescription(state.description),
            usage_budgets: budgetPayload(state.budgets),
            usage_pool_id: state.usagePoolId || null,
            rate_limits: rateLimitPayload(state.rateLimits),
            // The update replaces the whole permission, so the fields this
            // form does not edit (cache, model-as-judge overrides) are sent
            // back as loaded.
            permission: {
              ...keyAccessPayload(state.access),
              downgradable: token.permission.downgradable,
              model_floor: token.permission.model_floor,
              cache: token.permission.cache,
            },
          }),
        }
      );
      if (!res.ok) {
        const text = await res.text();
        throw new Error(text || `HTTP ${res.status}`);
      }
      const data: ApiKeyView = await res.json();
      onSave(data);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <Modal open onClose={onCancel} title={`Edit ${token.description || token.key_prefix}`} size="xl">
        <div className="space-y-3">
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium" style={{ color: "var(--color-cds-ink-secondary)" }}>
              Editing {token.key_prefix}
            </span>
            <div className="flex items-center gap-2">
              <button
                onClick={onCancel}
                className="text-xs px-3 py-1.5 rounded"
                style={{ color: "var(--color-cds-ink-secondary)" }}
              >
                Cancel
              </button>
              <button
                onClick={handleSave}
                disabled={saving}
                className="btn btn-primary"
              >
                {saving ? "Saving..." : "Save"}
              </button>
            </div>
          </div>

          <div>
            <label
              className="block text-xs font-medium mb-1"
              style={{ color: "var(--color-cds-ink-secondary)" }}
            >
              Description <span style={{ color: "var(--color-cds-ink-muted)" }}>(required, 120 characters max)</span>
            </label>
            <input
              type="text"
              value={state.description}
              onChange={(e) =>
                setState((s) => ({ ...s, description: e.target.value }))
              }
              maxLength={MAX_API_KEY_DESCRIPTION_LENGTH}
              className="w-full rounded-md px-3 py-2 text-sm focus:outline-none"
              style={inputStyle}
            />
          </div>

          <div className="grid grid-cols-2 gap-3">
            <label className="block text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>
              Usage pool
              <select value={state.usagePoolId} onChange={(event) => setState((current) => ({ ...current, usagePoolId: event.target.value }))} className="mt-1 w-full rounded-md px-3 py-2 text-sm" style={inputStyle}>
                <option value="">No usage pool</option>
                {pools.map((pool) => <option key={pool.id} value={pool.id}>{pool.name}</option>)}
              </select>
            </label>
          </div>

          <KeyAccessEditor value={state.access} onChange={(access) => setState((current) => ({ ...current, access }))} routes={routes} connections={connections} />

          <KeyQuotaEditor
            budgets={state.budgets}
            onBudgetsChange={(budgets) => setState((current) => ({ ...current, budgets }))}
            limits={state.rateLimits}
            onLimitsChange={(rateLimits) => setState((current) => ({ ...current, rateLimits }))}
            connections={connections}
            inherited={state.usagePoolId ? poolQuotaPolicies[state.usagePoolId] : undefined}
          />

          {error && (
            <p
              className="text-xs px-3 py-2 rounded"
              style={{ color: "var(--color-cds-status-red-text)", background: "var(--color-cds-status-red-fill)" }}
            >
              {error}
            </p>
          )}
        </div>
    </Modal>
  );
}

export function TokensClient({
  initialKeys,
  initialNextCursor,
  pools,
  connections,
  routes,
  poolQuotaPolicies,
  initialUsagePoolId,
  initialIssueOpen = false,
  initialEditKeyHash,
}: TokensClientProps) {
  // Every row (and every connection-scope or route grant within a row) looks
  // up its pool, connection or route by id; with these potentially numbering
  // in the hundreds, a `.find()` per lookup makes rendering the table O(rows *
  // pools|connections|routes). Index them once per render into `Map`s so each
  // lookup is O(1).
  const poolsById = useMemo(() => new Map(pools.map((p) => [p.id, p])), [pools]);
  const connectionsById = useMemo(() => new Map(connections.map((c) => [c.id, c])), [connections]);
  const routesById = useMemo(() => new Map(routes.map((r) => [r.id, r])), [routes]);
  const connectionName = (id: string): string => connectionsById.get(id)?.name ?? id;
  const { items: keys, setItems: setKeys, loading: pageLoading, setLoading: setPageLoading, nextCursor, applyPage } = usePagination<ApiKeyView>(
    (key) => key.key_hash,
    { items: initialKeys, nextCursor: initialNextCursor, loading: false },
  );
  const [newKey, setNewKey] = useState<ApiKeyView | null>(null);
  const [revoking, setRevoking] = useState<Set<string>>(new Set());
  const [keyPendingRevocation, setKeyPendingRevocation] = useState<ApiKeyView | null>(null);
  const [editingKey, setEditingKey] = useState<string | null>(() => initialKeys.some((key) => key.key_hash === initialEditKeyHash) ? initialEditKeyHash ?? null : null);
  const [error, setError] = useState<string | null>(null);

  /** Resolves to the keys loaded, or null when the fetch failed. */
  async function loadPage(cursor?: string): Promise<ApiKeyView[] | null> {
    setPageLoading(true);
    setError(null);
    try {
      const page = await fetchAdminPage<ApiKeyView>("/api/admin/keys", cursor);
      applyPage(page, cursor);
      setEditingKey(null);
      return page.data;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setPageLoading(false);
    }
  }

  async function handleNextPage() {
    if (!nextCursor) return;
    await loadPage(nextCursor);
  }

  // Revoking the last row of a page leaves the table blank; step back so the
  // user lands on rows instead of on an empty page with a live "Previous".
  async function reloadAfterRemoval() {
    await loadPage();
  }

  function handleIssued(k: ApiKeyView) {
    setNewKey(k);
    setKeys((prev) => [k, ...prev]);
  }

  function handleEditSave(updated: ApiKeyView) {
    setKeys((prev) => prev.map((k) => (k.key_hash === updated.key_hash ? updated : k)));
    setEditingKey(null);
  }

  async function handleRevoke(keyHash: string): Promise<boolean> {
    setRevoking((prev) => new Set(prev).add(keyHash));
    setError(null);
    try {
      const res = await fetch(`/api/admin/keys/${encodeURIComponent(keyHash)}`, {
        method: "DELETE",
      });
      if (!res.ok) {
        const text = await res.text();
        throw new Error(text || `HTTP ${res.status}`);
      }
      await reloadAfterRemoval();
      if (newKey?.key_hash === keyHash) setNewKey(null);
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setRevoking((prev) => {
        const next = new Set(prev);
        next.delete(keyHash);
        return next;
      });
    }
  }

  return (
    <div>
      <div className="mb-6"><PageHeader title="API keys" description="Issue credentials and control model route, connection, and quota access." action={<IssueTokenForm onIssued={handleIssued} pools={pools} connections={connections} routes={routes} poolQuotaPolicies={poolQuotaPolicies} initialUsagePoolId={initialUsagePoolId} initialOpen={initialIssueOpen} />} /></div>
      {error && (
        <div
          className="mb-4 px-4 py-3 rounded-lg text-sm"
          style={{
            background: "var(--color-cds-status-red-fill)",
            border: "1px solid var(--color-cds-status-red-border)",
            color: "var(--color-cds-status-red-text)",
          }}
        >
          <span className="font-medium">Error:</span> {error}
        </div>
      )}

      <Quickstart sampleToken={newKey?.raw_key ?? null} />

      {newKey && (
        <div
          className="mb-6 px-5 py-4 rounded-lg"
          style={{ background: "var(--color-cds-status-green-fill)", border: "1px solid var(--color-cds-status-green-border)" }}
        >
          <div className="flex items-center gap-2 mb-2">
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="#6ee7b7"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <polyline points="20 6 9 17 4 12" />
            </svg>
            <span
              className="text-sm font-medium"
              style={{ color: "var(--color-cds-status-green-text)" }}
            >
              Token issued — copy it now, it cannot be recovered
            </span>
          </div>
          <div className="flex items-center gap-3">
            <code
              className="flex-1 text-sm rounded px-3 py-2 select-all"
              style={{
                background: "var(--color-cds-status-green-fill)",
                color: "var(--color-cds-status-green-text)",
                fontFamily: "'Everett Mono', monospace",
                wordBreak: "break-all",
              }}
            >
              {newKey.raw_key}
            </code>
            <button
              onClick={() => {
                if (newKey.raw_key) navigator.clipboard.writeText(newKey.raw_key);
              }}
              className="text-xs px-3 py-2 rounded transition-colors"
              style={{ background: "var(--color-cds-status-green-border)", color: "var(--color-cds-status-green-text)" }}
            >
              Copy
            </button>
          </div>
          <button
            onClick={() => setNewKey(null)}
            className="mt-2 text-xs"
            style={{ color: "var(--color-cds-ink-muted)" }}
          >
            Dismiss
          </button>
        </div>
      )}

      {keys.length === 0 ? (
        <div
          className="mt-4 px-6 py-12 rounded-lg text-center"
          style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border)" }}
        >
          <p className="text-sm" style={{ color: "var(--color-cds-ink-subtle)" }}>
            No API tokens yet. Issue one above.
          </p>
        </div>
      ) : (
        <div
          className="mt-4 rounded-lg overflow-hidden"
          style={{ border: "1px solid var(--color-cds-border)" }}
        >
          <table className="w-full text-sm">
            <thead>
              <tr
                style={{
                  background: "var(--color-cds-raised)",
                  borderBottom: "1px solid var(--color-cds-border)",
                }}
              >
                <th
                  className="text-left px-5 py-3 font-medium"
                  style={{ color: "var(--color-cds-ink-secondary)" }}
                >
                  Description
                </th>
                <th
                  className="text-left px-5 py-3 font-medium"
                  style={{ color: "var(--color-cds-ink-secondary)" }}
                >
                  Usage Pool
                </th>
                <th
                  className="text-left px-5 py-3 font-medium"
                  style={{ color: "var(--color-cds-ink-secondary)" }}
                >
                  Access scope
                </th>
                <th
                  className="text-left px-5 py-3 font-medium"
                  style={{ color: "var(--color-cds-ink-secondary)" }}
                >
                  Quota limits
                </th>
                <th
                  className="text-left px-5 py-3 font-medium"
                  style={{ color: "var(--color-cds-ink-secondary)" }}
                >
                  Labels
                </th>
                <th
                  className="text-left px-5 py-3 font-medium"
                  style={{ color: "var(--color-cds-ink-secondary)" }}
                >
                  Created
                </th>
                <th
                  className="sticky right-0 bg-cds-raised px-5 py-3 text-right font-medium"
                  style={{ color: "var(--color-cds-ink-secondary)" }}
                >
                  Actions
                </th>
              </tr>
            </thead>
            <tbody>
              {keys.map((k, i) =>
                  <tr
                    key={k.key_hash}
                    role="link"
                    tabIndex={0}
                    onClick={(event) => { if (!(event.target as HTMLElement).closest("a,button,input")) window.location.assign(`/keys/${encodeURIComponent(k.key_hash)}`); }}
                    onKeyDown={(event) => { if (event.key === "Enter") window.location.assign(`/keys/${encodeURIComponent(k.key_hash)}`); }}
                    className="cursor-pointer hover:bg-cds-hover"
                    style={{
                      background: i % 2 === 0 ? "var(--color-cds-elevated)" : "var(--color-cds-raised)",
                      borderBottom:
                        i < keys.length - 1
                          ? "1px solid var(--color-cds-border)"
                          : undefined,
                      opacity: revoking.has(k.key_hash) ? 0.5 : 1,
                      transition: "opacity 0.15s",
                    }}
                  >
                    <td className="px-5 py-3">
                      <Link
                        href={`/keys/${encodeURIComponent(k.key_hash)}`}
                        className="text-sm"
                        title={k.key_prefix}
                        style={{
                          color: k.description ? "var(--color-cds-ink)" : "var(--color-cds-ink-subtle)",
                        }}
                      >
                        {k.description || (
                          <span style={{ fontStyle: "italic" }}>
                            No description
                          </span>
                        )}
                      </Link>
                    </td>
                    <td className="px-5 py-3">
                      {k.usage_pool_id ? (
                        <Link href={`/usage-pools/${encodeURIComponent(k.usage_pool_id)}`} className="text-xs px-1.5 py-0.5 rounded" style={{ background: "var(--color-cds-status-orange-fill)", color: "var(--color-cds-status-orange-text)" }}>
                          {poolsById.get(k.usage_pool_id)?.name ?? k.usage_pool_id}
                        </Link>
                      ) : (
                        <span className="text-xs" style={{ color: "var(--color-cds-ink-muted)" }}>—</span>
                      )}
                    </td>
                    <td className="px-5 py-3" style={{ color: "var(--color-cds-ink-medium)" }}>
                      {keyAccessKind(k.permission) === "all" ? (
                        <span className="text-xs" style={{ color: "var(--color-cds-status-yellow-text)" }}>All models</span>
                      ) : keyAccessKind(k.permission) === "none" ? (
                        <span className="text-xs" style={{ color: "var(--color-cds-status-red-text)" }} title="Restricted with no model routes or connection scopes: this key cannot call any model">No access</span>
                      ) : (
                        <div className="flex flex-col gap-1">
                          {grantedRouteIds(k.permission).length > 0 && (
                            <div className="flex flex-wrap items-center gap-1">
                              {routeGrantLabels(grantedRouteIds(k.permission), routesById).map((route) => (
                                <Link
                                  key={route.id}
                                  href={`/routes/${encodeURIComponent(route.id)}`}
                                  className="text-xs px-1.5 py-0.5 rounded font-mono"
                                  style={route.publicModelId ? { background: "var(--color-cds-status-blue-fill)", color: "var(--color-cds-status-blue-text)" } : { background: "var(--color-cds-status-red-fill)", color: "var(--color-cds-status-red-text)" }}
                                  title={route.id}
                                >
                                  {route.publicModelId ?? "deleted route"}
                                </Link>
                              ))}
                            </div>
                          )}
                          {k.permission.connection_scopes?.map((scope, scopeIndex) => (
                            <div key={`${scope.connection_id}-${scopeIndex}`} className="flex flex-wrap items-center gap-1">
                              <span
                                className="text-xs px-1.5 py-0.5 rounded"
                                style={{ background: "var(--color-cds-status-purple-fill)", color: "var(--color-cds-status-purple-text)" }}
                                title={scope.connection_id}
                              >
                                conn: {connectionName(scope.connection_id)}
                              </span>
                              <span className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>
                                {scope.models?.length ? scope.models.join(", ") : "all direct models"}
                              </span>
                            </div>
                          ))}
                        </div>
                      )}
                    </td>
                    <td className="px-5 py-3">
                      {(() => {
                        const direct = quotaSummary(k.usage_budgets, k.rate_limits.length);
                        const inherited = k.usage_pool_id ? poolQuotaPolicies[k.usage_pool_id] : undefined;
                        const inheritedSummary = inherited
                          ? quotaSummary(inherited.budgets, inherited.rateLimits.filter((limit) => limit.enabled).length)
                          : null;
                        return <div className="flex flex-col gap-1 text-xs"><Link href={`/access?v=usage-budgets&scope=api_key&scope_id=${encodeURIComponent(k.key_hash)}`} style={{ color: direct ? "var(--color-cds-status-green-text)" : "var(--color-cds-ink-subtle)" }}>{direct ? `Key-specific: ${direct}` : "No key-specific limits"}</Link>{k.usage_pool_id && (inheritedSummary ? <Link href={`/usage-pools/${encodeURIComponent(k.usage_pool_id)}`} style={{ color: "var(--color-cds-status-orange-text)" }}>Inherited: {inheritedSummary}</Link> : <Link href={`/usage-pools/${encodeURIComponent(k.usage_pool_id)}`} style={{ color: "var(--color-cds-ink-muted)" }}>Inherited: no shared limits</Link>)}</div>;
                      })()}
                    </td>
                    <td className="px-5 py-3">
                      {Object.keys(k.labels).length > 0 ? (
                        <div className="flex flex-wrap gap-1">
                          {Object.entries(k.labels).map(([lk, lv]) => (
                            <span
                              key={lk}
                              className="text-xs px-1.5 py-0.5 rounded"
                              style={{
                                background: "var(--color-cds-status-green-fill)",
                                color: "var(--color-cds-status-green-text)",
                              }}
                            >
                              {lk}={lv}
                            </span>
                          ))}
                        </div>
                      ) : (
                        <span style={{ color: "var(--color-cds-ink-muted)" }}>—</span>
                      )}
                    </td>
                    <td
                      className="px-5 py-3 text-xs"
                      style={{ color: "var(--color-cds-ink-subtle)" }}
                    >
                      {formatDateTime(k.created_at)}
                    </td>
                    <td className="sticky right-0 bg-inherit px-5 py-3 text-right">
                      <div className="flex items-center justify-end gap-2">
                        <IconButton icon="edit" label="Edit API key" onClick={() => setEditingKey(k.key_hash)} />
                        <IconButton icon="trash" label={revoking.has(k.key_hash) ? "Revoking API key" : "Revoke API key"} tone="danger" onClick={() => setKeyPendingRevocation(k)} disabled={revoking.has(k.key_hash)} />
                      </div>
                    </td>
                  </tr>
              )}
            </tbody>
          </table>
        </div>
      )}

      <InfiniteScroll hasMore={Boolean(nextCursor)} loading={pageLoading} onLoadMore={() => void handleNextPage()} />

      <p className="mt-4 text-xs" style={{ color: "var(--color-cds-ink-muted)" }}>
        {keys.length} token{keys.length !== 1 ? "s" : ""} loaded
      </p>

      {editingKey && (() => { const token = keys.find((item) => item.key_hash === editingKey); return token ? <EditTokenForm token={token} pools={pools} connections={connections} routes={routes} poolQuotaPolicies={poolQuotaPolicies} onSave={handleEditSave} onCancel={() => setEditingKey(null)} /> : null; })()}

      <ConfirmationModal
        open={keyPendingRevocation !== null}
        title="Revoke API key?"
        description={`This immediately disables ${keyPendingRevocation?.description || keyPendingRevocation?.key_prefix || "this API key"} for every client using it.`}
        confirmLabel="Revoke key"
        confirming={keyPendingRevocation ? revoking.has(keyPendingRevocation.key_hash) : false}
        onCancel={() => setKeyPendingRevocation(null)}
        onConfirm={async () => {
          if (keyPendingRevocation && await handleRevoke(keyPendingRevocation.key_hash)) {
            setKeyPendingRevocation(null);
          }
        }}
      />
    </div>
  );
}
