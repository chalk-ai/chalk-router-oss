import { describe, expect, it } from "vitest";
import { modelsForTab, usesResponsesApi, supportedChatParameters, supportsChatParameter, type Model } from "./playground-shared";

const model = (id: string, protocol: "openai_responses" | "openai_chat_completions" | null): Model => ({
  id,
  owned_by: "openai",
  capabilities: { playground: { chat_protocol: protocol } },
});

describe("server-projected chat eligibility", () => {
  it("keeps unverified models and uses the server-selected endpoint", () => {
    const audio = model("openai/gpt-4o-audio-preview", "openai_chat_completions");
    const text = model("openai/gpt-4o-mini", "openai_responses");
    const image = model("openai/gpt-image-1", null);
    const unknown: Model = { id: "custom/model", owned_by: "custom" };
    const projectedUnknown = model("custom/projected", null);
    projectedUnknown.capabilities!.playground!.chat_support = "unknown";
    expect(modelsForTab([audio, text, image, unknown, projectedUnknown], "chat")).toEqual({
      supported: [audio, text], unverified: [unknown, projectedUnknown],
    });
    expect(usesResponsesApi(audio, audio.id)).toBe(false);
    expect(usesResponsesApi(text, text.id)).toBe(true);
  });

  it("honors a server exclusion even if older capability fields suggest chat", () => {
    const excluded = model("openai/specialized", null);
    excluded.capabilities = {
      ...excluded.capabilities,
      surfaces: { chat: "supported" },
      modalities: { input: { text: "supported" }, output: { text: "supported" } },
      interfaces: { openai_responses: { operations: { create: { support: "supported" } } } },
    };
    expect(modelsForTab([excluded], "chat")).toEqual({ supported: [], unverified: [] });
  });

  it("preserves unverified suggestions for other tabs", () => {
    const embedding: Model = { id: "custom/embedding", owned_by: "custom" };
    expect(modelsForTab([embedding], "embeddings").unverified).toEqual([embedding]);
  });

  it("requires the Decisions surface, modalities, and Router Decisions operation", () => {
    const supported: Model = {
      id: "typesafe/jev-latest",
      owned_by: "typesafe",
      capabilities: {
        surfaces: { decisions: "supported" },
        modalities: { input: { text: "supported" }, output: { decisions: "supported" } },
        interfaces: { router_decisions: { operations: { create: { support: "supported" } } } },
      },
    };
    const chatOnly: Model = {
      ...supported,
      id: "custom/chat-only",
      capabilities: { ...supported.capabilities, surfaces: { decisions: "unsupported" } },
    };
    const legacy: Model = { id: "custom/jev-next", owned_by: "custom" };
    expect(modelsForTab([supported, chatOnly, legacy], "decisions")).toEqual({
      supported: [supported],
      unverified: [legacy],
    });
  });
});


describe("chat parameter capabilities", () => {
  it("omits unsupported parameters for the selected endpoint while preserving supported and unknown fields", () => {
    const search: Model = {
      id: "openai/gpt-5-search-api",
      owned_by: "openai",
      capabilities: {
        playground: { chat_protocol: "openai_chat_completions" },
        interfaces: {
          openai_chat_completions: { operations: { create: { parameters: {
            temperature: { support: "unsupported" },
            top_p: { support: "unsupported" },
            frequency_penalty: { support: "unsupported" },
            presence_penalty: { support: "unsupported" },
            max_tokens: { support: "supported" },
          } } } },
          openai_responses: { operations: { create: { parameters: { temperature: { support: "supported" } } } } },
        },
      },
    };
    const params = { temperature: 1, top_p: 1, frequency_penalty: 0, presence_penalty: 0, max_tokens: 2048, stream: true };
    expect(supportsChatParameter(search, search.id, "temperature")).toBe(false);
    expect(JSON.parse(JSON.stringify(supportedChatParameters(search, search.id, params)))).toEqual({ max_tokens: 2048, stream: true });
    expect(supportedChatParameters(undefined, "custom/model", params)).toEqual(params);
    search.capabilities!.playground!.chat_protocol = "openai_responses";
    expect(supportsChatParameter(search, search.id, "temperature")).toBe(true);
  });
});
