// Share in-flight discovery across the model selectors in a route form.
// Completed requests are not cached, so reopening a form refreshes the list.
const pending = new Map<string, Promise<string[]>>();

export function loadConnectionModels(connectionId: string): Promise<string[]> {
  const existing = pending.get(connectionId);
  if (existing) return existing;
  const request = (async () => {
    const response = await fetch(`/api/admin/providers/${encodeURIComponent(connectionId)}/models`);
    if (!response.ok) throw new Error("Could not load models for this connection.");
    const body: { data: { upstream_model_id: string }[] } = await response.json();
    return [...new Set(body.data.map(model => model.upstream_model_id).filter(Boolean))].sort();
  })();
  pending.set(connectionId, request);
  const clear = () => { if (pending.get(connectionId) === request) pending.delete(connectionId); };
  void request.then(clear, clear);
  return request;
}
