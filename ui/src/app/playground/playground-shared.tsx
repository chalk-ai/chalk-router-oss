"use client";

import { useCallback } from "react";
import { useRouter, useSearchParams } from "next/navigation";

export type Tab = "chat" | "decisions" | "embeddings" | "images";

export interface Model {
  id: string;
  owned_by: string;
  capabilities?: {
    playground?: {
      chat_support?: Support;
      chat_protocol: "openai_responses" | "openai_chat_completions" | null;
    };
    schema_version?: number;
    surfaces?: {
      chat?: Support;
      decisions?: Support;
      embeddings?: "supported" | "unsupported" | "unknown";
      image_generation?: "supported" | "unsupported" | "unknown";
    };
    modalities?: {
      input?: { text?: Support; image?: Support };
      output?: { text?: Support; decisions?: Support; image?: Support; embeddings?: Support };
    };
    limits?: {
      max_input_tokens?: number;
      embedding_input?: {
        max_images_per_document?: number;
        image_mime_types?: string[];
      };
    };
    interfaces?: {
      gemini_interactions?: {
        operations?: { create?: OperationCapability };
      };
      openai_responses?: {
        operations?: { create?: OperationCapability };
      };
      openai_chat_completions?: {
        operations?: { create?: OperationCapability };
      };
      openai_embeddings?: {
        operations?: { create?: OperationCapability };
      };
      router_decisions?: {
        operations?: { create?: OperationCapability };
      };
      openai_images?: {
        operations?: { generate?: OperationCapability };
      };
    };
  };
}

type Support = "supported" | "unsupported" | "unknown";

interface ParameterCapability {
  support: Support;
  constraint?:
    | { type: "any" | "unknown" }
    | { type: "string_enum"; values: string[] }
    | { type: "integer_range"; min: number; max: number }
    | { type: "number_range"; min: number; max: number };
}

interface OperationCapability {
  support?: Support;
  features?: Record<string, Support>;
  parameters?: Record<string, ParameterCapability>;
}

type CapabilityDecision = Support;

export interface ModelGroups {
  supported: Model[];
  unverified: Model[];
}

function embeddingNameHeuristic(model: Model): boolean {
  return (
    model.id.includes("embed") ||
    model.id.includes("ada") ||
    model.id.includes("voyage")
  );
}

function imageNameHeuristic(model: Model): boolean {
  return (
    model.id.includes("dall-e") ||
    model.id.includes("image") ||
    model.id.includes("gpt-image")
  );
}

function chatNameHeuristic(model: Model): boolean {
  return !/embed|ada|voyage|image|dall-e|imagen/i.test(model.id);
}

function decisionsNameHeuristic(model: Model): boolean {
  return /jev|system.?one|decision/i.test(model.id);
}

function required(...values: Array<Support | undefined>): CapabilityDecision {
  if (values.some((value) => value === "unsupported")) return "unsupported";
  if (values.every((value) => value === "supported")) return "supported";
  return "unknown";
}

function alternative(...values: Array<Support | undefined>): CapabilityDecision {
  if (values.some((value) => value === "supported")) return "supported";
  if (values.every((value) => value === "unsupported")) return "unsupported";
  return "unknown";
}

function modelDecision(model: Model, tab: Tab): CapabilityDecision {
  const capabilities = model.capabilities;
  if (!capabilities) return "unknown";

  if (tab === "chat") {
    const playground = capabilities.playground;
    if (playground?.chat_support) return playground.chat_support;
    if (playground) return playground.chat_protocol ? "supported" : "unsupported";
    return required(
      capabilities.surfaces?.chat,
      capabilities.modalities?.input?.text,
      capabilities.modalities?.output?.text,
      alternative(
        usesGeminiInteractions(model) ? "supported" : "unsupported",
        capabilities.interfaces?.openai_responses?.operations?.create?.support,
        capabilities.interfaces?.openai_chat_completions?.operations?.create?.support,
      ),
    );
  }
  if (tab === "embeddings") {
    return required(
      capabilities.surfaces?.embeddings,
      alternative(capabilities.modalities?.input?.text, capabilities.modalities?.input?.image),
      capabilities.modalities?.output?.embeddings,
      capabilities.interfaces?.openai_embeddings?.operations?.create?.support,
    );
  }
  if (tab === "decisions") {
    return required(
      capabilities.surfaces?.decisions,
      capabilities.modalities?.input?.text,
      capabilities.modalities?.output?.decisions,
      capabilities.interfaces?.router_decisions?.operations?.create?.support,
    );
  }
  return required(
    capabilities.surfaces?.image_generation,
    capabilities.modalities?.input?.text,
    capabilities.modalities?.output?.image,
    alternative(
      capabilities.interfaces?.gemini_interactions?.operations?.create?.support,
      capabilities.interfaces?.openai_images?.operations?.generate?.support,
    ),
  );
}

export function modelsForTab(models: Model[], tab: Tab): ModelGroups {
  const heuristic =
    tab === "chat"
      ? chatNameHeuristic
      : tab === "decisions"
        ? decisionsNameHeuristic
      : tab === "embeddings"
        ? embeddingNameHeuristic
        : imageNameHeuristic;
  return models.reduce<ModelGroups>(
    (groups, model) => {
      const decision = modelDecision(model, tab);
      if (decision === "supported") groups.supported.push(model);
      else if (decision === "unknown" && heuristic(model)) groups.unverified.push(model);
      return groups;
    },
    { supported: [], unverified: [] },
  );
}

// Unknown parameter support preserves existing behavior; only explicit exclusions disable a control.
export function supportsChatParameter(model: Model | undefined, modelId: string, parameter: string): boolean {
  const protocol = usesGeminiInteractions(model)
    ? "gemini_interactions"
    : usesResponsesApi(model, modelId) ? "openai_responses" : "openai_chat_completions";
  return model?.capabilities?.interfaces?.[protocol]?.operations?.create?.parameters?.[parameter]?.support !== "unsupported";
}

export function supportedChatParameters(model: Model | undefined, modelId: string, parameters: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(parameters).filter(([parameter]) => supportsChatParameter(model, modelId, parameter)));
}

/**
 * Prefer the resolved public capability profile. The id heuristic remains only
 * for legacy model objects that omit capabilities entirely.
 */
export function usesResponsesApi(model: Model | undefined, modelId: string): boolean {
  if (usesGeminiInteractions(model)) return false;
  const protocol = model?.capabilities?.playground?.chat_protocol;
  if (protocol) return protocol === "openai_responses";
  const responses =
    model?.capabilities?.interfaces?.openai_responses?.operations?.create?.support;
  const chat =
    model?.capabilities?.interfaces?.openai_chat_completions?.operations?.create?.support;
  if (responses === "supported") return true;
  if (chat === "supported" || responses === "unsupported") return false;
  return !modelId.includes("/") || modelId.startsWith("openai/");
}

/** Native Gemini is selected through server capabilities, never a model-name guess. */
export function usesGeminiInteractions(model: Model | undefined): boolean {
  const operation = model?.capabilities?.interfaces?.gemini_interactions?.operations?.create;
  return operation?.support === "supported" && operation.features?.streaming !== "unsupported";
}

/** Image generation does not require streaming. An advertised native operation
 * is eligible unless explicitly disabled, including unverified catalog entries. */
export function usesGeminiImageInteractions(model: Model | undefined): boolean {
  const operation = model?.capabilities?.interfaces?.gemini_interactions?.operations?.create;
  return operation !== undefined && operation.support !== "unsupported";
}

/** Normalized token-usage figures shared across the playground's protocols. */
export interface UsageInfo {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

export async function readResponsesTextStream(
  response: Response,
  onDelta: (delta: string) => void,
  onUsage?: (usage: UsageInfo) => void,
): Promise<void> {
  const reader = response.body?.getReader();
  if (!reader) throw new Error("Response stream was empty");

  const decoder = new TextDecoder();
  let buffer = "";
  let sawTerminalEvent = false;

  const processFrame = (frame: string) => {
    const lines = frame.split(/\r\n|\r|\n/);
    const eventName = lines
      .find((line) => line.startsWith("event:"))
      ?.slice("event:".length)
      .trim();
    const data = lines
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice("data:".length).trimStart())
      .join("\n");
    if (!data) return;

    let event: {
      type?: string;
      delta?: string;
      message?: string;
      error?: { message?: string };
      response?: {
        error?: { message?: string };
        incomplete_details?: { reason?: string };
        usage?: { input_tokens?: number; output_tokens?: number; total_tokens?: number };
      };
    };
    try {
      event = JSON.parse(data) as typeof event;
    } catch {
      const eventContext = eventName ? ` (${eventName})` : "";
      throw new Error(`Malformed Responses stream event${eventContext}`);
    }
    const eventType = event.type ?? eventName;

    if (eventType === "response.output_text.delta" && event.delta) {
      onDelta(event.delta);
      return;
    }
    if (eventType === "response.completed") {
      sawTerminalEvent = true;
      const usage = event.response?.usage;
      if (onUsage && usage) {
        onUsage({
          promptTokens: usage.input_tokens ?? 0,
          completionTokens: usage.output_tokens ?? 0,
          totalTokens: usage.total_tokens ?? 0,
        });
      }
      return;
    }
    if (eventType === "error" || eventType === "response.failed") {
      sawTerminalEvent = true;
      throw new Error(
        event.message ??
          event.error?.message ??
          event.response?.error?.message ??
          "The response failed",
      );
    }
    if (eventType === "response.incomplete") {
      sawTerminalEvent = true;
      throw new Error(
        event.response?.incomplete_details?.reason ?? "The response was incomplete",
      );
    }
  };

  while (true) {
    const { done, value } = await reader.read();
    buffer += decoder.decode(value, { stream: !done });

    let separator = buffer.match(/(?:\r\n|\r|\n){2}/);
    while (separator?.index !== undefined) {
      processFrame(buffer.slice(0, separator.index));
      buffer = buffer.slice(separator.index + separator[0].length);
      separator = buffer.match(/(?:\r\n|\r|\n){2}/);
    }

    if (done) break;
  }

  if (buffer.trim()) processFrame(buffer);
  if (!sawTerminalEvent) {
    throw new Error("Response stream closed before a terminal event");
  }
}

/** Update a single query param without a full navigation. */
export function useQueryParam(key: string, defaultValue: string): [string, (v: string) => void] {
  const searchParams = useSearchParams();
  const router = useRouter();
  const value = searchParams.get(key) ?? defaultValue;

  const setValue = useCallback(
    (v: string) => {
      const params = new URLSearchParams(searchParams.toString());
      if (v === defaultValue) {
        params.delete(key);
      } else {
        params.set(key, v);
      }
      const qs = params.toString();
      router.replace(qs ? `?${qs}` : ".", { scroll: false });
    },
    [key, defaultValue, searchParams, router],
  );

  return [value, setValue];
}

export function useQueryParamNumber(key: string, defaultValue: number): [number, (v: number) => void] {
  const [raw, setRaw] = useQueryParam(key, String(defaultValue));
  const value = Number(raw) || defaultValue;
  const setValue = useCallback((v: number) => setRaw(String(v)), [setRaw]);
  return [value, setValue];
}

function modelOptionLabel(model: Model): string {
  const provider = model.owned_by.trim();
  const id = model.id.trim();
  const normalizedProvider = provider.toLowerCase();
  const normalizedId = id.toLowerCase();
  if (
    !provider ||
    normalizedId === normalizedProvider ||
    normalizedId.startsWith(`${normalizedProvider}/`)
  ) {
    return model.id;
  }
  return `${model.id} (${provider})`;
}

export function ModelSelect({
  groups,
  value,
  onChange,
}: {
  groups: ModelGroups;
  value: string;
  onChange: (v: string) => void;
}) {
  const listed = [...groups.supported, ...groups.unverified].some(
    (model) => model.id === value,
  );
  return (
    <div className="flex items-center gap-2 flex-wrap">
      <select
        aria-label="Model"
        value={listed ? value : ""}
        onChange={(e) => onChange(e.target.value)}
        className="text-sm rounded-md px-3 py-2 outline-none"
        style={{
          background: "var(--color-cds-field)",
          border: "1px solid var(--color-cds-border)",
          color: "var(--color-cds-ink)",
          minWidth: "220px",
        }}
      >
        <option value="">Select a model...</option>
        {groups.supported.length > 0 && (
          <optgroup label="Supported">
            {groups.supported.map((model) => (
              <option key={model.id} value={model.id}>
                {modelOptionLabel(model)}
              </option>
            ))}
          </optgroup>
        )}
        {groups.unverified.length > 0 && (
          <optgroup label="Unverified">
            {groups.unverified.map((model) => (
              <option key={model.id} value={model.id}>
                {modelOptionLabel(model)}
              </option>
            ))}
          </optgroup>
        )}
      </select>
      <input
        type="text"
        value={listed ? "" : value}
        onChange={(event) => onChange(event.target.value)}
        placeholder="Or enter a model ID"
        aria-label="Manual model ID"
        className="text-sm rounded-md px-3 py-2 outline-none"
        style={{
          background: "var(--color-cds-field)",
          border: "1px solid var(--color-cds-border)",
          color: "var(--color-cds-ink)",
          minWidth: "220px",
        }}
      />
    </div>
  );
}
