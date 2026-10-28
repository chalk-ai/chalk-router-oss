"use client";

import { useRef, useState } from "react";

/**
 * Loads and caches the public model ids available on a connection, for UIs
 * that let an admin restrict a token scope or rate limit to a subset of a
 * connection's models (issuing a token, editing one, or editing a shared
 * rate limit).
 *
 * A connection's model list is the union of its provider catalog
 * (`/api/admin/providers/{id}/models`) and its published model routes
 * (`/api/admin/model-routes?connection_id=...&published=true`, paginated),
 * deduped and sorted. Results — including an empty list on failure — are
 * cached in `modelsByConnection` per connection id, and an in-flight ref set
 * guards against duplicate concurrent fetches for the same connection, so
 * `loadModels` is safe to call from multiple UI interactions (e.g. every
 * keystroke toggling "specific models" on) without refetching.
 */
export function useConnectionModels() {
  const [modelsByConnection, setModelsByConnection] = useState<Record<string, string[]>>({});
  const [modelsLoading, setModelsLoading] = useState<Record<string, boolean>>({});
  const loadingConnections = useRef(new Set<string>());

  async function loadModels(connectionId: string) {
    if (!connectionId || modelsByConnection[connectionId] !== undefined || loadingConnections.current.has(connectionId)) return;
    loadingConnections.current.add(connectionId);
    setModelsLoading((previous) => ({ ...previous, [connectionId]: true }));
    try {
      const [catalogResponse, firstRoutesResponse] = await Promise.all([
        fetch(`/api/admin/providers/${encodeURIComponent(connectionId)}/models`),
        fetch(`/api/admin/model-routes?connection_id=${encodeURIComponent(connectionId)}&published=true&limit=1000`),
      ]);
      if (!catalogResponse.ok || !firstRoutesResponse.ok) throw new Error("Unable to load models");
      const catalog: { data?: { public_model_id?: string }[] } = await catalogResponse.json();
      const routes: { data?: { public_model_id?: string }[]; next_cursor?: string } = await firstRoutesResponse.json();
      const routeModels = [...(routes.data ?? [])];
      let cursor = routes.next_cursor;
      while (cursor) {
        const response = await fetch(`/api/admin/model-routes?connection_id=${encodeURIComponent(connectionId)}&published=true&limit=1000&cursor=${encodeURIComponent(cursor)}`);
        if (!response.ok) throw new Error("Unable to load models");
        const page: { data?: { public_model_id?: string }[]; next_cursor?: string } = await response.json();
        routeModels.push(...(page.data ?? []));
        cursor = page.next_cursor;
      }
      setModelsByConnection((previous) => ({
        ...previous,
        [connectionId]: [...(catalog.data ?? []), ...routeModels]
          .flatMap((model) => typeof model.public_model_id === "string" ? [model.public_model_id] : [])
          .filter((model, index, models) => models.indexOf(model) === index)
          .sort(),
      }));
    } catch {
      setModelsByConnection((previous) => ({ ...previous, [connectionId]: [] }));
    } finally {
      loadingConnections.current.delete(connectionId);
      setModelsLoading((previous) => ({ ...previous, [connectionId]: false }));
    }
  }

  return { modelsByConnection, modelsLoading, loadModels };
}
