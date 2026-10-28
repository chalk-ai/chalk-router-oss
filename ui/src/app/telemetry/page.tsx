"use client";

import { useEffect, useState } from "react";
import type { TelemetryExporterJson } from "@/lib/serialize";
import { ConfirmationModal } from "@/components/ConfirmationModal";

const CARD = { background: "#111827", border: "1px solid #1f2937" } as const;
const INPUT = {
  background: "#0f172a",
  border: "1px solid #374151",
  color: "#e5e7eb",
  outline: "none",
} as const;

type FormState = {
  id: string;
  name: string;
  enabled: boolean;
  apiKey: string;
  parent: string;
  project: string;
  projectId: string;
  apiUrl: string;
};

type TestResult = {
  ok: boolean;
  error: string | null;
  latency_ms: number | null;
};

const EMPTY_FORM: FormState = {
  id: "",
  name: "",
  enabled: true,
  apiKey: "",
  parent: "",
  project: "",
  projectId: "",
  apiUrl: "https://api.braintrust.dev",
};

function formFromExporter(exporter: TelemetryExporterJson): FormState {
  const config = exporter.config;
  return {
    id: exporter.id,
    name: exporter.name,
    enabled: exporter.enabled,
    apiKey: "",
    parent: typeof config.parent === "string" ? config.parent : "",
    project: typeof config.project === "string" ? config.project : "",
    projectId: typeof config.project_id === "string" ? config.project_id : "",
    apiUrl: typeof config.api_url === "string" ? config.api_url : "https://api.braintrust.dev",
  };
}

function exporterPayload(form: FormState, id: string): Record<string, unknown> {
  const payload: Record<string, unknown> = {
    id,
    name: form.name.trim(),
    exporter_type: "braintrust",
    enabled: form.enabled,
    config: {
      parent: form.parent.trim(),
      project: form.parent.trim() || form.projectId.trim() ? "" : form.project.trim(),
      project_id: form.parent.trim() ? "" : form.projectId.trim(),
      api_url: form.apiUrl.trim(),
    },
  };
  if (form.apiKey.trim()) payload.api_key = form.apiKey.trim();
  return payload;
}

function exporterDetails(exporter: TelemetryExporterJson): string {
  if (exporter.exporter_type === "otlp") {
    const endpoint = String(exporter.config.endpoint ?? "");
    const protocol = String(exporter.config.protocol ?? "grpc");
    const serviceName = String(exporter.config.service_name ?? "chalk-router");
    return `endpoint=${endpoint} · protocol=${protocol} · service_name=${serviceName}`;
  }

  const parent = typeof exporter.config.parent === "string" ? exporter.config.parent.trim() : "";
  const projectId = typeof exporter.config.project_id === "string" ? exporter.config.project_id.trim() : "";
  const projectName = typeof exporter.config.project === "string" ? exporter.config.project.trim() : "";
  const project = parent || (projectId ? `project ID ${projectId}` : projectName || "chalk-router");
  return `${exporter.api_key_set ? "API key configured" : "API key required"} · ${project}`;
}

function Switch({ checked, onChange }: { checked: boolean; onChange: (value: boolean) => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className="flex-shrink-0 rounded-full transition-colors"
      style={{ width: "40px", height: "22px", padding: "2px", background: checked ? "#218E61" : "#374151" }}
    >
      <span
        className="block rounded-full transition-transform"
        style={{ width: "18px", height: "18px", background: "#fff", transform: checked ? "translateX(18px)" : "translateX(0)" }}
      />
    </button>
  );
}

export default function TelemetryPage() {
  const [exporters, setExporters] = useState<TelemetryExporterJson[]>([]);
  const [form, setForm] = useState<FormState | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [exporterPendingRemoval, setExporterPendingRemoval] = useState<TelemetryExporterJson | null>(null);
  const [removingExporterId, setRemovingExporterId] = useState<string | null>(null);

  async function load() {
    setLoading(true);
    try {
      const response = await fetch("/api/admin/telemetry-exporters", { cache: "no-store" });
      if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
      const body = (await response.json()) as { exporters: TelemetryExporterJson[] };
      setExporters(body.exporters);
      setForm(null);
      setTestResult(null);
    } catch (loadError) {
      setError(loadError instanceof Error ? loadError.message : String(loadError));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void load(); }, []);

  function updateForm(next: FormState) {
    setForm(next);
    setTestResult(null);
  }

  async function save() {
    if (!form || !form.name.trim()) return;
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const payload = exporterPayload(
        form,
        form.id || `braintrust-${crypto.randomUUID()}`,
      );
      const response = await fetch("/api/admin/telemetry-exporters", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => null);
        throw new Error(body?.error?.message ?? `${response.status} ${response.statusText}`);
      }
      setNotice("Telemetry export settings saved. Changes are active on this router within a few seconds.");
      setForm(null);
      await load();
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : String(saveError));
    } finally {
      setSaving(false);
    }
  }

  async function testConfiguration() {
    if (!form) return;
    setTesting(true);
    setError(null);
    setNotice(null);
    setTestResult(null);
    try {
      const response = await fetch("/api/admin/telemetry-exporters/test", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(exporterPayload(form, form.id)),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => null);
        throw new Error(body?.error?.message ?? `${response.status} ${response.statusText}`);
      }
      setTestResult((await response.json()) as TestResult);
    } catch (testError) {
      setTestResult({
        ok: false,
        error: testError instanceof Error ? testError.message : String(testError),
        latency_ms: null,
      });
    } finally {
      setTesting(false);
    }
  }

  async function remove(id: string): Promise<boolean> {
    setRemovingExporterId(id);
    setError(null);
    setNotice(null);
    try {
      const response = await fetch("/api/admin/telemetry-exporters", {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id }),
      });
      if (!response.ok) {
        const body = await response.json().catch(() => null);
        throw new Error(body?.error?.message ?? `${response.status} ${response.statusText}`);
      }
      setForm(null);
      await load();
      return true;
    } catch (removeError) {
      setError(removeError instanceof Error ? removeError.message : String(removeError));
      return false;
    } finally {
      setRemovingExporterId(null);
    }
  }

  return (
    <div>
      <div className="mb-8">
        <h1 className="text-2xl font-semibold text-white tracking-tight">Telemetry Export</h1>
        <p className="mt-1 text-sm" style={{ color: "#6b7280" }}>
          Send router traces to external observability platforms. The primary OTLP pipeline is managed by environment variables.
        </p>
      </div>

      {error && <div className="mb-5 rounded-lg px-4 py-3 text-sm" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>{error}</div>}
      {notice && <div className="mb-5 rounded-lg px-4 py-3 text-sm" style={{ background: "#052e16", border: "1px solid #166534", color: "#86efac" }}>{notice}</div>}

      {loading ? (
        <div className="text-sm" style={{ color: "#6b7280" }}>Loading telemetry exporters…</div>
      ) : (
        <div className="space-y-4">
          {exporters.map((exporter) => (
            <div key={exporter.id} className="rounded-lg p-5" style={{ ...CARD, opacity: exporter.enabled ? 1 : 0.65 }}>
              <div className="flex items-center justify-between gap-4">
                <div>
                  <div className="flex items-center gap-2">
                    <span className="text-xs px-2 py-0.5 rounded font-medium" style={{ background: "#172554", color: "#93c5fd" }}>{exporter.exporter_type}</span>
                    <span className="text-sm font-medium text-white">{exporter.name}</span>
                    <span className="text-xs" style={{ color: exporter.enabled ? "#4ade80" : "#6b7280" }}>{exporter.enabled ? "active" : "paused"}</span>
                    {exporter.read_only && <span className="text-xs" style={{ color: "#6b7280" }}>environment managed</span>}
                  </div>
                  <div className="text-xs mt-2" style={{ color: "#6b7280" }}>
                    {exporterDetails(exporter)}
                  </div>
                </div>
                {!exporter.read_only && (
                  <div className="flex items-center gap-2">
                    <button className="text-xs px-2.5 py-1 rounded" style={{ background: "#1f2937", color: "#d1d5db", border: "1px solid #374151" }} onClick={() => updateForm(formFromExporter(exporter))}>Edit</button>
                    <button className="text-xs px-2.5 py-1 rounded disabled:opacity-50" disabled={removingExporterId === exporter.id} style={{ background: "#1c0a0a", color: "#ef4444", border: "1px solid #7f1d1d" }} onClick={() => setExporterPendingRemoval(exporter)}>Remove</button>
                  </div>
                )}
              </div>
            </div>
          ))}

          {form ? (
            <div className="rounded-lg p-5 space-y-5" style={{ ...CARD, border: "1px solid #166534" }}>
              <div className="flex items-center justify-between">
                <h2 className="text-sm font-semibold text-white">{form.id ? "Edit exporter" : "Add exporter"}</h2>
                <button className="text-xs" style={{ color: "#6b7280" }} onClick={() => { setForm(null); setTestResult(null); }}>Cancel</button>
              </div>
              <div className="flex items-start justify-between gap-4">
                <div><div className="text-sm font-medium text-white">Enabled</div><div className="text-xs mt-0.5" style={{ color: "#6b7280" }}>Pause export without deleting the saved credentials.</div></div>
                <Switch checked={form.enabled} onChange={(enabled) => updateForm({ ...form, enabled })} />
              </div>
              <label className="block text-xs" style={{ color: "#9ca3af" }}>Display name<input className="mt-1 w-full rounded px-3 py-2 text-sm" style={INPUT} value={form.name} placeholder="e.g. Production Braintrust" onChange={(event) => updateForm({ ...form, name: event.target.value })} /></label>
              <label className="block text-xs" style={{ color: "#9ca3af" }}>API key{form.id && <span style={{ color: "#6b7280" }}> · leave blank to keep the existing key</span>}<input type="password" className="mt-1 w-full rounded px-3 py-2 text-sm" style={INPUT} value={form.apiKey} placeholder={form.id ? "••••••••••••••••" : "br-…"} onChange={(event) => updateForm({ ...form, apiKey: event.target.value })} /></label>
              <label className="block text-xs" style={{ color: "#9ca3af" }}>Parent <span style={{ color: "#6b7280" }}>· optional advanced Braintrust parent</span><input className="mt-1 w-full rounded px-3 py-2 text-sm" style={INPUT} value={form.parent} onChange={(event) => updateForm({ ...form, parent: event.target.value, project: "", projectId: "" })} placeholder="experiment_id:…" /></label>
              <div className="grid grid-cols-2 gap-4">
                <label className="block text-xs" style={{ color: "#9ca3af" }}>Project name<input className="mt-1 w-full rounded px-3 py-2 text-sm" style={INPUT} value={form.project} onChange={(event) => updateForm({ ...form, parent: "", project: event.target.value, projectId: "" })} placeholder="chalk-router (default)" /></label>
                <label className="block text-xs" style={{ color: "#9ca3af" }}>Project ID<input className="mt-1 w-full rounded px-3 py-2 text-sm" style={INPUT} value={form.projectId} onChange={(event) => updateForm({ ...form, parent: "", projectId: event.target.value, project: "" })} placeholder="optional" /></label>
              </div>
              <label className="block text-xs" style={{ color: "#9ca3af" }}>Endpoint <span style={{ color: "#6b7280" }}>· optional for self-hosted Braintrust</span><input className="mt-1 w-full rounded px-3 py-2 text-sm" style={INPUT} value={form.apiUrl} onChange={(event) => updateForm({ ...form, apiUrl: event.target.value })} placeholder="https://api.braintrust.dev" /></label>
              {testResult && (
                <div className="px-3 py-2 rounded-lg text-xs" style={{ background: testResult.ok ? "#052e16" : "#1c0a0a", border: `1px solid ${testResult.ok ? "#166534" : "#7f1d1d"}`, color: testResult.ok ? "#70CD96" : "#fca5a5" }}>
                  {testResult.ok
                    ? `Configuration OK${testResult.latency_ms != null ? ` · ${testResult.latency_ms}ms` : ""}`
                    : `Test failed: ${testResult.error ?? "unknown error"}`}
                </div>
              )}
              <div className="flex justify-between">
                <button disabled={testing || saving} className="px-4 py-2 rounded-lg text-sm disabled:opacity-50" style={{ background: "#1f2937", color: "#e5e7eb", border: "1px solid #374151" }} onClick={() => void testConfiguration()}>{testing ? "Testing…" : "Test configuration"}</button>
                <button disabled={saving || testing || !form.name.trim()} className="px-5 py-2 rounded-lg text-sm font-medium disabled:opacity-50" style={{ background: "#218E61", color: "white" }} onClick={() => void save()}>{saving ? "Saving…" : "Save exporter"}</button>
              </div>
            </div>
          ) : (
            <button className="px-4 py-2 rounded-lg text-sm font-medium" style={{ background: "#218E61", color: "white" }} onClick={() => updateForm({ ...EMPTY_FORM })}>Add Braintrust exporter</button>
          )}
        </div>
      )}
      <ConfirmationModal
        open={exporterPendingRemoval !== null}
        title="Remove telemetry exporter?"
        description={`This stops sending router traces to ${exporterPendingRemoval?.name ?? "this exporter"}.`}
        confirmLabel="Remove exporter"
        confirming={exporterPendingRemoval?.id === removingExporterId}
        onCancel={() => setExporterPendingRemoval(null)}
        onConfirm={async () => {
          if (exporterPendingRemoval && await remove(exporterPendingRemoval.id)) {
            setExporterPendingRemoval(null);
          }
        }}
      />
    </div>
  );
}
