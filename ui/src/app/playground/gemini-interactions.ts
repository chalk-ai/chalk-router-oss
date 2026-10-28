import type { UsageInfo } from "./playground-shared";

// Preserve native steps separately from the display transcript. Signatures,
// unknown fields, and step ordering must survive the next client request.
export type GeminiStep = Record<string, unknown> & { type: string };

export function buildGeminiRequest(
  model: string,
  history: GeminiStep[],
  input: string,
  systemInstruction: string,
  generationConfig: Record<string, unknown>,
) {
  return {
    model,
    input: [...history, { type: "user_input", content: [{ type: "text", text: input }] }],
    system_instruction: systemInstruction || undefined,
    generation_config: generationConfig,
    stream: true,
    store: false,
  };
}

function object(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function step(value: unknown): GeminiStep {
  const record = object(value);
  if (!record || typeof record.type !== "string") throw new Error("Invalid Gemini response step");
  return record as GeminiStep;
}

function textParts(value: unknown): string {
  return Array.isArray(value)
    ? value.map((part) => object(part)?.type === "text" ? object(part)?.text ?? "" : "").join("")
    : "";
}

function usageInfo(value: unknown): UsageInfo | undefined {
  const usage = object(value);
  if (!usage) return undefined;
  const count = (key: string) => typeof usage[key] === "number" ? usage[key] : 0;
  const promptTokens = count("total_input_tokens") + count("total_tool_use_tokens");
  const completionTokens = count("total_output_tokens") + count("total_thought_tokens");
  return {
    promptTokens,
    completionTokens,
    totalTokens: typeof usage.total_tokens === "number" ? usage.total_tokens : promptTokens + completionTokens,
  };
}

/** Reassemble native text/thought steps for stateless replay; never display signatures. */
export async function readGeminiStream(
  response: Response,
  onDelta: (delta: string) => void,
): Promise<{ steps: GeminiStep[]; usage?: UsageInfo }> {
  const reader = response.body?.getReader();
  if (!reader) throw new Error("Response stream was empty");
  const decoder = new TextDecoder();
  const steps = new Map<number, GeminiStep>();
  const argumentsByStep = new Map<number, string>();
  const stopped = new Set<number>();
  let finalSteps: GeminiStep[] | undefined;
  let usage: UsageInfo | undefined;
  let completed = false;
  let buffer = "";

  const failStatus = (status: unknown) => {
    if (status === "requires_action") throw new Error("This response needs a tool result. The playground does not execute tools.");
    if (["failed", "cancelled", "incomplete", "budget_exceeded"].includes(String(status))) {
      throw new Error(`Gemini response ${status}`);
    }
  };

  const processFrame = (frame: string) => {
    const lines = frame.split(/\r\n|\r|\n/);
    const data = lines.filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).trimStart()).join("\n");
    if (!data) return;
    let event: Record<string, unknown> | undefined;
    try {
      event = object(JSON.parse(data));
    } catch {
      throw new Error("Malformed Gemini stream event");
    }
    if (!event) throw new Error("Malformed Gemini stream event");
    const kind = event.event_type ?? lines.find((line) => line.startsWith("event:"))?.slice(6).trim();
    const interaction = object(event.interaction);
    usage = usageInfo(interaction?.usage ?? event.usage ?? object(event.metadata)?.total_usage) ?? usage;
    if (kind === "error") {
      throw new Error(String(object(event.error)?.message ?? "Gemini request failed"));
    }
    if (kind === "interaction.status_update") failStatus(event.status);
    if (kind === "interaction.completed") {
      failStatus(interaction?.status);
      if (Array.isArray(interaction?.steps)) {
        finalSteps = interaction.steps.map(step);
        // Some providers send the final steps without preceding text deltas.
        if (steps.size === 0) finalSteps.forEach((item) => {
          if (item.type === "model_output") onDelta(textParts(item.content));
        });
      } else if ([...steps.keys()].some((index) => !stopped.has(index))) {
        throw new Error("Gemini response ended with unfinished steps");
      }
      if (!finalSteps && steps.size === 0) throw new Error("Gemini response contained no steps");
      completed = true;
      return;
    }
    if (!["step.start", "step.delta", "step.stop"].includes(String(kind))) return;
    const index = event.index;
    if (typeof index !== "number" || !Number.isInteger(index) || index < 0) {
      throw new Error("Invalid Gemini step index");
    }
    if (kind === "step.start") {
      if (steps.has(index)) throw new Error("Duplicate Gemini step");
      const value = step(event.step);
      steps.set(index, value);
      if (value.type === "model_output") onDelta(textParts(value.content));
      return;
    }
    const current = steps.get(index);
    if (!current || stopped.has(index)) throw new Error("Gemini step arrived out of order");
    if (kind === "step.stop") {
      const args = argumentsByStep.get(index);
      if (args !== undefined) {
        try {
          current.arguments = JSON.parse(args);
        } catch {
          throw new Error("Malformed Gemini tool arguments");
        }
      }
      stopped.add(index);
      return;
    }
    const delta = object(event.delta);
    if (!delta) throw new Error("Invalid Gemini step delta");
    switch (delta.type) {
      case "text":
      case "text_delta": {
        if (typeof delta.text !== "string" || current.type !== "model_output") {
          throw new Error("Unexpected Gemini text delta");
        }
        const content = Array.isArray(current.content) ? current.content : [];
        const last = object(content[content.length - 1]);
        if (last?.type === "text") last.text = String(last.text ?? "") + delta.text;
        else content.push({ ...delta, type: "text" });
        current.content = content;
        onDelta(delta.text);
        break;
      }
      case "thought_signature":
      case "thought_signature_delta":
        if (typeof delta.signature !== "string") throw new Error("Invalid Gemini thought signature");
        current.signature = String(current.signature ?? "") + delta.signature;
        break;
      case "thought_summary":
      case "thought_summary_delta":
        if (!object(delta.content)) throw new Error("Invalid Gemini thought summary");
        current.summary = [...(Array.isArray(current.summary) ? current.summary : []), delta.content];
        break;
      case "arguments_delta":
        if (typeof delta.arguments !== "string") throw new Error("Invalid Gemini tool arguments");
        argumentsByStep.set(index, (argumentsByStep.get(index) ?? "") + delta.arguments);
        break;
      case "text_annotation_delta": {
        const content = Array.isArray(current.content) ? current.content : [];
        const last = object(content[content.length - 1]);
        if (!last || !Array.isArray(delta.annotations)) throw new Error("Invalid Gemini text annotations");
        last.annotations = [...(Array.isArray(last.annotations) ? last.annotations : []), ...delta.annotations];
        break;
      }
      default:
        // Silently discarding an unknown delta would corrupt signed history.
        throw new Error("This Gemini response contains content the playground cannot yet continue.");
    }
  };

  try {
    while (!completed) {
      const { done, value } = await reader.read();
      buffer += done ? decoder.decode() : decoder.decode(value, { stream: true });
      let boundary: RegExpMatchArray | null;
      while (!completed && (boundary = buffer.match(/\r\n\r\n|\n\n|\r\r/))) {
        const end = boundary.index!;
        processFrame(buffer.slice(0, end));
        buffer = buffer.slice(end + boundary[0].length);
      }
      if (done) {
        if (!completed && buffer.trim()) processFrame(buffer);
        break;
      }
    }
    if (!completed) throw new Error("Gemini stream ended before the response completed");
    return { steps: finalSteps ?? [...steps.entries()].sort(([a], [b]) => a - b).map(([, value]) => value), usage };
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}
