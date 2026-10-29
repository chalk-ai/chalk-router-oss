"use client";

import { useEffect, useMemo, useState } from "react";
import { inputStyle } from "@/components/router/RouterPrimitives";
import { useConnectionModels } from "@/lib/useConnectionModels";
import {
  connectionScopeModelError,
  directModelIds,
  keyAccessDraftGrantsNothing,
  type ConnectionScopeDraft,
  type KeyAccessDraft,
  type RouteOption,
} from "@/lib/keyAccess";

interface ConnectionOption {
  id: string;
  name: string;
}

/** Show a filter box above the route list once it is longer than this. */
const ROUTE_FILTER_THRESHOLD = 8;

/**
 * Access editor shared by the issue and edit forms: "All models", or
 * "Restricted" to the union of model routes (by route id) and connection
 * scopes (direct models on a connection). Validation and the request payload
 * live in `lib/keyAccess.ts`.
 */
export function KeyAccessEditor({
  value,
  onChange,
  routes,
  connections,
}: {
  value: KeyAccessDraft;
  onChange: (value: KeyAccessDraft) => void;
  routes: RouteOption[];
  connections: ConnectionOption[];
}) {
  const { modelsByConnection, modelsLoading, loadModels } = useConnectionModels();
  const [routeFilter, setRouteFilter] = useState("");

  const sortedRoutes = useMemo(
    () => [...routes].sort((a, b) => a.public_model_id.localeCompare(b.public_model_id)),
    [routes],
  );
  const knownRouteIds = useMemo(() => new Set(routes.map((route) => route.id)), [routes]);
  const deletedRouteIds = value.routeIds.filter((id) => !knownRouteIds.has(id));
  const filter = routeFilter.trim().toLowerCase();
  const visibleRoutes = filter
    ? sortedRoutes.filter((route) => route.public_model_id.toLowerCase().includes(filter))
    : sortedRoutes;

  // A key opened for editing can already have specific-model scopes; load
  // their model lists so the selections are visible without a click.
  const specificConnections = value.scopes
    .filter((scope) => scope.restrictModels && scope.connectionId)
    .map((scope) => scope.connectionId)
    .join("\n");
  useEffect(() => {
    for (const connectionId of specificConnections.split("\n")) {
      if (connectionId) void loadModels(connectionId);
    }
    // `loadModels` caches per connection; re-run only when the set changes.
  }, [specificConnections]);

  function setRouteGranted(id: string, granted: boolean) {
    onChange({
      ...value,
      routeIds: granted
        ? [...value.routeIds.filter((existing) => existing !== id), id]
        : value.routeIds.filter((existing) => existing !== id),
    });
  }

  function updateScope(id: number, update: Partial<ConnectionScopeDraft>) {
    onChange({
      ...value,
      scopes: value.scopes.map((scope) => (scope.id === id ? { ...scope, ...update } : scope)),
    });
  }

  function addScope() {
    onChange({
      ...value,
      scopes: [
        ...value.scopes,
        {
          id: Math.max(-1, ...value.scopes.map((scope) => scope.id)) + 1,
          connectionId: connections[0]?.id ?? "",
          restrictModels: false,
          models: [],
        },
      ],
    });
  }

  const grantsNothing = keyAccessDraftGrantsNothing(value);

  return (
    <section className="rounded-lg p-4" style={{ background: "var(--color-cds-elevated)", border: "1px solid var(--color-cds-border-strong)" }}>
      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
        <div>
          <h4 className="text-xs font-medium" style={{ color: "var(--color-cds-ink-medium)" }}>Access</h4>
          <p className="mt-1 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>
            Restrict a key to model routes, to direct models on specific connections, or both.
          </p>
        </div>
        <div className="grid grid-cols-2 overflow-hidden rounded-md" role="radiogroup" aria-label="Access" style={{ border: "1px solid var(--color-cds-border-strong)" }}>
          {(["all", "restricted"] as const).map((mode) => (
            <button
              key={mode}
              type="button"
              role="radio"
              aria-checked={value.mode === mode}
              onClick={() => onChange({ ...value, mode })}
              className="px-3 py-1.5 text-xs font-medium"
              style={value.mode === mode ? { background: "var(--color-cds-primary)", color: "white" } : { color: "var(--color-cds-ink-secondary)" }}
            >
              {mode === "all" ? "All models" : "Restricted"}
            </button>
          ))}
        </div>
      </div>

      {value.mode === "all" ? (
        <p className="rounded-md px-4 py-3 text-xs" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border-strong)", color: "var(--color-cds-ink-secondary)" }}>
          <span className="mr-2" aria-hidden="true">⚠</span>
          This key can call <span className="font-medium text-white">every model route and every model on every connection</span>. Choose Restricted to limit it.
        </p>
      ) : (
        <div className="space-y-4">
          {grantsNothing && (
            <p role="status" className="rounded-md px-4 py-3 text-xs" style={{ background: "var(--color-cds-status-yellow-fill)", color: "var(--color-cds-status-yellow-text)" }}>
              Nothing is selected. This key will not be able to call any model until you grant a model route or a connection scope.
            </p>
          )}

          <div>
            <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
              <div>
                <h5 className="text-xs font-medium" style={{ color: "var(--color-cds-ink-medium)" }}>
                  Model routes <span style={{ color: "var(--color-cds-ink-subtle)" }}>({value.routeIds.length} selected)</span>
                </h5>
                <p className="mt-0.5 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>
                  A route grant covers everything the route dispatches to: its default target, judge selections and fallbacks, on any connection.
                </p>
              </div>
              {routes.length > ROUTE_FILTER_THRESHOLD && (
                <input
                  type="search"
                  value={routeFilter}
                  onChange={(event) => setRouteFilter(event.target.value)}
                  placeholder="Filter routes"
                  aria-label="Filter model routes"
                  className="w-48 rounded-md px-2 py-1.5 text-xs focus:outline-none"
                  style={inputStyle}
                />
              )}
            </div>
            <div className="max-h-48 overflow-y-auto rounded-md p-2" style={{ background: "var(--color-cds-status-neutral-fill)", border: "1px solid var(--color-cds-border-strong)" }}>
              {deletedRouteIds.map((id) => (
                <label key={id} className="flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-sm hover:bg-white/5" style={{ color: "var(--color-cds-status-red-text)" }} title={id}>
                  <input type="checkbox" checked onChange={() => setRouteGranted(id, false)} />
                  Deleted route <span className="font-mono text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>{id}</span>
                  <span className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>· uncheck to remove</span>
                </label>
              ))}
              {visibleRoutes.map((route) => (
                <label key={route.id} className="flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-sm hover:bg-white/5" style={{ color: "var(--color-cds-ink)" }} title={route.id}>
                  <input
                    type="checkbox"
                    checked={value.routeIds.includes(route.id)}
                    onChange={(event) => setRouteGranted(route.id, event.target.checked)}
                  />
                  <span className="font-mono text-xs">{route.public_model_id}</span>
                  {route.published === false && <span className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>hidden</span>}
                </label>
              ))}
              {routes.length === 0 && deletedRouteIds.length === 0 && (
                <p className="px-2 py-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>No model routes exist yet.</p>
              )}
              {routes.length > 0 && visibleRoutes.length === 0 && (
                <p className="px-2 py-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>No routes match “{routeFilter}”.</p>
              )}
            </div>
          </div>

          <div>
            <h5 className="text-xs font-medium" style={{ color: "var(--color-cds-ink-medium)" }}>Connection scopes</h5>
            <p className="mt-0.5 mb-2 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>
              Grants direct model ids on a connection. Model routes are granted above, never here.
            </p>
            {value.scopes.length > 0 && (
              <div className="divide-y" style={{ borderColor: "var(--color-cds-border-strong)" }}>
                {value.scopes.map((scope) => {
                  const available = directModelIds(modelsByConnection[scope.connectionId] ?? []);
                  // Keep stored selections visible (and removable) even when
                  // the connection no longer lists them or they are invalid.
                  const choices = [...available, ...scope.models.filter((model) => !available.includes(model))];
                  return (
                    <div key={scope.id} className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.65fr)_auto] gap-3 py-3">
                      <select
                        value={scope.connectionId}
                        onChange={(event) => {
                          const connectionId = event.target.value;
                          updateScope(scope.id, { connectionId, models: [] });
                          if (scope.restrictModels) void loadModels(connectionId);
                        }}
                        aria-label="Connection"
                        className="min-w-0 self-start rounded-md px-3 py-2 text-sm focus:outline-none"
                        style={inputStyle}
                      >
                        <option value="">Choose connection</option>
                        {connections.map((connection) => (
                          <option key={connection.id} value={connection.id}>{connection.name}</option>
                        ))}
                        {scope.connectionId && !connections.some((connection) => connection.id === scope.connectionId) && (
                          <option value={scope.connectionId}>{scope.connectionId}</option>
                        )}
                      </select>
                      <div>
                        <div className="grid grid-cols-2 overflow-hidden rounded-md" style={{ border: "1px solid var(--color-cds-border-strong)" }}>
                          {([false, true] as const).map((restrictModels) => (
                            <button
                              key={String(restrictModels)}
                              type="button"
                              onClick={() => {
                                updateScope(scope.id, restrictModels ? { restrictModels } : { restrictModels, models: [] });
                                if (restrictModels) void loadModels(scope.connectionId);
                              }}
                              className="px-3 py-2 text-sm font-medium"
                              style={scope.restrictModels === restrictModels ? { background: "var(--color-cds-primary)", color: "white" } : { color: "var(--color-cds-ink-secondary)" }}
                            >
                              {restrictModels ? "Specific models" : "All direct models"}
                            </button>
                          ))}
                        </div>
                        {scope.restrictModels && (
                          <div className="mt-2 max-h-44 overflow-y-auto rounded-md p-2" style={{ background: "var(--color-cds-status-neutral-fill)", border: "1px solid var(--color-cds-border-strong)" }}>
                            {choices.map((model) => {
                              const invalid = connectionScopeModelError(model);
                              return (
                                <label key={model} className="flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-sm hover:bg-white/5" style={{ color: invalid ? "var(--color-cds-status-red-text)" : "var(--color-cds-ink)" }} title={invalid ?? undefined}>
                                  <input
                                    type="checkbox"
                                    checked={scope.models.includes(model)}
                                    onChange={(event) => updateScope(scope.id, {
                                      models: event.target.checked
                                        ? [...scope.models, model]
                                        : scope.models.filter((selected) => selected !== model),
                                    })}
                                  />
                                  {model}
                                  {invalid && <span className="text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>· not allowed here, uncheck to remove</span>}
                                </label>
                              );
                            })}
                            {choices.length === 0 && (
                              <p className="px-2 py-1 text-xs" style={{ color: "var(--color-cds-ink-secondary)" }}>
                                {modelsLoading[scope.connectionId]
                                  ? "Loading models for this connection…"
                                  : "No direct models are available for this connection. Allow all direct models, or grant a model route instead."}
                              </p>
                            )}
                          </div>
                        )}
                      </div>
                      <button
                        type="button"
                        onClick={() => onChange({ ...value, scopes: value.scopes.filter((item) => item.id !== scope.id) })}
                        className="self-start rounded p-2 text-sm"
                        aria-label="Remove connection scope"
                        style={{ color: "var(--color-cds-ink-medium)" }}
                      >
                        🗑
                      </button>
                    </div>
                  );
                })}
              </div>
            )}
            <button
              type="button"
              onClick={addScope}
              disabled={connections.length === 0}
              className="mt-2 rounded-lg px-3 py-2 text-xs font-medium disabled:opacity-50"
              style={{ border: "1px solid var(--color-cds-border-strong)", color: "var(--color-cds-ink-strong)" }}
            >
              + Add a connection scope
            </button>
            {connections.length === 0 && (
              <p className="mt-2 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>No connections exist yet.</p>
            )}
          </div>
        </div>
      )}
    </section>
  );
}
