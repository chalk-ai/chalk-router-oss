import { routeGridFromProto, routeReferencedTargets } from "@/lib/routeGrid";
import {
  apiKeyClient,
  modelRouteClient,
  providerConnectionClient,
  usagePoolClient,
} from "@/lib/grpc";
import { fetchAllGrpcPages } from "@/lib/grpcPagination";
import UsageExplorer, { type UsageExplorerOptions } from "./UsageExplorer";
import { PageHeader } from "@/components/router/RouterPrimitives";


const PROVIDER_LABELS: Record<string, string> = {
  anthropic: "Anthropic",
  bedrock: "Bedrock",
  cohere: "Cohere",
  fireworks: "Fireworks AI",
  gemini: "Gemini",
  groq: "Groq",
  mistral: "Mistral AI",
  perplexity: "Perplexity",
  openai: "OpenAI",
  "openai-compatible": "OpenAI-compatible",
  openrouter: "OpenRouter",
  qwen: "Qwen",
  together: "Together AI",
  vertex: "Vertex AI",
  vllm: "vLLM",
  voyage: "Voyage AI",
  xai: "xAI",
};

function providerLabel(providerKind: string): string {
  return PROVIDER_LABELS[providerKind] ?? providerKind
    .split(/[-_]/)
    .filter(Boolean)
    .map((part) => part[0]?.toUpperCase() + part.slice(1))
    .join(" ");
}

function modelLabel(upstreamModelId: string): string {
  const leaf = upstreamModelId.split("/").filter(Boolean).at(-1) ?? upstreamModelId;
  const recognizableModel = leaf.match(
    /(?:^|\.)(claude|gpt|o[134]|gemini|gemma|llama|mistral|mixtral|qwen|deepseek|grok|nova|titan|command|embed|jamba)(?:[._-]|\d|$)/i,
  );
  let name = recognizableModel ? leaf.slice(recognizableModel.index! + recognizableModel[0].search(/[a-z0-9]/i)) : leaf;
  name = name
    .replace(/@\d{8}$/i, "")
    .replace(/[-_.]20\d{6}(?=[-_.:]|$)/g, "")
    .replace(/-20\d{2}-\d{2}-\d{2}$/g, "")
    .replace(/[-_.]v\d+:\d+$/i, "")
    .replace(/:\d+$/i, "")
    .replace(/(\d+)-(\d+)(?=-|$)/g, "$1.$2");

  const protectedDecimals = name.replace(/(\d)\.(\d)/g, "$1\u0000$2");
  const words = protectedDecimals
    .split(/[-_.:]+/)
    .filter(Boolean)
    .map((word) => {
      const restored = word.replaceAll("\u0000", ".");
      const lower = restored.toLowerCase();
      if (lower === "gpt") return "GPT";
      if (lower === "ai") return "AI";
      if (lower === "vl") return "VL";
      if (/^\d+(?:\.\d+)?b$/i.test(restored)) return restored.toUpperCase();
      return restored[0]?.toUpperCase() + restored.slice(1);
    });
  if (words[0] === "GPT" && words[1]) {
    words.splice(0, 2, `GPT-${words[1]}`);
  }
  return words.join(" ") || upstreamModelId;
}

function listAllModelRoutes() {
  return fetchAllGrpcPages(
    (cursor) => modelRouteClient().listModelRoutes({ limit: 1000, cursor, filters: {} }),
    (page) => page.routes,
  );
}

export default async function UsageContent({ embedded = false }: { embedded?: boolean } = {}) {
  let options: UsageExplorerOptions = {
    apiKeys: [],
    connections: [],
    models: [],
    usagePools: [],
  };
  let loadError: string | null = null;

  try {
    const [keys, pools, connections] = await Promise.all([
      fetchAllGrpcPages(
        (cursor) => apiKeyClient().listKeys({ limit: 1000, cursor }),
        (page) => page.keys,
      ),
      fetchAllGrpcPages(
        (cursor) => usagePoolClient().listUsagePools({ limit: 1000, cursor }),
        (page) => page.pools,
      ),
      fetchAllGrpcPages(
        (cursor) => providerConnectionClient().listProviderConnections({ limit: 1000, cursor }),
        (page) => page.connections,
      ),
    ]);

    const [catalog, routes] = await Promise.all([
      providerConnectionClient().listProviderConnectionModels({ filters: {} }),
      listAllModelRoutes(),
    ]);
    const connectionsById = new Map(
      connections.map((connection) => [connection.id, connection]),
    );
    const models = new Map<string, { label: string; contexts: Set<string> }>();
    const addModel = (upstreamModelId: string, connectionId: string) => {
      const option = models.get(upstreamModelId) ?? {
        label: modelLabel(upstreamModelId),
        contexts: new Set<string>(),
      };
      const connection = connectionsById.get(connectionId);
      if (connection) {
        const provider = providerLabel(connection.providerKind);
        const normalizedConnectionName = connection.name.trim().toLowerCase();
        const providerOnly = normalizedConnectionName === connection.providerKind.toLowerCase()
          || normalizedConnectionName === provider.toLowerCase();
        option.contexts.add(providerOnly ? provider : `${provider} · ${connection.name}`);
      } else {
        option.contexts.add(connectionId);
      }
      models.set(upstreamModelId, option);
    };
    for (const model of catalog.models) {
      addModel(model.upstreamModelId, model.connectionId);
    }
    for (const route of routes) {
      for (const target of routeReferencedTargets({ connection_id: route.connectionId, upstream_model_id: route.upstreamModelId, routing: routeGridFromProto(route.routing) })) {
        addModel(target.upstream_model_id, target.connection_id);
      }
    }

    const modelOptions = [...models].map(([value, option]) => ({ value, ...option }));
    const labelCounts = new Map<string, number>();
    for (const option of modelOptions) {
      const key = option.label.toLocaleLowerCase();
      labelCounts.set(key, (labelCounts.get(key) ?? 0) + 1);
    }

    options = {
      apiKeys: keys.map((key) => ({
        value: key.keyHash,
        label: key.description || key.keyPrefix || key.keyHash.slice(0, 12),
        labels: key.labels,
      })),
      connections: connections.map((connection) => ({
        value: connection.id,
        label: `${connection.name} (${connection.providerKind})`,
      })),
      models: modelOptions
        .map(({ value, label, contexts }) => ({
          value,
          label,
          secondaryLabel: contexts.size > 1 || (labelCounts.get(label.toLocaleLowerCase()) ?? 0) > 1
            ? [...contexts].sort().join("; ")
            : undefined,
          title: value,
        }))
        .sort((a, b) => a.label.localeCompare(b.label) || a.value.localeCompare(b.value)),
      usagePools: pools.map((pool) => ({ value: pool.id, label: pool.name })),
    };
  } catch (error) {
    loadError = error instanceof Error ? error.message : String(error);
  }

  return (
    <div>
      <div className="mb-6"><PageHeader title="Usage" description="Explore token and cache usage across your router over time. Dates are UTC." /></div>

      {loadError && (
        <div
          className="mb-6 rounded-lg px-4 py-3 text-sm"
          style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}
        >
          <span className="font-medium">Some filter choices could not be loaded:</span> {loadError}
        </div>
      )}

      <UsageExplorer options={options} />
    </div>
  );
}
