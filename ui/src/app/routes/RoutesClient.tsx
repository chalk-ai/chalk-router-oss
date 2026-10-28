"use client";

import { useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { ConfirmationModal } from "@/components/ConfirmationModal";
import { IconButton, InfiniteScroll, inputStyle, Modal, PageHeader } from "@/components/router/RouterPrimitives";
import { useJudgeValidation } from "@/components/routes/useJudgeValidation";
import { RouteConfigurationEditor } from "@/components/routes/RouteConfigurationEditor";
import { effectiveTarget, routeConfigurationPayload, routeGridError, type RouteGridConfig } from "@/lib/routeGrid";
import { fetchAdminPage, fetchAllAdminPages } from "@/lib/adminPagination";
import { usePagination } from "@/lib/usePagination";

type Diagnostic =
  | "ready"
  | "unconfigured"
  | "disabled"
  | "unreadable"
  | "missing_connection"
  | "unspecified";

interface ModelRoute {
  id: string;
  public_model_id: string;
  connection_id: string;
  upstream_model_id: string;
  published: boolean;
  routing: RouteGridConfig | null;
  diagnostic: Diagnostic;
  created_at: string;
  updated_at: string;
}

interface Connection {
  id: string;
  name: string;
  provider_kind: string;
}

const DIAGNOSTIC_LABELS: Record<Diagnostic, { label: string; bg: string; color: string }> = {
  ready: { label: "ready", bg: "#052e16", color: "#4ade80" },
  unconfigured: { label: "unconfigured", bg: "#3b1a0a", color: "#fdba74" },
  disabled: { label: "disabled", bg: "#1f2937", color: "#9ca3af" },
  unreadable: { label: "unreadable", bg: "#1c0a0a", color: "#fca5a5" },
  missing_connection: { label: "missing connection", bg: "#1c0a0a", color: "#ef4444" },
  unspecified: { label: "unknown", bg: "#1f2937", color: "#6b7280" },
};

interface FormState {
  public_model_id: string;
  connection_id: string;
  upstream_model_id: string;
  published: boolean;
  routing: RouteGridConfig | null;
}

const EMPTY_FORM: FormState = {
  public_model_id: "chalk/",
  connection_id: "",
  upstream_model_id: "",
  published: true,
  routing: null,
};

async function errorText(res: Response): Promise<string> {
  try {
    const data = await res.json();
    return data?.error?.message ?? `${res.status} ${res.statusText}`;
  } catch {
    return `${res.status} ${res.statusText}`;
  }
}

export default function RoutesClient({ embedded = false }: { embedded?: boolean }) {
  const { items: routes, setItems: setRoutes, loading, setLoading, nextCursor, applyPage } = usePagination<ModelRoute>((route) => route.id);
  const [connections, setConnections] = useState<Connection[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [routePendingDeletion, setRoutePendingDeletion] = useState<ModelRoute | null>(null);
  const [deletingRouteId, setDeletingRouteId] = useState<string | null>(null);

  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [form, setForm] = useState<FormState>(EMPTY_FORM);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  /** Resolves to the routes loaded, or null when the fetch failed. */
  async function fetchAll(cursor?: string): Promise<ModelRoute[] | null> {
    setLoading(true);
    try {
      const [routePage, connectionPage] = await Promise.all([
        fetchAdminPage<ModelRoute>("/api/admin/model-routes", cursor),
        fetchAllAdminPages<Connection>("/api/admin/providers"),
      ]);
      applyPage(routePage, cursor);
      setConnections(connectionPage);
      return routePage.data;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void fetchAll(); }, []);

  // `connectionLabel` runs once per route row; index `connections` into a
  // `Map` once per render instead of scanning it with `.find()` for every row.
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

  function connectionLabel(id: string): string {
    const c = connectionsById.get(id);
    return c ? `${c.name} (${c.provider_kind})` : id;
  }

  function openCreate() {
    setEditingId(null);
    setForm({ ...EMPTY_FORM, connection_id: connections[0]?.id ?? "" });
    setSaveError(null);
    setShowForm(true);
  }

  function openEdit(r: ModelRoute) {
    setEditingId(r.id);
    setForm({
      public_model_id: r.public_model_id,
      connection_id: r.connection_id,
      upstream_model_id: r.upstream_model_id,
      published: r.published,
      routing: r.routing ?? null,
    });
    setSaveError(null);
    setShowForm(true);
  }

  function closeForm() {
    setShowForm(false);
    setEditingId(null);
    setForm(EMPTY_FORM);
  }

  const judgeValidation = useJudgeValidation(form.routing?.judge?.backend);

  function formValid(): boolean {
    if (judgeValidation.checking || judgeValidation.error) return false;
    return (
      form.public_model_id.trim().length > 0 &&
      form.public_model_id.trim() !== "chalk/" &&
      (form.routing ? !routeGridError(form.routing) : !!form.connection_id.trim() && !!form.upstream_model_id.trim())
    );
  }

  async function handleSave() {
    if (!formValid() || saving) return;
    setSaving(true);
    setSaveError(null);
    try {
      if (editingId) {
        const res = await fetch(`/api/admin/model-routes/${encodeURIComponent(editingId)}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            ...routeConfigurationPayload(form),
            published: form.published,
          }),
        });
        if (!res.ok) throw new Error(await errorText(res));
        await resetToFirstPage();
      } else {
        const res = await fetch("/api/admin/model-routes", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            public_model_id: form.public_model_id.trim(),
            ...routeConfigurationPayload(form),
            published: form.published,
          }),
        });
        if (!res.ok) throw new Error(await errorText(res));
        await resetToFirstPage();
      }
      closeForm();
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  async function handleDelete(r: ModelRoute): Promise<boolean> {
    setDeletingRouteId(r.id);
    try {
      const res = await fetch(`/api/admin/model-routes/${encodeURIComponent(r.id)}`, { method: "DELETE" });
      if (!res.ok) throw new Error(await errorText(res));
      const body = await res.json();
      if (Array.isArray(body.warnings) && body.warnings.length > 0) {
        alert(`Deleted with warnings:\n${body.warnings.join("\n")}`);
      }
      await reloadAfterRemoval();
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setDeletingRouteId(null);
    }
  }

  const saveDisabled = !formValid() || saving;

  return (
    <div>
      <div className="mb-6"><PageHeader title="Routes" description={<>Map public <code>chalk/*</code> aliases to models, judge outcomes and fallback chains.</>} action={<button onClick={openCreate} className="rounded-lg px-4 py-2 text-sm font-medium" style={{ background: "#218E61", color: "white" }}>New route</button>} /></div>

      {error && (
        <div className="mb-6 px-4 py-3 rounded-lg text-sm" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>
          <span className="font-medium">Error:</span> {error}
        </div>
      )}

      {loading ? (
        <p className="text-sm" style={{ color: "#6b7280" }}>Loading...</p>
      ) : (
        <>
          {routes.length > 0 ? (
            <div className="rounded-lg overflow-hidden mb-6" style={{ border: "1px solid #1f2937" }}>
              <table className="w-full text-sm">
                <thead>
                  <tr style={{ background: "#111827", borderBottom: "1px solid #1f2937" }}>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Public model</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Connection</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Upstream model</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Published</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Diagnostic</th>
                    <th className="sticky right-0 bg-slate-900 px-5 py-3 text-right font-medium" style={{ color: "#9ca3af" }}>Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {routes.map((r, i) => {
                    const diag = DIAGNOSTIC_LABELS[r.diagnostic] ?? DIAGNOSTIC_LABELS.unspecified;
                    return (
                      <tr key={r.id} role="link" tabIndex={0} onClick={(event) => { if (!(event.target as HTMLElement).closest("a,button,input")) window.location.assign(`/routes/${encodeURIComponent(r.id)}`); }} onKeyDown={(event) => { if (event.key === "Enter") window.location.assign(`/routes/${encodeURIComponent(r.id)}`); }} className="cursor-pointer hover:bg-slate-800/70" style={{ background: i % 2 === 0 ? "#0f172a" : "#111827", borderBottom: i < routes.length - 1 ? "1px solid #1f2937" : undefined }}>
                        <td className="px-5 py-3">
                          <Link href={`/routes/${encodeURIComponent(r.id)}`} className="inline-block font-mono text-xs px-2 py-0.5 rounded hover:underline" style={{ background: "#052e16", color: "#70CD96" }}>{r.public_model_id}</Link>
                        </td>
                        <td className="px-5 py-3" style={{ color: "#d1d5db" }}>{connectionLabel(effectiveTarget(r).connection_id)}</td>
                        <td className="px-5 py-3">
                          <code className="font-mono text-xs px-2 py-0.5 rounded" style={{ background: "#0f172a", color: "#93c5fd" }}>{effectiveTarget(r).upstream_model_id}</code>{r.routing && <span className="ml-2 text-xs text-slate-400">Custom routing</span>}
                        </td>
                        <td className="px-5 py-3">
                          {r.published ? (
                            <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#052e16", color: "#4ade80" }}>published</span>
                          ) : (
                            <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#1f2937", color: "#6b7280" }}>draft</span>
                          )}
                        </td>
                        <td className="px-5 py-3">
                          <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: diag.bg, color: diag.color }}>{diag.label}</span>
                        </td>
                        <td className="sticky right-0 bg-inherit px-5 py-3 text-right">
                          <div className="flex items-center justify-end gap-2">
                            <IconButton icon="edit" label="Edit route" onClick={() => openEdit(r)} />
                            <IconButton icon="trash" label="Delete route" tone="danger" onClick={() => setRoutePendingDeletion(r)} disabled={deletingRouteId === r.id} />
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          ) : (
            !showForm && (
              <div className="px-6 py-12 rounded-lg text-center mb-6" style={{ background: "#111827", border: "1px solid #1f2937" }}>
                <p className="text-sm" style={{ color: "#6b7280" }}>No model routes yet.</p>
              </div>
            )
          )}

          <InfiniteScroll hasMore={Boolean(nextCursor)} loading={loading} onLoadMore={() => void handleNextPage()} />

          <Modal size="xl" open={showForm} onClose={closeForm} title={editingId ? "Edit route" : "New route"}>
            <div>

              {saveError && (
                <div className="mb-4 px-3 py-2 rounded-lg text-xs" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>
                  {saveError}
                </div>
              )}

              {connections.length === 0 && (
                <div className="mb-4 px-3 py-2 rounded-lg text-xs" style={{ background: "#3b1a0a", border: "1px solid #7c4a12", color: "#fdba74" }}>
                  No provider connections exist yet. Create one on the Providers page first.
                </div>
              )}

              <div className="space-y-4">
                <div>
                  <label htmlFor="route-public-model" className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>Public model id (must start with chalk/)</label>
                  <input
                    id="route-public-model"
                    type="text"
                    value={form.public_model_id}
                    disabled={editingId != null}
                    onChange={(e) => setForm((f) => ({ ...f, public_model_id: e.target.value }))}
                    placeholder="chalk/my-model"
                    className="w-full rounded-md px-3 py-2 text-sm font-mono focus:outline-none disabled:opacity-60"
                    style={inputStyle}
                  />
                </div>
                <RouteConfigurationEditor judgeValidation={judgeValidation} value={form} connections={connections} onChange={configuration => setForm(f => ({ ...f, ...configuration, routing: configuration.routing ?? null }))} />
                <label className="flex items-center gap-2 text-sm" style={{ color: "#9ca3af" }}>
                  <input type="checkbox" checked={form.published} onChange={(e) => setForm((f) => ({ ...f, published: e.target.checked }))} />
                  Published (listed in /v1/models)
                </label>
                <div className="flex justify-end">
                  <button onClick={handleSave} disabled={saveDisabled} className="px-5 py-2 rounded-lg text-sm font-medium transition-colors disabled:opacity-50" style={{ background: "#218E61", color: "white" }}>
                    {saving ? "Saving..." : editingId ? "Save changes" : "Create route"}
                  </button>
                </div>
              </div>
            </div>
          </Modal>
        </>
      )}
      <ConfirmationModal
        open={routePendingDeletion !== null}
        title="Delete model route?"
        description={`This removes ${routePendingDeletion?.public_model_id ?? "this model route"} from the router.`}
        confirmLabel="Delete route"
        confirming={routePendingDeletion?.id === deletingRouteId}
        onCancel={() => setRoutePendingDeletion(null)}
        onConfirm={async () => {
          if (routePendingDeletion && await handleDelete(routePendingDeletion)) {
            setRoutePendingDeletion(null);
          }
        }}
      />
    </div>
  );
}
