"use client";

import { useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { ConfirmationModal } from "@/components/ConfirmationModal";
import { IconButton, InfiniteScroll, inputStyle, Modal, PageHeader } from "@/components/router/RouterPrimitives";
import { fetchAdminPage } from "@/lib/adminPagination";
import { usePagination } from "@/lib/usePagination";

type Exposure = "dynamic" | "unlisted" | "explicit" | "unspecified";

interface ConnectionState {
  configured: boolean;
  unreadable: boolean;
  healthy: boolean | null;
}

interface Connection {
  id: string;
  name: string;
  provider_kind: string;
  base_url: string | null;
  prefix: string | null;
  exposure: Exposure;
  routing_enabled: boolean;
  state: ConnectionState;
  declared_capabilities: Record<string, unknown> | null;
  created_at: string;
  updated_at: string;
}

interface TestResult {
  ok: boolean;
  error: string | null;
  discovered_models: number | null;
  latency_ms: number | null;
}

/* ------------------------------------------------------------------ */
/*  Provider logos – inline SVGs                                       */
/* ------------------------------------------------------------------ */

function OpenAILogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M22.282 9.821a5.985 5.985 0 0 0-.516-4.91 6.046 6.046 0 0 0-6.51-2.9A6.065 6.065 0 0 0 4.981 4.18a5.998 5.998 0 0 0-3.998 2.9 6.042 6.042 0 0 0 .743 7.097 5.98 5.98 0 0 0 .51 4.911 6.051 6.051 0 0 0 6.515 2.9A5.985 5.985 0 0 0 13.26 24a6.056 6.056 0 0 0 5.772-4.206 5.99 5.99 0 0 0 3.997-2.9 6.056 6.056 0 0 0-.747-7.073zM13.26 22.43a4.476 4.476 0 0 1-2.876-1.04l.143-.08 4.779-2.758a.795.795 0 0 0 .392-.681v-6.737l2.02 1.168a.071.071 0 0 1 .038.052v5.583a4.504 4.504 0 0 1-4.496 4.494zM3.6 18.304a4.47 4.47 0 0 1-.535-3.014l.142.085 4.783 2.759a.771.771 0 0 0 .78 0l5.843-3.369v2.332a.08.08 0 0 1-.033.062L9.74 19.95a4.5 4.5 0 0 1-6.14-1.646zM2.34 7.896a4.485 4.485 0 0 1 2.366-1.973V11.6a.766.766 0 0 0 .388.676l5.815 3.355-2.02 1.168a.076.076 0 0 1-.071 0l-4.83-2.786A4.504 4.504 0 0 1 2.34 7.872zm16.597 3.855l-5.833-3.387L15.119 7.2a.076.076 0 0 1 .071 0l4.83 2.791a4.494 4.494 0 0 1-.676 8.105v-5.678a.79.79 0 0 0-.407-.667zm2.01-3.023l-.141-.085-4.774-2.782a.776.776 0 0 0-.785 0L9.409 9.23V6.897a.066.066 0 0 1 .028-.061l4.83-2.787a4.5 4.5 0 0 1 6.68 4.66zm-12.64 4.135l-2.02-1.164a.08.08 0 0 1-.038-.057V6.075a4.5 4.5 0 0 1 7.375-3.453l-.142.08L8.704 5.46a.795.795 0 0 0-.393.681zm1.097-2.365l2.602-1.5 2.607 1.5v2.999l-2.597 1.5-2.607-1.5z" fill="currentColor"/>
    </svg>
  );
}

function AnthropicLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M13.827 3.52h3.603L24 20.48h-3.603l-6.57-16.96zm-7.258 0h3.767L16.906 20.48h-3.674l-1.343-3.461H5.017l-1.344 3.46H0L6.57 3.522zm1.94 5.027l-2.2 5.659h4.4l-2.2-5.66z" fill="currentColor"/>
    </svg>
  );
}

function GeminiLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M12 24A14.304 14.304 0 0 0 0 12 14.304 14.304 0 0 0 12 0a14.305 14.305 0 0 0 12 12 14.305 14.305 0 0 0-12 12z" fill="currentColor"/>
    </svg>
  );
}

function BedrockLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M12 2L2 7v10l10 5 10-5V7L12 2zm0 2.18L19.09 7.5 12 10.82 4.91 7.5 12 4.18zM4 8.83l7 3.5v7.35l-7-3.5V8.83zm9 10.85v-7.35l7-3.5v7.35l-7 3.5z" fill="currentColor"/>
    </svg>
  );
}

function VLLMLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <rect x="3" y="3" width="18" height="18" rx="3" stroke="currentColor" strokeWidth="1.5"/>
      <path d="M7 9l3 6 3-6M16 9v6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"/>
    </svg>
  );
}

function OpenRouterLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="1.5"/>
      <path d="M12 3v18M3 12h18M5.5 5.5l13 13M18.5 5.5l-13 13" stroke="currentColor" strokeWidth="1" opacity="0.4"/>
      <circle cx="12" cy="12" r="3" fill="currentColor"/>
    </svg>
  );
}

function XAILogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <text x="4" y="18" fontSize="16" fontWeight="700" fontFamily="system-ui, sans-serif" fill="currentColor">x.ai</text>
    </svg>
  );
}

function CohereLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M8.5 4C5.46 4 3 6.46 3 9.5S5.46 15 8.5 15h2c1.38 0 2.5 1.12 2.5 2.5S11.88 20 10.5 20H6" stroke="currentColor" strokeWidth="2" strokeLinecap="round"/>
      <circle cx="17" cy="8" r="3" fill="currentColor"/>
    </svg>
  );
}

function QwenLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="1.5"/>
      <path d="M12 7v5l3.5 3.5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"/>
      <path d="M15 15l4 4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round"/>
    </svg>
  );
}

function FireworksLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M12 2l1.7 7.1L20 5l-4.1 6.3L23 13l-7.1 1.7L20 21l-6.3-4.1L12 24l-1.7-7.1L4 21l4.1-6.3L1 13l7.1-1.7L4 5l6.3 4.1L12 2z" fill="currentColor"/>
      <circle cx="12" cy="13" r="2.5" fill="#111827"/>
    </svg>
  );
}

function TogetherLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <circle cx="8" cy="12" r="5" stroke="currentColor" strokeWidth="2"/>
      <circle cx="16" cy="12" r="5" stroke="currentColor" strokeWidth="2"/>
      <path d="M8 12h8" stroke="currentColor" strokeWidth="2" strokeLinecap="round"/>
    </svg>
  );
}

function GroqLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M18.5 7.5A8 8 0 1 0 20 12h-8v4h3.7A4.5 4.5 0 1 1 15.3 8" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"/>
    </svg>
  );
}

function PerplexityLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M12 2v20M4 7l8 5 8-5M4 7v8l8-3 8 3V7M7 18.5l5-6.5 5 6.5" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round"/>
    </svg>
  );
}

function MistralLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M4 4h4v4h2v4h4V8h2V4h4v16h-4v-6h-2v4h-4v-4H8v6H4V4z" fill="currentColor"/>
    </svg>
  );
}

function VoyageLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="1.5" />
      <path d="M12 6l2.2 5.8L12 18l-2.2-6.2L12 6z" fill="currentColor" />
    </svg>
  );
}

function TypeSafeLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M4 5h16M12 5v14" stroke="currentColor" strokeWidth="2" strokeLinecap="round"/>
      <path d="M7 11h10l-5 8-5-8z" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round"/>
    </svg>
  );
}

function CompatibleLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/>
      <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>
    </svg>
  );
}

function VertexLogo() {
  return (
    <svg width="24" height="24" viewBox="0 0 24 24" fill="none">
      <path d="M12 3L3 20h18L12 3z" stroke="currentColor" strokeWidth="1.75" strokeLinejoin="round"/>
      <path d="M12 9.5L8.5 16h7L12 9.5z" fill="currentColor"/>
    </svg>
  );
}

const PROVIDER_LOGOS: Record<string, React.ReactNode> = {
  openai: <OpenAILogo />,
  anthropic: <AnthropicLogo />,
  voyage: <VoyageLogo />,
  typesafe: <TypeSafeLogo />,
  vllm: <VLLMLogo />,
  gemini: <GeminiLogo />,
  vertex: <VertexLogo />,
  bedrock: <BedrockLogo />,
  openrouter: <OpenRouterLogo />,
  xai: <XAILogo />,
  cohere: <CohereLogo />,
  qwen: <QwenLogo />,
  fireworks: <FireworksLogo />,
  together: <TogetherLogo />,
  groq: <GroqLogo />,
  mistral: <MistralLogo />,
  perplexity: <PerplexityLogo />,
  "openai-compatible": <CompatibleLogo />,
};

const PROVIDER_COLORS: Record<string, { accent: string; bg: string }> = {
  openai: { accent: "#10a37f", bg: "#052e16" },
  anthropic: { accent: "#d4a27f", bg: "#2a1a0e" },
  voyage: { accent: "#3ab0d4", bg: "#08222b" },
  typesafe: { accent: "#a3e635", bg: "#17220a" },
  vllm: { accent: "#a78bfa", bg: "#1e1538" },
  gemini: { accent: "#4285f4", bg: "#0f1d3a" },
  vertex: { accent: "#34a853", bg: "#0c2416" },
  bedrock: { accent: "#ff9900", bg: "#2a1a00" },
  openrouter: { accent: "#6366f1", bg: "#15153a" },
  xai: { accent: "#e5e7eb", bg: "#1a1a1a" },
  cohere: { accent: "#39594d", bg: "#0d1f18" },
  qwen: { accent: "#615fff", bg: "#131340" },
  fireworks: { accent: "#f97316", bg: "#2a1207" },
  together: { accent: "#14b8a6", bg: "#072724" },
  groq: { accent: "#f55036", bg: "#2b0e09" },
  mistral: { accent: "#ff7000", bg: "#2a1300" },
  perplexity: { accent: "#20b8cd", bg: "#062328" },
  "openai-compatible": { accent: "#9ca3af", bg: "#1f2937" },
};

// Built-in kinds settable through this UI, plus the catch-all
// "openai-compatible" kind (which requires a base_url).
const BUILTIN_KINDS = [
  "openai",
  "anthropic",
  "gemini",
  "vertex",
  "voyage",
  "typesafe",
  "vllm",
  "bedrock",
  "openrouter",
  "xai",
  "cohere",
  "qwen",
  "fireworks",
  "together",
  "groq",
  "mistral",
  "perplexity",
];
const KIND_OPTIONS = [...BUILTIN_KINDS, "openai-compatible"];

const VERTEX_US_CENTRAL_URL = "https://us-central1-aiplatform.googleapis.com";
const VERTEX_GLOBAL_URL = "https://aiplatform.googleapis.com";

// Built-in kinds that support a base-URL override. Anthropic overrides are
// validated by the backend, which allows loopback and public endpoints.
const SUPPORTS_BASE_URL = new Set([
  "openai",
  "anthropic",
  "gemini",
  // For Vertex the base URL is how the region is chosen: the endpoint host
  // carries it (https://<region>-aiplatform.googleapis.com).
  "vertex",
  "vllm",
  "bedrock",
  "openrouter",
  "typesafe",
  "xai",
  "cohere",
  "qwen",
  "fireworks",
  "together",
  "groq",
  "mistral",
  "perplexity",
]);

function showsBaseUrl(kind: string): boolean {
  return kind === "openai-compatible" || SUPPORTS_BASE_URL.has(kind);
}
function requiresBaseUrl(kind: string): boolean {
  // vLLM and openai-compatible endpoints have no code-owned default endpoint, so
  // a base URL is mandatory (mirrors the server-side validation).
  return kind === "openai-compatible" || kind === "vllm";
}

// Mirrors `is_runtime_connection` in `src/backend/bedrock.rs`: true when
// `baseUrl` is an AWS Bedrock *runtime* endpoint
// (`https://bedrock-runtime.<region>.amazonaws.com`), which authenticates
// through the process's AWS credential chain rather than an API key. Any
// other Bedrock base URL (the default Mantle endpoint, or a custom override)
// still needs a key.
function isBedrockRuntimeConnection(baseUrl: string): boolean {
  let url: URL;
  try {
    url = new URL(baseUrl);
  } catch {
    return false;
  }
  if (url.protocol !== "https:") return false;
  const host = url.hostname;
  const prefix = "bedrock-runtime.";
  const suffix = ".amazonaws.com";
  if (!host.startsWith(prefix) || !host.endsWith(suffix)) return false;
  const region = host.slice(prefix.length, host.length - suffix.length);
  return region.length > 0 && /^[a-z0-9-]+$/.test(region);
}

const EXPOSURE_COLORS: Record<Exposure, { bg: string; color: string }> = {
  dynamic: { bg: "#0f2a4a", color: "#93c5fd" },
  unlisted: { bg: "#1f2937", color: "#9ca3af" },
  explicit: { bg: "#2a1440", color: "#c4b5fd" },
  unspecified: { bg: "#1f2937", color: "#6b7280" },
};

interface FormState {
  name: string;
  provider_kind: string;
  base_url: string;
  prefix: string;
  exposure: Exposure;
  routing_enabled: boolean;
  api_key: string;
  declared_capabilities: string;
}

const EMPTY_FORM: FormState = {
  name: "",
  provider_kind: "openai",
  base_url: "",
  prefix: "",
  exposure: "dynamic",
  routing_enabled: true,
  api_key: "",
  declared_capabilities: "",
};

function parseDeclaredCapabilities(value: string): Record<string, unknown> | undefined {
  if (!value.trim()) return undefined;
  const parsed: unknown = JSON.parse(value);
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("Declared capabilities must be a JSON object");
  }
  return parsed as Record<string, unknown>;
}

export default function ProvidersClient({ embedded = false }: { embedded?: boolean }) {
  const { items: connections, setItems: setConnections, loading, setLoading, nextCursor, applyPage } = usePagination<Connection>((connection) => connection.id);
  const [error, setError] = useState<string | null>(null);
  const [connectionPendingDeletion, setConnectionPendingDeletion] = useState<Connection | null>(null);
  const [deletingConnectionId, setDeletingConnectionId] = useState<string | null>(null);

  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [form, setForm] = useState<FormState>(EMPTY_FORM);
  const [prefixEdited, setPrefixEdited] = useState(false);
  // Vertex's credential is a service account key JSON, not a bearer token, so
  // its field is a JSON textarea rather than a masked one-liner.
  const isVertex = form.provider_kind === "vertex";
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  // Test-in-form (draft) state.
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  // Per-row test state for saved connections.
  const [rowTest, setRowTest] = useState<Record<string, { loading: boolean; result: TestResult | null }>>({});

  /** Resolves to the rows loaded, or null when the fetch failed. */
  async function fetchConnections(cursor?: string): Promise<Connection[] | null> {
    setLoading(true);
    try {
      const page = await fetchAdminPage<Connection>("/api/admin/providers", cursor);
      applyPage(page, cursor);
      return page.data;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void fetchConnections(); }, []);

  async function handleNextPage() {
    if (!nextCursor) return;
    await fetchConnections(nextCursor);
  }

  async function resetToFirstPage() {
    if (await fetchConnections()) {
    }
  }

  // Deleting the last row of a page leaves the table blank; step back so the
  // user lands on rows instead of on an empty page with a live "Previous".
  async function reloadAfterRemoval() {
    await fetchConnections();
  }

  function suggestedPrefix(kind: string, name: string): string {
    const used = new Set(connections.flatMap((connection) => connection.prefix ? [connection.prefix] : []));
    const canonical = `${kind}/`;
    if (kind !== "openai-compatible" && !used.has(canonical)) return canonical;

    const slug = name
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9_-]+/g, "-")
      .replace(/^-+|-+$/g, "");
    if (slug && !used.has(`${slug}/`)) return `${slug}/`;

    for (let suffix = 2; ; suffix += 1) {
      const candidate = `${kind}-${suffix}/`;
      if (!used.has(candidate)) return candidate;
    }
  }

  // `nextCursor == null` (the full connection list is loaded) is the only
  // case this warns in; recomputing it is a nested filter/some scan over
  // every connection, so only redo it when the inputs actually change rather
  // than on every render (e.g. every keystroke while the create/edit form is
  // open).
  const ambiguityWarnings = useMemo(
    () =>
      nextCursor == null
        ? ["openai", "anthropic"].filter((kind) => {
            const eligible = connections.filter(
              (connection) => connection.provider_kind === kind && connection.exposure !== "explicit",
            );
            return eligible.length > 1 && !eligible.some((connection) => connection.prefix === `${kind}/`);
          })
        : [],
    [connections, nextCursor],
  );

  function openCreate() {
    setEditingId(null);
    setPrefixEdited(false);
    setForm({ ...EMPTY_FORM, prefix: suggestedPrefix(EMPTY_FORM.provider_kind, "") });
    setSaveError(null);
    setTestResult(null);
    setShowForm(true);
  }

  function openEdit(c: Connection) {
    setEditingId(c.id);
    setPrefixEdited(true);
    setForm({
      name: c.name,
      provider_kind: c.provider_kind,
      base_url: c.base_url ?? "",
      prefix: c.prefix ?? "",
      exposure: c.exposure === "unspecified" ? "dynamic" : c.exposure,
      routing_enabled: c.routing_enabled,
      api_key: "",
      declared_capabilities: c.declared_capabilities
        ? JSON.stringify(c.declared_capabilities, null, 2)
        : "",
    });
    setSaveError(null);
    setTestResult(null);
    setShowForm(true);
  }

  function closeForm() {
    setShowForm(false);
    setEditingId(null);
    setForm(EMPTY_FORM);
    setTestResult(null);
  }

  function formValid(): boolean {
    if (!form.name.trim()) return false;
    if (requiresBaseUrl(form.provider_kind) && !form.base_url.trim()) return false;
    if (form.exposure !== "explicit" && !form.prefix.trim()) return false;
    // On create, vLLM and arbitrary OpenAI-compatible endpoints may be
    // unauthenticated, and so may a Bedrock runtime connection (it
    // authenticates via the AWS credential chain, not an API key); on edit a
    // blank key means "keep the current one".
    if (
      !editingId &&
      !form.api_key.trim() &&
      form.provider_kind !== "vllm" &&
      form.provider_kind !== "openai-compatible" &&
      !(form.provider_kind === "bedrock" && isBedrockRuntimeConnection(form.base_url.trim()))
    ) return false;
    return true;
  }

  async function handleSave() {
    if (!formValid() || saving || testing) return;
    setSaving(true);
    setSaveError(null);
    try {
      const declaredCapabilities = parseDeclaredCapabilities(form.declared_capabilities);
      if (editingId) {
        const payload: Record<string, unknown> = {
          name: form.name.trim(),
          exposure: form.exposure,
          routing_enabled: form.routing_enabled,
          declared_capabilities: declaredCapabilities ?? null,
        };
        if (form.prefix.trim()) payload.prefix = form.prefix.trim();
        else payload.clear_prefix = true;
        if (showsBaseUrl(form.provider_kind)) {
          if (form.base_url.trim()) payload.base_url = form.base_url.trim();
          else payload.clear_base_url = true;
        }
        if (form.api_key.trim()) payload.api_key = form.api_key.trim();
        const res = await fetch(`/api/admin/providers/${encodeURIComponent(editingId)}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(payload),
        });
        if (!res.ok) throw new Error(await errorText(res));
        const updated: Connection = await res.json();
        setConnections((prev) => prev.map((c) => (c.id === editingId ? updated : c)));
      } else {
        const result = await testDraft();
        if (!result.ok) return;
        const res = await fetch("/api/admin/providers", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            name: form.name.trim(),
            provider_kind: form.provider_kind,
            base_url: showsBaseUrl(form.provider_kind) ? form.base_url.trim() || undefined : undefined,
            prefix: form.prefix.trim() || undefined,
            exposure: form.exposure,
            routing_enabled: form.routing_enabled,
            api_key: form.api_key.trim(),
            declared_capabilities: declaredCapabilities,
          }),
        });
        if (!res.ok) throw new Error(await errorText(res));
        await res.json();
        await resetToFirstPage();
      }
      closeForm();
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  async function testDraft(): Promise<TestResult> {
    setTesting(true);
    setTestResult(null);
    try {
      const res = await fetch("/api/admin/providers/test", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          connection_id: editingId ?? undefined,
          draft: {
            provider_kind: form.provider_kind.trim(),
            base_url: showsBaseUrl(form.provider_kind) ? form.base_url.trim() || undefined : undefined,
            prefix: form.prefix.trim() || undefined,
            exposure: form.exposure,
            api_key: form.api_key.trim(),
          },
        }),
      });
      if (!res.ok) throw new Error(await errorText(res));
      const result: TestResult = await res.json();
      setTestResult(result);
      return result;
    } catch (e) {
      const result: TestResult = { ok: false, error: e instanceof Error ? e.message : String(e), discovered_models: null, latency_ms: null };
      setTestResult(result);
      return result;
    } finally {
      setTesting(false);
    }
  }

  async function handleTestRow(id: string) {
    setRowTest((prev) => ({ ...prev, [id]: { loading: true, result: null } }));
    try {
      const res = await fetch("/api/admin/providers/test", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ connection_id: id }),
      });
      if (!res.ok) throw new Error(await errorText(res));
      const result: TestResult = await res.json();
      setRowTest((prev) => ({ ...prev, [id]: { loading: false, result } }));
    } catch (e) {
      setRowTest((prev) => ({
        ...prev,
        [id]: { loading: false, result: { ok: false, error: e instanceof Error ? e.message : String(e), discovered_models: null, latency_ms: null } },
      }));
    }
  }

  async function handleDelete(c: Connection): Promise<boolean> {
    setDeletingConnectionId(c.id);
    try {
      const res = await fetch(`/api/admin/providers/${encodeURIComponent(c.id)}`, { method: "DELETE" });
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
      setDeletingConnectionId(null);
    }
  }

  const saveDisabled = !formValid() || saving || testing;
  const editingConnection = connections.find((connection) => connection.id === editingId);

  return (
    <div>
      <div className="mb-6"><PageHeader title="Connections" description="Provider credentials, endpoints, and routing configuration." action={<button onClick={openCreate} className="rounded-lg px-4 py-2 text-sm font-medium" style={{ background: "#218E61", color: "white" }}>New connection</button>} /></div>

      {error && (
        <div className="mb-6 px-4 py-3 rounded-lg text-sm" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>
          <span className="font-medium">Error:</span> {error}
        </div>
      )}

      {ambiguityWarnings.map((kind) => (
        <div key={kind} className="mb-4 px-4 py-3 rounded-lg text-sm" style={{ background: "#2a1a00", border: "1px solid #92400e", color: "#fbbf24" }}>
          Multiple {kind} connections are configured without a <code>{kind}/</code> owner. Implicit {kind} requests are ambiguous; assign the canonical prefix or route explicitly.
        </div>
      ))}

      {loading ? (
        <p className="text-sm" style={{ color: "#6b7280" }}>Loading...</p>
      ) : (
        <>
          {connections.length > 0 ? (
            <div className="rounded-lg overflow-hidden mb-6" style={{ border: "1px solid #1f2937" }}>
              <table className="w-full text-sm">
                <thead>
                  <tr style={{ background: "#111827", borderBottom: "1px solid #1f2937" }}>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Connection</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Prefix</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Exposure</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Routing</th>
                    <th className="text-left px-5 py-3 font-medium" style={{ color: "#9ca3af" }}>Status</th>
                    <th className="sticky right-0 bg-slate-900 px-5 py-3 text-right font-medium" style={{ color: "#9ca3af" }}>Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {connections.map((c, i) => {
                    const colors = PROVIDER_COLORS[c.provider_kind] ?? { accent: "#9ca3af", bg: "#1f2937" };
                    const logo = PROVIDER_LOGOS[c.provider_kind] ?? <CompatibleLogo />;
                    const rt = rowTest[c.id];
                    return (
                      <tr key={c.id} role="link" tabIndex={0} onClick={(event) => { if (!(event.target as HTMLElement).closest("a,button,input")) window.location.assign(`/connections/${encodeURIComponent(c.id)}`); }} onKeyDown={(event) => { if (event.key === "Enter") window.location.assign(`/connections/${encodeURIComponent(c.id)}`); }} className="cursor-pointer hover:bg-slate-800/70" style={{ background: i % 2 === 0 ? "#0f172a" : "#111827", borderBottom: i < connections.length - 1 ? "1px solid #1f2937" : undefined }}>
                        <td className="px-5 py-3">
                          <div className="flex items-center gap-3">
                            <div
                              className="flex items-center justify-center rounded-lg flex-shrink-0"
                              style={{ width: 36, height: 36, background: colors.accent + "18", color: colors.accent }}
                            >
                              {logo}
                            </div>
                            <div>
                              <Link href={`/connections/${encodeURIComponent(c.id)}`} className="font-medium text-white hover:underline">{c.name}</Link>
                              <div className="text-xs mt-0.5" style={{ color: "#6b7280" }}>
                                {c.provider_kind}
                                {c.base_url ? ` · ${c.base_url}` : ""}
                              </div>
                              {rt?.result && (
                                <div className="text-xs mt-1" style={{ color: rt.result.ok ? "#4ade80" : "#fca5a5" }}>
                                  {rt.result.ok
                                    ? `OK${rt.result.latency_ms != null ? ` · ${rt.result.latency_ms}ms` : ""}${rt.result.discovered_models != null ? ` · ${rt.result.discovered_models} models` : ""}`
                                    : `Failed: ${rt.result.error ?? "unknown error"}`}
                                </div>
                              )}
                            </div>
                          </div>
                        </td>
                        <td className="px-5 py-3">
                          {c.prefix ? (
                            <code className="text-xs px-1.5 py-0.5 rounded font-mono" style={{ background: "#052e16", color: "#70CD96" }}>{c.prefix}</code>
                          ) : (
                            <span className="text-xs" style={{ color: "#4b5563" }}>—</span>
                          )}
                        </td>
                        <td className="px-5 py-3">
                          <span className="text-xs px-1.5 py-0.5 rounded" style={EXPOSURE_COLORS[c.exposure]}>{c.exposure}</span>
                        </td>
                        <td className="px-5 py-3">
                          {c.routing_enabled ? (
                            <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#052e16", color: "#4ade80" }}>on</span>
                          ) : (
                            <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#1f2937", color: "#6b7280" }}>off</span>
                          )}
                        </td>
                        <td className="px-5 py-3">
                          <div className="flex flex-wrap gap-1.5">
                            {c.state.configured ? (
                              <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#052e16", color: "#4ade80" }}>configured</span>
                            ) : (
                              <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#1f2937", color: "#6b7280" }}>not configured</span>
                            )}
                            {c.state.unreadable && (
                              <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#1c0a0a", color: "#fca5a5" }}>unreadable</span>
                            )}
                            {c.state.healthy === true && (
                              <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#052e16", color: "#70CD96" }}>healthy</span>
                            )}
                            {c.state.healthy === false && (
                              <span className="text-xs px-1.5 py-0.5 rounded" style={{ background: "#1c0a0a", color: "#ef4444" }}>unhealthy</span>
                            )}
                          </div>
                        </td>
                        <td className="sticky right-0 bg-inherit px-5 py-3 text-right">
                          <div className="flex items-center justify-end gap-2">
                            <IconButton icon="activity" label={rt?.loading ? "Testing connection" : "Test connection"} onClick={() => handleTestRow(c.id)} disabled={rt?.loading} />
                            <IconButton icon="edit" label="Edit connection" onClick={() => openEdit(c)} />
                            <IconButton icon="trash" label="Delete connection" tone="danger" onClick={() => setConnectionPendingDeletion(c)} disabled={deletingConnectionId === c.id} />
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
                <p className="text-sm" style={{ color: "#6b7280" }}>No provider connections yet.</p>
              </div>
            )
          )}

          <InfiniteScroll hasMore={Boolean(nextCursor)} loading={loading} onLoadMore={() => void handleNextPage()} />

          <Modal open={showForm} onClose={() => { if (!saving && !testing) closeForm(); }} title={editingId ? "Edit connection" : "New connection"} size="xl">
            <div>

              {saveError && (
                <div className="mb-4 px-3 py-2 rounded-lg text-xs" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>
                  {saveError}
                </div>
              )}

              <fieldset disabled={saving || testing} className="space-y-4">
                <div className="grid grid-cols-2 gap-4">
                  <div>
                    <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>Name</label>
                    <input type="text" value={form.name} onChange={(e) => {
                      const name = e.target.value;
                      setForm((f) => ({
                        ...f,
                        name,
                        prefix: prefixEdited ? f.prefix : suggestedPrefix(f.provider_kind, name),
                      }));
                    }} placeholder="e.g. OpenAI (prod)" className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle} />
                  </div>
                  <div>
                    <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>Provider kind</label>
                    <select
                      value={form.provider_kind}
                      disabled={editingId != null}
                      onChange={(e) => {
                        const providerKind = e.target.value;
                        setPrefixEdited(false);
                        setForm((f) => ({
                          ...f,
                          provider_kind: providerKind,
                          prefix: suggestedPrefix(providerKind, f.name),
                          base_url: providerKind === "vertex" ? "" : f.base_url,
                        }));
                      }}
                      className="w-full rounded-md px-3 py-2 text-sm focus:outline-none disabled:opacity-60"
                      style={inputStyle}
                    >
                      {KIND_OPTIONS.map((k) => <option key={k} value={k}>{k}</option>)}
                    </select>
                  </div>
                </div>

                {isVertex && (
                  <div>
                    <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>Vertex location</label>
                    <select
                      value={form.base_url === VERTEX_GLOBAL_URL ? "global" : (!form.base_url || form.base_url === VERTEX_US_CENTRAL_URL ? "us-central1" : "custom")}
                      onChange={(e) => setForm((f) => ({
                        ...f,
                        base_url: e.target.value === "global" ? VERTEX_GLOBAL_URL : e.target.value === "us-central1" ? "" : f.base_url,
                      }))}
                      className="w-full rounded-md px-3 py-2 text-sm focus:outline-none"
                      style={inputStyle}
                    >
                      <option value="us-central1">us-central1</option>
                      <option value="global">global</option>
                      <option value="custom">Custom endpoint</option>
                    </select>
                  </div>
                )}

                {showsBaseUrl(form.provider_kind) && (
                  <div>
                    <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>
                      Base URL{" "}
                      <span style={{ color: "#4b5563" }}>({requiresBaseUrl(form.provider_kind) ? "required" : "optional"})</span>
                    </label>
                    <input type="text" value={form.base_url} onChange={(e) => setForm((f) => ({ ...f, base_url: e.target.value }))} placeholder="https://..." className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle} />
                  </div>
                )}

                <div className="grid grid-cols-2 gap-4">
                  <div>
                    <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>Exposure</label>
                    <select value={form.exposure} onChange={(e) => setForm((f) => ({ ...f, exposure: e.target.value as Exposure }))} className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle}>
                      <option value="dynamic">Dynamic (prefix routing + listed)</option>
                      <option value="unlisted">Unlisted (prefix routing, hidden)</option>
                      <option value="explicit">Explicit (routes only)</option>
                    </select>
                  </div>
                  <div>
                    <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>
                      Prefix{" "}
                      <span style={{ color: "#4b5563" }}>({form.exposure === "explicit" ? "optional" : "required"})</span>
                    </label>
                    <input type="text" value={form.prefix} onChange={(e) => {
                      setPrefixEdited(true);
                      setForm((f) => ({ ...f, prefix: e.target.value }));
                    }} placeholder="e.g. openai/" className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle} />
                    {form.exposure === "explicit" && (
                      <p className="mt-1.5 text-xs" style={{ color: "#6b7280" }}>
                        Explicit connections ignore prefix routing and are reachable only through <code>chalk/*</code> model routes.
                      </p>
                    )}
                  </div>
                </div>

                <div>
                  <label className="block text-xs font-medium mb-1.5" style={{ color: "#9ca3af" }}>
                    {isVertex ? "Service account key" : "API key"}{" "}
                    <span style={{ color: "#4b5563" }}>
                      ({editingId
                        ? "leave blank to keep current"
                        : form.provider_kind === "vllm" ||
                            form.provider_kind === "openai-compatible" ||
                            (form.provider_kind === "bedrock" && isBedrockRuntimeConnection(form.base_url.trim()))
                          ? "optional"
                          : "required"})
                    </span>
                  </label>
                  {isVertex ? (
                    <>
                      <textarea
                        value={form.api_key}
                        onChange={(e) => setForm((f) => ({ ...f, api_key: e.target.value }))}
                        rows={5}
                        spellCheck={false}
                        placeholder={'{\n  "type": "service_account",\n  "project_id": "...",\n  "private_key": "-----BEGIN PRIVATE KEY-----\\n..."\n}'}
                        className="w-full rounded-md px-3 py-2 text-xs font-mono focus:outline-none"
                        style={inputStyle}
                      />
                      <p className="mt-1.5 text-xs" style={{ color: "#6b7280" }}>
                        Vertex AI takes no API key. Paste the GCP service account key JSON — the router
                        mints short-lived OAuth tokens from it and refreshes them automatically. The
                        project comes from the key; add <code>&quot;location&quot;</code> to it, or set the
                        Base URL to another <code>https://&lt;region&gt;-aiplatform.googleapis.com</code>,
                        to serve a different region.
                      </p>
                    </>
                  ) : (
                    <input type="password" value={form.api_key} onChange={(e) => setForm((f) => ({ ...f, api_key: e.target.value }))} placeholder="sk-..." className="w-full rounded-md px-3 py-2 text-sm focus:outline-none" style={inputStyle} />
                  )}
                </div>

                <details className="rounded-lg p-4" style={{ background: "#0f172a", border: "1px solid #374151" }}>
                  <summary className="cursor-pointer text-sm font-medium" style={{ color: "#d1d5db" }}>
                    Advanced capability declaration
                  </summary>
                  <p className="mt-2 text-xs" style={{ color: "#6b7280" }}>
                    Leave blank to inherit built-in and discovered facts. Use <code>model_defaults</code>, canonical upstream ids under <code>models</code>, and <code>endpoint.interfaces</code>; <code>unknown</code> is an explicit value.
                  </p>
                  <textarea
                    value={form.declared_capabilities}
                    onChange={(e) => setForm((current) => ({ ...current, declared_capabilities: e.target.value }))}
                    rows={10}
                    spellCheck={false}
                    placeholder={'{\n  "schema_version": 1,\n  "model_defaults": {\n    "features": {\n      "tool_calling": "supported"\n    }\n  },\n  "models": {},\n  "endpoint": {\n    "interfaces": {}\n  }\n}'}
                    className="mt-3 w-full rounded-md px-3 py-2 text-xs font-mono focus:outline-none"
                    style={inputStyle}
                  />
                </details>

                <label className="flex items-center gap-2 text-sm" style={{ color: "#9ca3af" }}>
                  <input type="checkbox" checked={form.routing_enabled} onChange={(e) => setForm((f) => ({ ...f, routing_enabled: e.target.checked }))} />
                  Routing enabled
                </label>

                {testResult && (
                  <div
                    className="px-3 py-2 rounded-lg text-xs"
                    style={{
                      background: testResult.ok ? "#052e16" : "#1c0a0a",
                      border: `1px solid ${testResult.ok ? "#166534" : "#7f1d1d"}`,
                      color: testResult.ok ? "#70CD96" : "#fca5a5",
                    }}
                  >
                    {testResult.ok
                      ? `Connection OK${testResult.latency_ms != null ? ` · ${testResult.latency_ms}ms` : ""}${testResult.discovered_models != null ? ` · ${testResult.discovered_models} models discovered` : ""}`
                      : `Test failed: ${testResult.error ?? "unknown error"}`}
                  </div>
                )}

                <div className="flex justify-between items-center">
                  <button
                    onClick={() => void testDraft()}
                    disabled={saving || testing}
                    className="px-4 py-2 rounded-lg text-sm transition-colors disabled:opacity-50"
                    style={{ background: "#1f2937", color: "#e5e7eb", border: "1px solid #374151" }}
                  >
                    {testing ? "Testing..." : "Test connection"}
                  </button>
                  <button
                    onClick={handleSave}
                    disabled={saveDisabled}
                    className="px-5 py-2 rounded-lg text-sm font-medium transition-colors disabled:opacity-50"
                    style={{ background: "#218E61", color: "white" }}
                  >
                    {saving ? testing ? "Testing..." : "Saving..." : editingId ? "Save changes" : "Create connection"}
                  </button>
                </div>
              </fieldset>
            </div>
          </Modal>
        </>
      )}
      <ConfirmationModal
        open={connectionPendingDeletion !== null}
        title="Delete provider connection?"
        description={`This removes ${connectionPendingDeletion?.name ?? "this provider connection"} and may make its routes unavailable.`}
        confirmLabel="Delete connection"
        confirming={connectionPendingDeletion?.id === deletingConnectionId}
        onCancel={() => setConnectionPendingDeletion(null)}
        onConfirm={async () => {
          if (connectionPendingDeletion && await handleDelete(connectionPendingDeletion)) {
            setConnectionPendingDeletion(null);
          }
        }}
      />
    </div>
  );
}

async function errorText(res: Response): Promise<string> {
  try {
    const data = await res.json();
    return data?.error?.message ?? `${res.status} ${res.statusText}`;
  } catch {
    return `${res.status} ${res.statusText}`;
  }
}
