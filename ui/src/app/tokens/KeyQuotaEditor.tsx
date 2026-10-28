"use client";

import { useState } from "react";
import { inputStyle } from "@/components/router/RouterPrimitives";
import { useConnectionModels } from "@/lib/useConnectionModels";

export interface ConnectionOption { id: string; name: string; provider_kind?: string; }
export interface KeyBudgetDraft { id: number; period: "daily" | "weekly" | "monthly"; tokenLimit: string; enabled: boolean; }
export type RateLimitType = "tokens_per_minute" | "requests_per_minute" | "concurrent_requests";
export interface KeyRateLimitDraft { id: number; connectionId: string; modelAccess: "all" | "specific"; models: string[]; limitType: RateLimitType; limitValue: string; burstValue: string; enabled: boolean; }
export interface ApiKeyRateLimit { connection_id?: string | null; models?: string[]; requests_per_minute?: number | null; request_burst?: number | null; tokens_per_minute?: number | null; token_burst?: number | null; concurrent_requests?: number | null; enabled?: boolean | null; }
export interface InheritedQuotaPolicies {
  poolName: string;
  budgets: { token_limit: number; period: "daily" | "weekly" | "monthly"; enabled: boolean }[];
  rateLimits: { id: string; limit_type: "tokens_per_minute" | "requests_per_minute" | "concurrent_requests"; limit_value: number; burst_value?: number | null; connection_id?: string | null; models: string[]; enabled: boolean }[];
}


export function emptyBudget(id: number): KeyBudgetDraft { return { id, period: "daily", tokenLimit: "", enabled: true }; }
export function emptyRateLimit(id: number): KeyRateLimitDraft { return { id, connectionId: "", modelAccess: "all", models: [], limitType: "tokens_per_minute", limitValue: "", burstValue: "", enabled: true }; }
export function budgetPayload(drafts: KeyBudgetDraft[]) { return drafts.map((draft) => ({ token_limit: Number(draft.tokenLimit), period: draft.period, enabled: draft.enabled })); }
export function rateLimitPayload(drafts: KeyRateLimitDraft[]) {
  const number = (value: string) => value.trim() ? Number(value.trim()) : undefined;
  return drafts.map((draft) => {
    const value = number(draft.limitValue);
    const burst = number(draft.burstValue);
    const connectionId = draft.connectionId.trim() || undefined;
    const models = connectionId ? [...new Set(draft.models.map((model) => model.trim()).filter(Boolean))].sort() : [];
    const target = { connection_id: connectionId, models, enabled: draft.enabled };
    switch (draft.limitType) {
      case "requests_per_minute": return { ...target, requests_per_minute: value, request_burst: burst, tokens_per_minute: undefined, token_burst: undefined, concurrent_requests: undefined };
      case "concurrent_requests": return { ...target, requests_per_minute: undefined, request_burst: undefined, tokens_per_minute: undefined, token_burst: undefined, concurrent_requests: value };
      default: return { ...target, requests_per_minute: undefined, request_burst: undefined, tokens_per_minute: value, token_burst: burst, concurrent_requests: undefined };
    }
  });
}
export function rateLimitDraftsFromKey(limits: ApiKeyRateLimit[]): KeyRateLimitDraft[] {
  return limits.flatMap((limit) => {
    const target = { connectionId: limit.connection_id ?? "", modelAccess: limit.models?.length ? "specific" as const : "all" as const, models: limit.models ?? [], enabled: limit.enabled ?? true };
    return [
      limit.requests_per_minute != null ? { ...target, limitType: "requests_per_minute" as const, limitValue: limit.requests_per_minute.toString(), burstValue: limit.request_burst?.toString() ?? "" } : null,
      limit.tokens_per_minute != null ? { ...target, limitType: "tokens_per_minute" as const, limitValue: limit.tokens_per_minute.toString(), burstValue: limit.token_burst?.toString() ?? "" } : null,
      limit.concurrent_requests != null ? { ...target, limitType: "concurrent_requests" as const, limitValue: limit.concurrent_requests.toString(), burstValue: "" } : null,
    ].filter((draft): draft is Omit<KeyRateLimitDraft, "id"> => draft !== null);
  }).map((draft, id) => ({ ...draft, id }));
}
export function rateLimitIdentityKey(limitType: RateLimitType, connectionId: string, selectedModels: string[]): string {
  const normalizedConnection = connectionId.trim();
  const models = normalizedConnection ? [...new Set(selectedModels.map((model) => model.trim()).filter(Boolean))].sort() : [];
  return JSON.stringify([limitType, normalizedConnection, models]);
}
export function validateQuota(budgets: KeyBudgetDraft[], drafts: KeyRateLimitDraft[]): string | null {
  const periods = new Set<string>();
  for (const budget of budgets) {
    if (!budget.tokenLimit.trim() || !/^\d+$/.test(budget.tokenLimit.trim()) || !Number.isSafeInteger(Number(budget.tokenLimit)) || Number(budget.tokenLimit) <= 0) return "Each budget must have a positive safe-integer token limit.";
    if (periods.has(budget.period)) return "Only one token budget is allowed for each period.";
    periods.add(budget.period);
  }
  const identities = new Set<string>();
  for (const draft of drafts) {
    const values = [draft.limitValue, draft.burstValue];
    if (!draft.limitValue.trim()) return "Each rate limit needs a limit value.";
    if (values.some((value) => value.trim() && (!/^\d+$/.test(value.trim()) || !Number.isSafeInteger(Number(value)) || Number(value) <= 0))) return "Rate limits and burst capacities must be positive safe integers.";
    if (draft.limitType === "concurrent_requests" && draft.burstValue.trim()) return "Concurrent-request limits do not support burst capacity.";
    if (!draft.connectionId && (draft.models.length || draft.modelAccess === "specific")) return "Model selectors require a connection-scoped rate limit.";
    if (draft.modelAccess === "specific" && draft.models.length === 0) return "Choose at least one model for every specific-model rate limit.";
    const identity = rateLimitIdentityKey(draft.limitType, draft.connectionId, draft.models);
    if (identities.has(identity)) return "This target already has this rate limit. Edit the existing rate limit instead.";
    identities.add(identity);
  }
  return null;
}

/** Shared connection + public-model picker for standalone and direct rate limits. */
export function ConnectionModelPicker({ connectionId, modelAccess, models, connections, onChange, allowGlobal = true }: { connectionId: string; modelAccess: "all" | "specific"; models: string[]; connections: ConnectionOption[]; onChange: (value: { connectionId: string; modelAccess: "all" | "specific"; models: string[] }) => void; allowGlobal?: boolean; }) {
  const { modelsByConnection, modelsLoading, loadModels } = useConnectionModels();
  const specific = modelAccess === "specific";
  return <div className="grid gap-2 md:grid-cols-[minmax(0,1fr)_minmax(0,1.45fr)]">
    <label className="block text-xs font-medium" style={{ color: "#9ca3af" }}>Connection<select value={connectionId} onChange={(event) => onChange({ connectionId: event.target.value, modelAccess: "all", models: [] })} className="mt-1.5 h-10 w-full rounded-md px-3 text-sm focus:outline-none" style={inputStyle}>{allowGlobal && <option value="">All connections</option>}{connections.map((connection) => <option key={connection.id} value={connection.id}>{connection.name}</option>)}</select></label>
    {connectionId ? <div><span className="block text-xs font-medium" style={{ color: "#9ca3af" }}>Models</span><div className="mt-1.5 grid h-10 grid-cols-2 overflow-hidden rounded-md" style={{ border: "1px solid #374151" }}><button type="button" onClick={() => onChange({ connectionId, modelAccess: "all", models: [] })} className="h-full px-3 text-sm" style={specific ? { color: "#9ca3af" } : { background: "#218E61", color: "white" }}>All models</button><button type="button" onClick={() => { void loadModels(connectionId); onChange({ connectionId, modelAccess: "specific", models }); }} className="h-full px-3 text-sm" style={specific ? { background: "#218E61", color: "white" } : { color: "#9ca3af" }}>Specific models</button></div>{specific && <ModelChecklist models={modelsByConnection[connectionId] ?? []} selected={models} loading={Boolean(modelsLoading[connectionId])} onChange={(next) => onChange({ connectionId, modelAccess: "specific", models: next })}/>} {!specific && modelsByConnection[connectionId] !== undefined && <p className="mt-1 text-xs" style={{ color: "#6b7280" }}>All models on this connection share this limit.</p>}</div> : <p className="self-end pb-1 text-xs" style={{ color: "#6b7280" }}>Applies once per request across all connections.</p>}
  </div>;
}

function ModelChecklist({ models, selected, loading, onChange }: { models: string[]; selected: string[]; loading: boolean; onChange: (models: string[]) => void }) {
  return <div className="mt-2 max-h-36 overflow-y-auto rounded p-1" style={{ background: "#1f2937", border: "1px solid #374151" }}>{loading ? <p className="px-2 py-1 text-xs" style={{ color: "#a1a1aa" }}>Loading models…</p> : models.length ? models.map((model) => <label key={model} className="flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-xs hover:bg-white/5" style={{ color: "#e5e7eb" }}><input type="checkbox" checked={selected.includes(model)} onChange={(event) => onChange(event.target.checked ? [...selected, model] : selected.filter((item) => item !== model))} />{model}</label>) : <p className="px-2 py-1 text-xs" style={{ color: "#a1a1aa" }}>No public models are available for this connection.</p>}</div>;
}

export function KeyQuotaEditor({ budgets, onBudgetsChange, limits, onLimitsChange, connections, inherited, title = "Quota limits", description = "Direct budgets and rate limits are additive. They do not change which connections or models this token can access." }: { budgets: KeyBudgetDraft[]; onBudgetsChange: (budgets: KeyBudgetDraft[]) => void; limits: KeyRateLimitDraft[]; onLimitsChange: (limits: KeyRateLimitDraft[]) => void; connections: ConnectionOption[]; inherited?: InheritedQuotaPolicies; title?: string; description?: string; }) {
  const [nextBudgetId, setNextBudgetId] = useState(() => Math.max(-1, ...budgets.map((budget) => budget.id)) + 1);
  const [nextLimitId, setNextLimitId] = useState(() => Math.max(-1, ...limits.map((limit) => limit.id)) + 1);
  const updateBudget = (id: number, patch: Partial<KeyBudgetDraft>) => onBudgetsChange(budgets.map((budget) => budget.id === id ? { ...budget, ...patch } : budget));
  const updateLimit = (id: number, patch: Partial<KeyRateLimitDraft>) => onLimitsChange(limits.map((limit) => limit.id === id ? { ...limit, ...patch } : limit));
  return <section className="rounded-lg p-4" style={{ background: "#0f172a", border: "1px solid #334155" }}>
    <div className="mb-3"><h4 className="text-xs font-medium" style={{ color: "#d1d5db" }}>{title}</h4><p className="mt-1 text-xs" style={{ color: "#6b7280" }}>{description}</p></div>
    <div className="space-y-2">{budgets.map((budget) => <div key={budget.id} className="grid grid-cols-[1fr_1fr_auto_auto] items-end gap-2 rounded p-3" style={{ background: "#111827", border: "1px solid #334155" }}><label className="text-xs" style={{ color: "#9ca3af" }}>Period<select value={budget.period} onChange={(event) => updateBudget(budget.id, { period: event.target.value as KeyBudgetDraft["period"] })} className="mt-1 w-full rounded px-2 py-1.5 text-sm" style={inputStyle}><option value="daily">Daily</option><option value="weekly">Weekly</option><option value="monthly">Monthly</option></select></label><label className="text-xs" style={{ color: "#9ca3af" }}>Token limit<input value={budget.tokenLimit} onChange={(event) => updateBudget(budget.id, { tokenLimit: event.target.value })} inputMode="numeric" className="mt-1 w-full rounded px-2 py-1.5 text-sm" style={inputStyle}/></label><label className="flex gap-2 pb-2 text-xs" style={{ color: "#d1d5db" }}><input type="checkbox" checked={budget.enabled} onChange={(event) => updateBudget(budget.id, { enabled: event.target.checked })}/>Enabled</label><button type="button" onClick={() => onBudgetsChange(budgets.filter((item) => item.id !== budget.id))} className="pb-2 text-xs" style={{ color: "#fca5a5" }}>Remove</button></div>)}</div>
    <button type="button" onClick={() => { onBudgetsChange([...budgets, emptyBudget(nextBudgetId)]); setNextBudgetId((id) => id + 1); }} className="mt-3 rounded px-3 py-2 text-xs" style={{ border: "1px solid #374151", color: "#70CD96" }}>+ Add token budget</button>
    <div className="mt-5 space-y-3">{limits.map((limit) => <RateLimitRow key={limit.id} limit={limit} connections={connections} onChange={(patch) => updateLimit(limit.id, patch)} onRemove={() => onLimitsChange(limits.filter((item) => item.id !== limit.id))}/>)}</div>
    <button type="button" onClick={() => { onLimitsChange([...limits, emptyRateLimit(nextLimitId)]); setNextLimitId((id) => id + 1); }} className="mt-4 rounded px-3 py-2 text-xs" style={{ border: "1px solid #374151", color: "#70CD96" }}>+ Add rate limit</button>
    {inherited && <InheritedLimits policies={inherited} connections={connections} />}
  </section>;
}

function RateLimitRow({ limit, connections, onChange, onRemove }: { limit: KeyRateLimitDraft; connections: ConnectionOption[]; onChange: (patch: Partial<KeyRateLimitDraft>) => void; onRemove: () => void }) {
  const supportsBurst = limit.limitType !== "concurrent_requests";
  return <div className="rounded-md p-3" style={{ background: "#111827", border: "1px solid #334155" }}><div className="mb-3 flex items-center justify-between"><span className="text-xs font-medium" style={{ color: "#d1d5db" }}>Rate limit</span><button type="button" onClick={onRemove} className="text-xs" style={{ color: "#fca5a5" }}>Remove</button></div><ConnectionModelPicker connectionId={limit.connectionId} modelAccess={limit.modelAccess} models={limit.models} connections={connections} onChange={({ connectionId, modelAccess, models }) => onChange({ connectionId, modelAccess, models })}/><div className="mt-3 grid gap-2 md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,1fr)_auto]"><label className="text-xs" style={{ color: "#9ca3af" }}>Rate limit type<select value={limit.limitType} onChange={(event) => onChange({ limitType: event.target.value as RateLimitType, burstValue: event.target.value === "concurrent_requests" ? "" : limit.burstValue })} className="mt-1 w-full rounded px-2 py-1.5 text-sm" style={inputStyle}><option value="tokens_per_minute">Tokens / min</option><option value="requests_per_minute">Requests / min</option><option value="concurrent_requests">Concurrent requests</option></select></label><label className="text-xs" style={{ color: "#9ca3af" }}>Limit<input value={limit.limitValue} onChange={(event) => onChange({ limitValue: event.target.value })} inputMode="numeric" placeholder="e.g. 100000" className="mt-1 w-full rounded px-2 py-1.5 text-sm" style={inputStyle}/></label><label className="text-xs" style={{ color: "#9ca3af" }}>Burst capacity <span style={{ color: "#4b5563" }}>(optional)</span><input value={limit.burstValue} onChange={(event) => onChange({ burstValue: event.target.value })} inputMode="numeric" disabled={!supportsBurst} placeholder={supportsBurst ? "defaults to limit" : "not applicable"} className="mt-1 w-full rounded px-2 py-1.5 text-sm disabled:opacity-50" style={inputStyle}/></label><label className="flex items-end gap-2 pb-2 text-xs" style={{ color: "#d1d5db" }}><input type="checkbox" checked={limit.enabled} onChange={(event) => onChange({ enabled: event.target.checked })}/>Enabled</label></div></div>;
}

function InheritedLimits({ policies, connections }: { policies: InheritedQuotaPolicies; connections: ConnectionOption[] }) {
  const rateLabel = (limit: InheritedQuotaPolicies["rateLimits"][number]) => `${limit.limit_value.toLocaleString()} ${limit.limit_type === "tokens_per_minute" ? "tokens / min" : limit.limit_type === "requests_per_minute" ? "requests / min" : "concurrent requests"}`;
  return <div className="mt-5 rounded-md p-3" style={{ background: "#172554", border: "1px solid #1d4ed8" }}><p className="text-xs font-medium" style={{ color: "#bfdbfe" }}>Inherited from usage pool: {policies.poolName}</p><p className="mt-1 text-xs" style={{ color: "#bfdbfe" }}>These shared policies are read-only here. Key-specific limits above are additive.</p><div className="mt-3 grid gap-2 md:grid-cols-2">{policies.budgets.map((budget) => <div key={`${budget.period}-${budget.token_limit}`} className="rounded px-2 py-1.5 text-xs" style={{ background: "#0f1d40", color: budget.enabled ? "#dbeafe" : "#93a4bf" }}>Token budget · {budget.token_limit.toLocaleString()} / {budget.period === "daily" ? "day" : budget.period === "weekly" ? "week" : "month"}{budget.enabled ? "" : " · Disabled"}</div>)}{policies.rateLimits.map((limit) => <div key={limit.id} className="rounded px-2 py-1.5 text-xs" style={{ background: "#0f1d40", color: limit.enabled ? "#dbeafe" : "#93a4bf" }}>Rate limit · {rateLabel(limit)}{limit.burst_value != null && limit.burst_value !== limit.limit_value ? ` · burst ${limit.burst_value.toLocaleString()}` : ""}{limit.connection_id ? ` · ${connections.find((connection) => connection.id === limit.connection_id)?.name ?? limit.connection_id}` : " · all connections"}{limit.models.length ? ` · ${limit.models.join(", ")}` : " · all models"}{limit.enabled ? "" : " · Disabled"}</div>)}{policies.budgets.length === 0 && policies.rateLimits.length === 0 && <p className="text-xs" style={{ color: "#bfdbfe" }}>This usage pool has no shared limits.</p>}</div></div>;
}
