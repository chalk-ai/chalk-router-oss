"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ModelSelect, modelsForTab, supportedChatParameters, supportsChatParameter, readResponsesTextStream, useQueryParam, useQueryParamNumber, usesGeminiInteractions, usesResponsesApi, type Model, type UsageInfo } from "./playground-shared";
import { buildGeminiRequest, readGeminiStream, type GeminiStep } from "./gemini-interactions";

type ChatMessage = { role: "system" | "user" | "assistant"; content: string; usage?: UsageInfo };

function ChatBubble({
  role,
  content,
  usage,
  streaming,
}: {
  role: ChatMessage["role"];
  content: string;
  usage?: UsageInfo;
  streaming?: boolean;
}) {
  return (
    <div className="flex gap-3">
      <div
        className="flex-shrink-0 flex items-center justify-center rounded-full text-xs font-medium"
        style={{
          width: "28px",
          height: "28px",
          background: role === "user" ? "var(--color-cds-selected)" : "var(--color-cds-status-green-fill)",
          color: role === "user" ? "var(--color-cds-ink-strong)" : "var(--color-cds-status-green-text)",
        }}
      >
        {role === "user" ? "U" : "A"}
      </div>
      <div className="flex-1 min-w-0">
        <div className="text-xs font-medium mb-1" style={{ color: "var(--color-cds-ink-subtle)" }}>
          {role === "user" ? "You" : "Assistant"}
        </div>
        <div className="text-sm whitespace-pre-wrap break-words" style={{ color: "var(--color-cds-ink-medium)" }}>
          {content}
          {streaming && (
            <span
              className="inline-block w-2 h-4 ml-0.5 animate-pulse"
              style={{ background: "var(--color-cds-status-green-text)" }}
            />
          )}
        </div>
        {usage && (
          <div className="mt-1 text-[11px]" style={{ color: "var(--color-cds-ink-muted)" }}>
            {usage.promptTokens} prompt
            {usage.completionTokens !== undefined ? ` + ${usage.completionTokens} completion` : ""} ={" "}
            {usage.totalTokens} tokens
          </div>
        )}
      </div>
    </div>
  );
}

export default function ChatTab({
  models,
}: {
  models: Model[];
}) {
  const [model, setModel] = useQueryParam("model", "");
  const modelGroups = useMemo(() => modelsForTab(models, "chat"), [models]);
  const [systemPrompt, setSystemPrompt] = useState("");
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  // The in-flight assistant reply lives here, not as a placeholder entry in
  // `messages`: each SSE chunk then only updates this one string instead of
  // copying the whole messages array, and an error or abort — whether or not
  // anything streamed yet — leaves no stray empty assistant bubble behind.
  const [streamingContent, setStreamingContent] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [temperature, setTemperature] = useQueryParamNumber("temperature", 1.0);
  const selectedModel = models.find((candidate) => candidate.id === model);
  const temperatureSupported = supportsChatParameter(selectedModel, model, "temperature");
  const [maxTokens, setMaxTokens] = useQueryParamNumber("max_tokens", 2048);
  const useGemini = usesGeminiInteractions(selectedModel);
  const geminiHistoryRef = useRef<GeminiStep[]>([]);
  const [needsClear, setNeedsClear] = useState(false);
  const requestVersionRef = useRef(0);
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const abortRef = useRef<AbortController | null>(null);
  const scrollRafRef = useRef<number | null>(null);

  const clear = useCallback(() => {
    requestVersionRef.current += 1;
    abortRef.current?.abort();
    abortRef.current = null;
    geminiHistoryRef.current = [];
    setMessages([]);
    setStreamingContent(null);
    setStreaming(false);
    setNeedsClear(false);
    setError(null);
  }, []);

  // Signed history belongs to the model/connection that produced it. A model
  // change starts a fresh conversation, including changes through the URL.
  useEffect(() => { clear(); }, [model, useGemini, clear]);
  useEffect(() => () => {
    requestVersionRef.current += 1;
    abortRef.current?.abort();
  }, []);

  // Throttle auto-scroll to at most once per animation frame. A streaming
  // reply can update `streamingContent` far more often than the screen
  // repaints, so scrolling on every single update is wasted work.
  useEffect(() => {
    if (scrollRafRef.current !== null) return;
    scrollRafRef.current = requestAnimationFrame(() => {
      scrollRafRef.current = null;
      messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
    });
    return () => {
      if (scrollRafRef.current !== null) {
        cancelAnimationFrame(scrollRafRef.current);
        scrollRafRef.current = null;
      }
    };
  }, [messages, streamingContent]);

  const send = useCallback(async () => {
    if (!input.trim() || !model || streaming || needsClear) return;
    const version = ++requestVersionRef.current;
    const isCurrent = () => version === requestVersionRef.current;
    setError(null);

    const userMsg: ChatMessage = { role: "user", content: input.trim() };
    const history = [...messages, userMsg];
    setMessages(history);
    setInput("");
    setStreaming(true);
    setStreamingContent("");

    const plainHistory = history.map(({ role, content }) => ({ role, content }));
    const apiMessages = [
      ...(systemPrompt ? [{ role: "system" as const, content: systemPrompt }] : []),
      ...plainHistory,
    ];
    const useResponses = usesResponsesApi(
      models.find((candidate) => candidate.id === model),
      model,
    );

    let accumulated = "";
    let usage: UsageInfo | undefined;
    const geminiRequest = useGemini
      ? buildGeminiRequest(model, geminiHistoryRef.current, input.trim(), systemPrompt, {
          ...supportedChatParameters(selectedModel, model, { temperature }),
          max_output_tokens: maxTokens,
        })
      : undefined;

    try {
      abortRef.current = new AbortController();
      const res = await fetch(
        useGemini ? "/api/v1beta/interactions" : useResponses ? "/api/v1/responses" : "/api/v1/chat/completions",
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(
            geminiRequest ?? (useResponses
              ? {
                  model,
                  instructions: systemPrompt || undefined,
                  input: plainHistory,
                  stream: true,
                  store: false,
                  ...supportedChatParameters(selectedModel, model, { temperature }),
                  max_output_tokens: maxTokens,
                }
              : {
                  model,
                  messages: apiMessages,
                  stream: true,
                  ...supportedChatParameters(selectedModel, model, { temperature }),
                  max_tokens: maxTokens,
                }),
          ),
          signal: abortRef.current.signal,
        },
      );

      if (!res.ok) {
        const errBody = await res.text();
        throw new Error(`${res.status}: ${errBody}`);
      }

      const appendDelta = (delta: string) => {
        if (!isCurrent()) return;
        accumulated += delta;
        setStreamingContent(accumulated);
      };

      if (geminiRequest) {
        const reply = await readGeminiStream(res, appendDelta);
        if (isCurrent()) {
          geminiHistoryRef.current = [...geminiRequest.input, ...reply.steps];
          usage = reply.usage;
        }
      } else if (useResponses) {
        // The Responses API reports usage on its terminal `response.completed`
        // event; capture it into the closured `usage` local for the finally
        // block below.
        await readResponsesTextStream(res, appendDelta, (u) => {
          usage = u;
        });
      } else {
        const reader = res.body?.getReader();
        const decoder = new TextDecoder();
        if (reader) {
          let buffer = "";
          while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            buffer += decoder.decode(value, { stream: true });
            const lines = buffer.split("\n");
            buffer = lines.pop() ?? "";
            for (const line of lines) {
              if (!line.startsWith("data: ")) continue;
              const payload = line.slice(6).trim();
              if (payload === "[DONE]") break;
              try {
                const chunk = JSON.parse(payload);
                const delta = chunk.choices?.[0]?.delta?.content;
                if (delta) appendDelta(delta);
                // The router requests `stream_options.include_usage` from
                // built-in OpenAI-compatible providers, so the terminal chunk
                // (an empty `choices` array alongside `usage`) is passed
                // through unchanged; arbitrary compatible endpoints may omit
                // it, so this stays optional.
                if (chunk.usage) {
                  usage = {
                    promptTokens: chunk.usage.prompt_tokens ?? 0,
                    completionTokens: chunk.usage.completion_tokens ?? 0,
                    totalTokens: chunk.usage.total_tokens ?? 0,
                  };
                }
              } catch {
                // skip unparseable lines
              }
            }
          }
        }
      }
    } catch (e) {
      if (isCurrent() && useGemini) {
        // A partial stream is insufficient to replay signed history safely.
        // Keep any visible text, but require a fresh conversation next time.
        setNeedsClear(true);
        const reason = (e as Error).name === "AbortError"
          ? "Response stopped."
          : e instanceof Error ? e.message : String(e);
        setError(`${reason} Clear the conversation to continue.`);
      } else if (isCurrent() && (e as Error).name !== "AbortError") {
        setError(e instanceof Error ? e.message : String(e));
      }
    } finally {
      // Finalize whatever was actually produced into `messages` on success,
      // on a mid-stream error, or on an abort — but never on nothing at all,
      // so a failure before the first delta leaves no empty assistant bubble.
      if (isCurrent() && (accumulated || usage)) {
        setMessages((prev) => [
          ...prev,
          { role: "assistant", content: accumulated, usage },
        ]);
      }
      if (isCurrent()) {
        setStreamingContent(null);
        setStreaming(false);
        abortRef.current = null;
      }
    }
  }, [
    input,
    model,
    streaming,
    messages,
    systemPrompt,
    temperature,
    selectedModel,
    maxTokens,
    models,
    useGemini,
    needsClear,
  ]);

  const stop = () => {
    abortRef.current?.abort();
  };

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Controls */}
      <div className="flex items-center gap-3 mb-4 flex-wrap">
        <ModelSelect groups={modelGroups} value={model} onChange={setModel} />
        <div className="flex items-center gap-2">
          <label className="text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>Temp</label>
          <input
            type="number"
            min={0}
            max={2}
            step={0.1}
            value={temperature}
            disabled={!temperatureSupported}
            title={temperatureSupported ? undefined : "This model does not support temperature"}
            onChange={(e) => setTemperature(parseFloat(e.target.value) || 0)}
            className="text-sm rounded-md px-2 py-1.5 w-16 outline-none"
            style={{ background: "var(--color-cds-field)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink)" }}
          />
        </div>
        <div className="flex items-center gap-2">
          <label className="text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>Max tokens</label>
          <input
            type="number"
            min={1}
            max={128000}
            step={256}
            value={maxTokens}
            onChange={(e) => setMaxTokens(parseInt(e.target.value) || 2048)}
            className="text-sm rounded-md px-2 py-1.5 w-24 outline-none"
            style={{ background: "var(--color-cds-field)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink)" }}
          />
        </div>
        <button
          onClick={clear}
          className="text-xs px-3 py-1.5 rounded-md transition-colors"
          style={{ background: "var(--color-cds-status-neutral-fill)", color: "var(--color-cds-ink-secondary)", border: "1px solid var(--color-cds-border-strong)" }}
        >
          Clear
        </button>
      </div>

      {/* System prompt */}
      <div className="mb-4">
        <input
          type="text"
          value={systemPrompt}
          onChange={(e) => {
            if (useGemini) clear();
            setSystemPrompt(e.target.value);
          }}
          placeholder="System prompt (optional)"
          className="w-full text-sm rounded-md px-3 py-2 outline-none"
          style={{ background: "var(--color-cds-field)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink)" }}
        />
      </div>

      {/* Messages */}
      <div
        className="flex-1 min-h-0 overflow-y-auto rounded-lg mb-4 p-4 space-y-4"
        style={{ background: "var(--color-cds-elevated)", border: "1px solid var(--color-cds-border)" }}
      >
        {messages.length === 0 && (
          <div className="flex items-center justify-center h-full">
            <p className="text-sm" style={{ color: "var(--color-cds-ink-muted)" }}>
              Send a message to start a conversation.
            </p>
          </div>
        )}
        {messages.map((msg, i) => (
          <ChatBubble key={i} role={msg.role} content={msg.content} usage={msg.usage} />
        ))}
        {streamingContent !== null && (
          <ChatBubble role="assistant" content={streamingContent} streaming />
        )}
        <div ref={messagesEndRef} />
      </div>

      {error && (
        <div
          className="mb-3 px-4 py-2 rounded-lg text-sm"
          style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}
        >
          {error}
        </div>
      )}

      {/* Input */}
      <div className="flex gap-2">
        <input
          type="text"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
          placeholder={model ? "Type a message..." : "Select a model first"}
          disabled={!model || needsClear}
          className="flex-1 text-sm rounded-md px-4 py-2.5 outline-none"
          style={{
            background: "var(--color-cds-field)",
            border: "1px solid var(--color-cds-border)",
            color: "var(--color-cds-ink)",
            opacity: model ? 1 : 0.5,
          }}
        />
        {streaming ? (
          <button
            onClick={stop}
            className="px-5 py-2.5 text-sm font-medium rounded-md transition-colors"
            style={{ background: "var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}
          >
            Stop
          </button>
        ) : (
          <button
            onClick={send}
            disabled={!input.trim() || !model || needsClear}
            className="px-5 py-2.5 text-sm font-medium rounded-md transition-colors"
            style={{
              background: input.trim() && model ? "var(--color-cds-primary)" : "var(--color-cds-status-neutral-fill)",
              color: input.trim() && model ? "white" : "var(--color-cds-ink-muted)",
            }}
          >
            Send
          </button>
        )}
      </div>
    </div>
  );
}
