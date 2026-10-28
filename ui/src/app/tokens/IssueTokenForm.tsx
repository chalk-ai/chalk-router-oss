"use client";

import { useState } from "react";
import {
  apiKeyDescriptionError,
  MAX_API_KEY_DESCRIPTION_LENGTH,
  normalizeApiKeyDescription,
} from "@/lib/apiKeyDescription";
import { KeyQuotaEditor, budgetPayload, rateLimitPayload, validateQuota, type InheritedQuotaPolicies, type KeyBudgetDraft, type KeyRateLimitDraft } from "./KeyQuotaEditor";
import { Modal } from "@/components/router/RouterPrimitives";
import {
  EMPTY_KEY_ACCESS_DRAFT,
  keyAccessError,
  keyAccessPayload,
  type KeyAccessDraft,
  type KeyAccessJson,
  type KeyAccessPayload,
  type RouteOption,
} from "@/lib/keyAccess";
import { KeyAccessEditor } from "./KeyAccessEditor";

interface LabelRow {
  id: number;
  key: string;
  value: string;
}

interface IssuedKey {
  key_hash: string;
  key_prefix: string;
  raw_key?: string;
  description: string;
  created_at: string;
  permission: KeyAccessJson & {
    downgradable?: boolean | null;
    model_floor?: string | null;
  };
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

interface IssueTokenFormProps {
  onIssued: (key: IssuedKey) => void;
  pools: PoolOption[];
  connections: ConnectionOption[];
  routes: RouteOption[];
  poolQuotaPolicies: Record<string, InheritedQuotaPolicies>;
  initialUsagePoolId?: string;
  initialOpen?: boolean;
}

export function IssueTokenForm({ onIssued, pools, connections, routes, poolQuotaPolicies, initialUsagePoolId, initialOpen = false }: IssueTokenFormProps) {
  const [open, setOpen] = useState(initialOpen || Boolean(initialUsagePoolId));
  const [description, setDescription] = useState("");
  const [access, setAccess] = useState<KeyAccessDraft>(EMPTY_KEY_ACCESS_DRAFT);
  const [budgets, setBudgets] = useState<KeyBudgetDraft[]>([]);
  const [rateLimits, setRateLimits] = useState<KeyRateLimitDraft[]>([]);
  const [usagePoolId, setUsagePoolId] = useState(initialUsagePoolId ?? "");
  const [labels, setLabels] = useState<LabelRow[]>([]);
  const [nextId, setNextId] = useState(0);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function addLabel() {
    setLabels((prev) => [...prev, { id: nextId, key: "", value: "" }]);
    setNextId((n) => n + 1);
  }

  function removeLabel(id: number) {
    setLabels((prev) => prev.filter((l) => l.id !== id));
  }

  function updateLabel(id: number, field: "key" | "value", val: string) {
    setLabels((prev) =>
      prev.map((l) => (l.id === id ? { ...l, [field]: val } : l))
    );
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    setSubmitting(true);
    setError(null);

    const descriptionError = apiKeyDescriptionError(description);
    if (descriptionError) {
      setError(descriptionError);
      setSubmitting(false);
      return;
    }

    const accessError = keyAccessError(access);
    if (accessError) {
      setError(accessError);
      setSubmitting(false);
      return;
    }
    const quotaError = validateQuota(budgets, rateLimits);
    if (quotaError) {
      setError(quotaError);
      setSubmitting(false);
      return;
    }
    // "All models" sends no permission at all; "Restricted" always sends
    // `routes` (possibly empty) so the key cannot widen to every model.
    const permission: KeyAccessPayload = keyAccessPayload(access);

    const labelsMap: Record<string, string> = {};
    for (const l of labels) {
      if (l.key.trim()) labelsMap[l.key.trim()] = l.value;
    }

    const body: {
      description: string;
      permission?: typeof permission;
      labels?: Record<string, string>;
      usage_budgets?: { token_limit: number; period: "daily" | "weekly" | "monthly"; enabled: boolean }[];
      usage_pool_id?: string;
      rate_limits?: {
        connection_id?: string;
        models: string[];
        requests_per_minute?: number;
        request_burst?: number;
        tokens_per_minute?: number;
        token_burst?: number;
        concurrent_requests?: number;
        enabled: boolean;
      }[];
    } = {
      description: normalizeApiKeyDescription(description),
    };
    if (Object.keys(permission).length > 0) body.permission = permission;
    if (Object.keys(labelsMap).length > 0) body.labels = labelsMap;
    if (budgets.length > 0) body.usage_budgets = budgetPayload(budgets);
    if (usagePoolId) body.usage_pool_id = usagePoolId;
    const directRateLimits = rateLimitPayload(rateLimits);
    if (directRateLimits.length > 0) body.rate_limits = directRateLimits;

    try {
      const res = await fetch("/api/admin/keys", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        const text = await res.text();
        throw new Error(text || `HTTP ${res.status}`);
      }
      const data: IssuedKey = await res.json();
      onIssued(data);
      setOpen(false);
      setDescription("");
      setAccess(EMPTY_KEY_ACCESS_DRAFT);
      setBudgets([]);
      setRateLimits([]);
      setUsagePoolId("");
      setLabels([]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  }

  if (!open) {
    return (
      <button
        onClick={() => setOpen(true)}
        className="flex items-center gap-2 px-4 py-2 rounded-lg text-sm font-medium transition-colors"
        style={{
          background: "#218E61",
          color: "white",
        }}
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
          <path d="M5 12h14M12 5l7 7-7 7" />
        </svg>
        Issue Token
      </button>
    );
  }

  return (
    <Modal open={open} onClose={() => { setOpen(false); setError(null); }} title="Issue new API key" size="xl">
      <form onSubmit={handleSubmit} className="space-y-4">
        <div className="grid grid-cols-3 gap-4">
          <div className="col-span-2">
            <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>
              Description <span style={{ color: "#4b5563" }}>(required, 120 characters max)</span>
            </label>
            <input
              type="text"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="e.g. Production chatbot, Andrew's dev key"
              required
              maxLength={MAX_API_KEY_DESCRIPTION_LENGTH}
              className="w-full rounded-md px-3 py-2 text-sm focus:outline-none"
              style={{
                background: "#1f2937",
                border: "1px solid #374151",
                color: "#e5e7eb",
              }}
            />
          </div>
          <div>
            <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>
              Usage Pool <span style={{ color: "#4b5563" }}>(optional)</span>
            </label>
            <select
              value={usagePoolId}
              onChange={(e) => setUsagePoolId(e.target.value)}
              className="w-full rounded-md px-3 py-2 text-sm focus:outline-none"
              style={{
                background: "#1f2937",
                border: "1px solid #374151",
                color: "#e5e7eb",
              }}
            >
              <option value="">No usage pool</option>
              {pools.map((t) => (
                <option key={t.id} value={t.id}>{t.name}</option>
              ))}
            </select>
          </div>
        </div>

        <KeyAccessEditor value={access} onChange={setAccess} routes={routes} connections={connections} />

        <KeyQuotaEditor
          budgets={budgets}
          onBudgetsChange={setBudgets}
          limits={rateLimits}
          onLimitsChange={setRateLimits}
          connections={connections}
          inherited={usagePoolId ? poolQuotaPolicies[usagePoolId] : undefined}
        />

        <div>
          <div className="flex items-center justify-between mb-2">
            <label className="text-xs font-medium" style={{ color: "#9ca3af" }}>
              Labels <span style={{ color: "#4b5563" }}>(optional, immutable)</span>
            </label>
            <button
              type="button"
              onClick={addLabel}
              className="text-xs px-2 py-1 rounded transition-colors"
              style={{ color: "#70CD96", background: "#052e16" }}
            >
              + Add label
            </button>
          </div>
          {labels.length > 0 && (
            <div className="space-y-2">
              {labels.map((l) => (
                <div key={l.id} className="flex items-center gap-2">
                  <input
                    type="text"
                    value={l.key}
                    onChange={(e) => updateLabel(l.id, "key", e.target.value)}
                    placeholder="key"
                    className="flex-1 rounded-md px-3 py-1.5 text-sm focus:outline-none"
                    style={{
                      background: "#1f2937",
                      border: "1px solid #374151",
                      color: "#e5e7eb",
                    }}
                  />
                  <span style={{ color: "#4b5563" }}>:</span>
                  <input
                    type="text"
                    value={l.value}
                    onChange={(e) => updateLabel(l.id, "value", e.target.value)}
                    placeholder="value"
                    className="flex-1 rounded-md px-3 py-1.5 text-sm focus:outline-none"
                    style={{
                      background: "#1f2937",
                      border: "1px solid #374151",
                      color: "#e5e7eb",
                    }}
                  />
                  <button
                    type="button"
                    onClick={() => removeLabel(l.id)}
                    className="px-2 py-1.5 rounded text-xs transition-colors"
                    style={{ color: "#ef4444", background: "#1c0a0a" }}
                  >
                    ×
                  </button>
                </div>
              ))}
            </div>
          )}
          <p className="mt-2 text-xs" style={{ color: "#6b7280" }}>
            Labels are fixed when the token is issued. Create a replacement token to change them.
          </p>
        </div>

        {error && (
          <p className="text-xs px-3 py-2 rounded" style={{ color: "#fca5a5", background: "#1c0a0a" }}>
            {error}
          </p>
        )}

        <div className="flex justify-end">
          <button
            type="submit"
            disabled={submitting}
            className="px-5 py-2 rounded-lg text-sm font-medium transition-colors disabled:opacity-50"
            style={{ background: "#218E61", color: "white" }}
          >
            {submitting ? "Issuing..." : "Issue Token"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
