import { describe, expect, it } from "vitest";
import { buildGeminiRequest, readGeminiStream } from "./gemini-interactions";
import { modelsForTab, supportedChatParameters, usesGeminiInteractions, usesResponsesApi, type Model } from "./playground-shared";

const frame = (event: Record<string, unknown>) => `event: ${event.event_type}\r\ndata: ${JSON.stringify(event)}\r\n\r\n`;
const complete = (extra = {}) => ({ event_type: "interaction.completed", interaction: { status: "completed", ...extra } });
const start = (index: number, step: Record<string, unknown>) => ({ event_type: "step.start", index, step });
const delta = (index: number, delta: Record<string, unknown>) => ({ event_type: "step.delta", index, delta });
const stop = (index: number) => ({ event_type: "step.stop", index });

function stream(events: Record<string, unknown>[], fragment = 7) {
  const bytes = new TextEncoder().encode(": keepalive\r\n\r\n" + events.map(frame).join(""));
  return new Response(new ReadableStream({
    start(controller) {
      for (let i = 0; i < bytes.length; i += fragment) controller.enqueue(bytes.slice(i, i + fragment));
      controller.close();
    },
  }));
}

describe("native Gemini selection", () => {
  const native: Model = {
    id: "team/fast", owned_by: "gemini",
    capabilities: {
      surfaces: { chat: "supported" },
      modalities: { input: { text: "supported" }, output: { text: "supported" } },
      interfaces: {
        gemini_interactions: { operations: { create: { support: "supported", features: { streaming: "supported" } } } },
        openai_responses: { operations: { create: { support: "supported" } } },
      },
    },
  };

  it("prefers native Gemini for aliases when advertised by the server", () => {
    expect(usesGeminiInteractions(native)).toBe(true);
    expect(usesResponsesApi(native, native.id)).toBe(false);
    expect(modelsForTab([native], "chat").supported).toEqual([native]);
  });

  it("keeps legacy, unsupported, and non-streaming models on existing APIs", () => {
    expect(usesGeminiInteractions({ id: "gemini/gemini-2.5-flash", owned_by: "gemini" })).toBe(false);
    for (const change of [{ support: "unsupported" }, { features: { streaming: "unsupported" } }]) {
      const value = structuredClone(native);
      Object.assign(value.capabilities!.interfaces!.gemini_interactions!.operations!.create!, change);
      expect(usesGeminiInteractions(value)).toBe(false);
      expect(usesResponsesApi(value, value.id)).toBe(true);
    }
  });

  it("includes native-only text models but excludes non-text models", () => {
    const value = structuredClone(native);
    delete value.capabilities!.interfaces!.openai_responses;
    expect(modelsForTab([value], "chat").supported).toEqual([value]);
    value.capabilities!.modalities!.output!.text = "unsupported";
    expect(modelsForTab([value], "chat")).toEqual({ supported: [], unverified: [] });
  });

  it("retains server-projected exclusions while preferring native Gemini for eligible chat", () => {
    const value = structuredClone(native);
    value.capabilities!.playground = { chat_support: "supported", chat_protocol: "openai_responses" };
    expect(modelsForTab([value], "chat").supported).toEqual([value]);
    expect(usesResponsesApi(value, value.id)).toBe(false);
    value.capabilities!.playground = { chat_support: "unsupported", chat_protocol: null };
    expect(modelsForTab([value], "chat")).toEqual({ supported: [], unverified: [] });
  });

  it("checks sampling parameters against the native endpoint when selected", () => {
    const value = structuredClone(native);
    value.capabilities!.interfaces!.gemini_interactions!.operations!.create!.parameters = {
      temperature: { support: "unsupported" },
    };
    expect(supportedChatParameters(value, value.id, { temperature: 1, max_output_tokens: 512 }))
      .toEqual({ max_output_tokens: 512 });
  });
});

describe("Gemini streaming and stateless replay", () => {
  it("preserves signed steps and unknown fields across a multi-turn conversation", async () => {
    let visible = "";
    const first = buildGeminiRequest("team/fast", [], "Hi", "Be brief", { max_output_tokens: 512 });
    const reply = await readGeminiStream(stream([
      start(0, { type: "thought", future_field: { untouched: [3, 2, 1] } }),
      delta(0, { type: "thought_summary", content: { type: "text", text: "A private summary" } }),
      delta(0, { type: "thought_signature", signature: "signed==" }),
      stop(0),
      start(1, { type: "model_output" }),
      delta(1, { type: "text", text: "Hé" }),
      delta(1, { type: "text_delta", text: "llo 👋" }),
      delta(1, { type: "text_annotation_delta", annotations: [{ url: "https://example.com" }] }),
      stop(1),
      complete({ usage: { total_input_tokens: 3577, total_output_tokens: 305, total_tokens: 4650 } }),
    ], 1), (text) => { visible += text; });
    expect(visible).toBe("Héllo 👋");
    expect(reply.steps).toEqual([
      { type: "thought", future_field: { untouched: [3, 2, 1] }, signature: "signed==", summary: [{ type: "text", text: "A private summary" }] },
      { type: "model_output", content: [{ type: "text", text: "Héllo 👋", annotations: [{ url: "https://example.com" }] }] },
    ]);
    expect(reply.usage).toEqual({ promptTokens: 3577, completionTokens: 305, totalTokens: 4650 });
    const history = [...first.input, ...reply.steps];
    const second = buildGeminiRequest("team/fast", history, "Continue", "Be brief", { temperature: 1, max_output_tokens: 512 });
    expect(second.input.slice(0, -1)).toEqual(history);
    expect(second.input.at(-1)).toEqual({ type: "user_input", content: [{ type: "text", text: "Continue" }] });
    expect(second).toMatchObject({ store: false, stream: true, system_instruction: "Be brief", generation_config: { max_output_tokens: 512 } });
    expect(second).not.toHaveProperty("previous_interaction_id");
    expect(history).toHaveLength(3);
  });

  it("preserves complete terminal steps, including unfamiliar fields", async () => {
    const steps = [{ type: "thought", signature: "opaque" }, { type: "model_output", content: [{ type: "text", text: "Answer" }], extra: true }];
    let text = "";
    const reply = await readGeminiStream(stream([complete({ steps })]), (delta) => { text += delta; });
    expect(reply.steps).toEqual(steps);
    expect(text).toBe("Answer");
  });

  it("does not duplicate text when the terminal event also contains steps", async () => {
    const output = { type: "model_output", content: [{ type: "text", text: "Hello" }] };
    let text = "";
    await readGeminiStream(stream([start(0, output), stop(0), complete({ steps: [output] })]), (delta) => { text += delta; });
    expect(text).toBe("Hello");
  });

  it("assembles function-call arguments without reordering signed steps", async () => {
    const reply = await readGeminiStream(stream([
      start(0, { type: "thought", signature: "sig" }), stop(0),
      start(1, { type: "function_call", id: "call-1", name: "weather", arguments: {} }),
      delta(1, { type: "arguments_delta", arguments: '{"city":' }),
      delta(1, { type: "arguments_delta", arguments: '"Paris"}' }), stop(1), complete(),
    ]), () => {});
    expect(reply.steps.map((step) => step.type)).toEqual(["thought", "function_call"]);
    expect(reply.steps[1].arguments).toEqual({ city: "Paris" });
  });

  it.each(["failed", "cancelled", "incomplete", "budget_exceeded", "requires_action"])("rejects %s history", async (status) => {
    await expect(readGeminiStream(stream([complete({ status })]), () => {})).rejects.toThrow();
  });

  it("reports upstream errors and disconnects instead of returning reusable history", async () => {
    await expect(readGeminiStream(stream([{ event_type: "error", error: { message: "quota exceeded" } }]), () => {})).rejects.toThrow("quota exceeded");
    await expect(readGeminiStream(stream([start(0, { type: "model_output" }), delta(0, { type: "text", text: "Partial" })]), () => {})).rejects.toThrow("before the response completed");
    await expect(readGeminiStream(stream([start(0, { type: "model_output" }), complete()]), () => {})).rejects.toThrow("unfinished steps");
  });

  it("does not silently drop unknown deltas or malformed frames", async () => {
    await expect(readGeminiStream(stream([start(0, { type: "thought" }), delta(0, { type: "future_delta" }), stop(0), complete()]), () => {})).rejects.toThrow("cannot yet continue");
    await expect(readGeminiStream(new Response("data: nope\n\n"), () => {})).rejects.toThrow("Malformed");
  });

  it("propagates an aborted stream and cancels its reader", async () => {
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const response = new Response(new ReadableStream<Uint8Array>({ start(value) { controller = value; } }));
    const pending = readGeminiStream(response, () => {});
    controller.error(new DOMException("Stopped", "AbortError"));
    await expect(pending).rejects.toMatchObject({ name: "AbortError" });
    expect(response.body!.locked).toBe(false);
  });
});
