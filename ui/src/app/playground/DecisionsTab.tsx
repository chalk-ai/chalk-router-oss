"use client";

import { useMemo, useRef, useState, type ChangeEvent } from "react";
import { ModelSelect, modelsForTab, useQueryParam, type Model } from "./playground-shared";
import {
  boxFromAnswer,
  buildDecisionsRequest,
  imageForBoxOverlay,
  imageFormatForMime,
  imageMimeForFormat,
  insertPlaceholderIntoState,
  stripDataUrlPrefix,
  summarizeDecisionAnswer,
  type DecisionBox,
  type DecisionImage,
  type DecisionQuestionDraft,
  type DecisionQuestionType,
} from "./decisions-playground";

interface DecisionsResponse {
  id?: string;
  model?: string;
  provider?: string;
  answers?: Record<string, unknown>;
  usage?: { input_tokens?: number; output_tokens?: number; cost?: number };
  [key: string]: unknown;
}

const fieldStyle = {
  background: "#111827",
  border: "1px solid #374151",
  color: "#e5e7eb",
};

function question(
  id: string,
  type: DecisionQuestionType,
  instructions: string,
  criteriaText = "",
): DecisionQuestionDraft {
  return {
    id,
    type,
    instructions,
    criteriaText,
    trueCriterion: "",
    falseCriterion: "",
  };
}

const QUESTION_TYPES: DecisionQuestionType[] = ["noul", "choice", "score", "box"];
const BOX_COLORS = ["#70CD96", "#60a5fa", "#f59e0b", "#f472b6", "#a78bfa"];

function imageSrc(image: DecisionImage): string {
  return `data:${imageMimeForFormat(image.format)};base64,${image.dataBase64}`;
}

function readImageFile(file: File): Promise<DecisionImage> {
  return new Promise((resolve, reject) => {
    let format: string;
    try {
      format = imageFormatForMime(file.type, file.name);
    } catch (caught) {
      reject(caught);
      return;
    }
    const reader = new FileReader();
    reader.onload = () => resolve({
      placeholder: crypto.randomUUID(),
      format,
      dataBase64: stripDataUrlPrefix(String(reader.result ?? "")),
      name: file.name,
    });
    reader.onerror = () => reject(new Error(`Could not read ${file.name}.`));
    reader.readAsDataURL(file);
  });
}

const presets: Record<string, {
  stateMode: "text" | "json";
  state: string;
  questions: DecisionQuestionDraft[];
  images: DecisionImage[];
}> = {
  "Ticket triage": {
    images: [],
    stateMode: "json" as const,
    state: JSON.stringify({ subject: "Checkout is blank", body: "Customers see a white page after confirming payment." }, null, 2),
    questions: [
      question("team", "choice", "Which team should own this ticket?", "frontend: Browser and rendering issues\npayments: Checkout, billing, and payment issues\nplatform: Infrastructure and reliability issues"),
      question("severity", "score", "How severe is the customer impact?", "low\nmedium\nhigh\ncritical"),
    ],
  },
  "Approval review": {
    images: [],
    stateMode: "json" as const,
    state: JSON.stringify({ request: "Refund $120 for duplicate charge", policy: "Refund verified duplicate charges under $500." }, null, 2),
    questions: [{
      ...question("approve", "noul", "Should this request be approved?"),
      trueCriterion: "The request satisfies the policy",
      falseCriterion: "The request does not satisfy the policy",
    }],
  },
  "Find the object": {
    images: [],
    stateMode: "text" as const,
    state: "Upload an image under Images, then click “Insert into state” to put its placeholder here. Box questions need an image and a TypeSafe Jev 1.14+ model (for example typesafe/jev-preview).\n\nPhoto:",
    questions: [question("object", "box", "Draw a box around the main object in the photo.")],
  },
};

type PresetName = keyof typeof presets;
const DEFAULT_PRESET = presets["Ticket triage"];

export default function DecisionsTab({ models }: { models: Model[] }) {
  const [model, setModel] = useQueryParam("model", "");
  const modelGroups = useMemo(() => modelsForTab(models, "decisions"), [models]);
  const [stateMode, setStateMode] = useState<"text" | "json">("json");
  const [stateText, setStateText] = useState(DEFAULT_PRESET.state);
  const [questions, setQuestions] = useState<DecisionQuestionDraft[]>(DEFAULT_PRESET.questions);
  const [images, setImages] = useState<DecisionImage[]>([]);
  const [imageError, setImageError] = useState<string | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  // The images and state of the request that produced `result`, so box
  // overlays are drawn on what was actually sent even after later edits.
  const [sent, setSent] = useState<{ images: DecisionImage[]; stateText: string } | null>(null);
  const [result, setResult] = useState<DecisionsResponse | null>(null);
  const [latencyMs, setLatencyMs] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const updateQuestion = (index: number, patch: Partial<DecisionQuestionDraft>) => {
    setQuestions((current) => current.map((item, itemIndex) => itemIndex === index ? { ...item, ...patch } : item));
  };

  const addQuestion = (type: DecisionQuestionType) => {
    setQuestions((current) => [...current, question(`question_${current.length + 1}`, type, "")]);
  };

  const loadPreset = (name: PresetName) => {
    const preset = presets[name];
    setStateMode(preset.stateMode);
    setStateText(preset.state);
    setQuestions(preset.questions.map((item) => ({ ...item })));
    setImages(preset.images.map((item) => ({ ...item })));
    setImageError(null);
    setResult(null);
    setSent(null);
    setError(null);
  };

  const addImages = async (event: ChangeEvent<HTMLInputElement>) => {
    const files = Array.from(event.target.files ?? []);
    event.target.value = "";
    if (files.length === 0) return;
    setImageError(null);
    const settled = await Promise.allSettled(files.map(readImageFile));
    const added = settled.flatMap((item) => item.status === "fulfilled" ? [item.value] : []);
    const failures = settled.flatMap((item) => item.status === "rejected"
      ? [item.reason instanceof Error ? item.reason.message : String(item.reason)]
      : []);
    if (added.length > 0) setImages((current) => [...current, ...added]);
    if (failures.length > 0) setImageError(failures.join(" "));
  };

  const insertImage = (image: DecisionImage) => {
    setStateText((current) => insertPlaceholderIntoState(current, stateMode, image.placeholder));
  };

  const decide = async () => {
    if (loading) return;
    setError(null);
    setResult(null);
    setLatencyMs(null);
    setSent(null);
    let body;
    try {
      body = buildDecisionsRequest(model, stateText, stateMode, questions, images);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
      return;
    }

    setLoading(true);
    const started = performance.now();
    try {
      const response = await fetch("/api/v1/alpha/decisions", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      const payload = await response.json().catch(() => null) as DecisionsResponse | null;
      if (!response.ok) {
        const message = (payload?.error as { message?: string } | undefined)?.message;
        throw new Error(message ?? `Request failed with status ${response.status}`);
      }
      setResult(payload ?? {});
      setSent({ images, stateText });
      setLatencyMs(performance.now() - started);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <ModelSelect groups={modelGroups} value={model} onChange={setModel} />
        <div className="flex items-center gap-2">
          <span className="text-xs" style={{ color: "#6b7280" }}>Presets</span>
          {(Object.keys(presets) as PresetName[]).map((name) => (
            <button key={name} onClick={() => loadPreset(name)} className="rounded-md px-3 py-2 text-xs" style={fieldStyle}>
              {name}
            </button>
          ))}
        </div>
      </div>

      {models.length > 0 && modelGroups.supported.length === 0 && (
        <div className="text-xs" style={{ color: "#6b7280" }}>
          No listed models advertise verified Decisions support. Enter a model ID to try one directly.
        </div>
      )}

      <div className="grid min-h-0 flex-1 grid-cols-1 gap-4 xl:grid-cols-2">
        <div className="min-h-0 space-y-4 overflow-auto rounded-lg p-4" style={{ background: "#0f172a", border: "1px solid #1f2937" }}>
          <section>
            <div className="mb-2 flex items-center justify-between">
              <label className="text-sm font-medium" style={{ color: "#d1d5db" }}>State</label>
              <div className="flex rounded-md p-0.5" style={{ background: "#111827" }}>
                {(["text", "json"] as const).map((mode) => (
                  <button
                    key={mode}
                    onClick={() => setStateMode(mode)}
                    className="rounded px-2.5 py-1 text-xs uppercase"
                    style={{ color: stateMode === mode ? "#70CD96" : "#6b7280", background: stateMode === mode ? "#064e3b" : "transparent" }}
                  >
                    {mode}
                  </button>
                ))}
              </div>
            </div>
            <textarea
              aria-label="Decision state"
              value={stateText}
              onChange={(event) => setStateText(event.target.value)}
              rows={7}
              className="w-full resize-y rounded-md px-3 py-2 font-mono text-sm outline-none"
              style={fieldStyle}
              placeholder="The text or JSON state to evaluate"
            />
          </section>

          <section>
            <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
              <label className="text-sm font-medium" style={{ color: "#d1d5db" }}>Images</label>
              <button onClick={() => fileInput.current?.click()} className="rounded px-2.5 py-1.5 text-xs" style={fieldStyle}>
                + Add images
              </button>
              <input
                ref={fileInput}
                type="file"
                accept="image/png,image/jpeg,image/webp"
                multiple
                aria-label="Add images"
                onChange={addImages}
                className="hidden"
              />
            </div>
            {images.length === 0 ? (
              <div className="text-xs" style={{ color: "#6b7280" }}>
                Optional. PNG, JPEG, or WebP; each image is sent as an asset and referenced from the state by its placeholder.
              </div>
            ) : (
              <div className="space-y-2">
                {images.map((image) => (
                  <div key={image.placeholder} className="flex items-center gap-3 rounded-lg p-2" style={{ background: "#111827", border: "1px solid #273244" }}>
                    {/* eslint-disable-next-line @next/next/no-img-element -- local data URL preview */}
                    <img src={imageSrc(image)} alt={image.name ?? "Uploaded image"} className="h-14 w-14 shrink-0 rounded object-cover" style={{ border: "1px solid #374151" }} />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-xs" style={{ color: "#d1d5db" }} title={image.name}>{image.name ?? "Image"} · {image.format}</div>
                      <div className="mt-1 truncate font-mono text-xs" style={{ color: "#9ca3af" }} title={image.placeholder}>{image.placeholder}</div>
                    </div>
                    <button onClick={() => insertImage(image)} className="shrink-0 rounded px-2.5 py-1.5 text-xs" style={fieldStyle}>
                      Insert into state
                    </button>
                    <button
                      aria-label={`Remove image ${image.name ?? image.placeholder}`}
                      onClick={() => setImages((current) => current.filter((item) => item.placeholder !== image.placeholder))}
                      className="shrink-0 rounded-md px-2.5 py-1.5 text-xs"
                      style={{ color: "#fca5a5", border: "1px solid #4c1d1d" }}
                    >
                      Remove
                    </button>
                  </div>
                ))}
              </div>
            )}
            {imageError && <div className="mt-2 rounded-lg px-3 py-2 text-xs" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>{imageError}</div>}
          </section>

          <section>
            <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
              <label className="text-sm font-medium" style={{ color: "#d1d5db" }}>Questions</label>
              <div className="flex gap-1.5">
                {QUESTION_TYPES.map((type) => (
                  <button key={type} onClick={() => addQuestion(type)} className="rounded px-2.5 py-1.5 text-xs capitalize" style={fieldStyle}>
                    + {type}
                  </button>
                ))}
              </div>
            </div>

            <div className="space-y-3">
              {questions.map((item, index) => (
                <div key={index} className="rounded-lg p-3" style={{ background: "#111827", border: "1px solid #273244" }}>
                  <div className="mb-2 grid grid-cols-[1fr_130px_auto] gap-2">
                    <input
                      aria-label={`Question ${index + 1} ID`}
                      value={item.id}
                      onChange={(event) => updateQuestion(index, { id: event.target.value })}
                      className="min-w-0 rounded-md px-3 py-2 font-mono text-xs outline-none"
                      style={fieldStyle}
                      placeholder="question_id"
                    />
                    <select
                      aria-label={`Question ${index + 1} type`}
                      value={item.type}
                      onChange={(event) => updateQuestion(index, { type: event.target.value as DecisionQuestionType })}
                      className="rounded-md px-3 py-2 text-xs capitalize outline-none"
                      style={fieldStyle}
                    >
                      <option value="noul">Noul</option>
                      <option value="choice">Choice</option>
                      <option value="score">Score</option>
                      <option value="box">Box</option>
                    </select>
                    <button
                      aria-label={`Remove question ${index + 1}`}
                      onClick={() => setQuestions((current) => current.filter((_, itemIndex) => itemIndex !== index))}
                      className="rounded-md px-2 text-sm"
                      style={{ color: "#fca5a5", border: "1px solid #4c1d1d" }}
                    >
                      ×
                    </button>
                  </div>
                  <textarea
                    aria-label={`Question ${index + 1} instructions`}
                    value={item.instructions}
                    onChange={(event) => updateQuestion(index, { instructions: event.target.value })}
                    rows={2}
                    className="mb-2 w-full resize-y rounded-md px-3 py-2 text-sm outline-none"
                    style={fieldStyle}
                    placeholder="Instructions"
                  />
                  {item.type === "box" ? (
                    <div className="text-xs" style={{ color: "#6b7280" }}>
                      Answers with a bounding box. Needs at least one image; only TypeSafe Jev 1.14+ answers box questions.
                    </div>
                  ) : item.type === "noul" ? (
                    <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                      <input value={item.trueCriterion} onChange={(event) => updateQuestion(index, { trueCriterion: event.target.value })} className="rounded-md px-3 py-2 text-xs outline-none" style={fieldStyle} placeholder="True criterion (optional)" />
                      <input value={item.falseCriterion} onChange={(event) => updateQuestion(index, { falseCriterion: event.target.value })} className="rounded-md px-3 py-2 text-xs outline-none" style={fieldStyle} placeholder="False criterion (optional)" />
                    </div>
                  ) : (
                    <textarea
                      value={item.criteriaText}
                      onChange={(event) => updateQuestion(index, { criteriaText: event.target.value })}
                      rows={item.type === "choice" ? 3 : 4}
                      className="w-full resize-y rounded-md px-3 py-2 font-mono text-xs outline-none"
                      style={fieldStyle}
                      placeholder={item.type === "choice" ? "option: Optional description (one per line)" : "Score level (one per line, 2–10)"}
                    />
                  )}
                </div>
              ))}
            </div>
          </section>

          <button
            onClick={decide}
            disabled={!model || loading}
            className="w-full rounded-md px-5 py-2.5 text-sm font-medium transition-colors"
            style={{ background: model && !loading ? "#065f46" : "#1f2937", color: model && !loading ? "#70CD96" : "#4b5563" }}
          >
            {loading ? "Deciding…" : "Run Decisions"}
          </button>
          {error && <div className="rounded-lg px-4 py-2 text-sm" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>{error}</div>}
        </div>

        <div className="min-h-[320px] overflow-auto rounded-lg p-4" style={{ background: "#0f172a", border: "1px solid #1f2937" }}>
          {!result ? (
            <div className="flex h-full min-h-[280px] items-center justify-center text-sm" style={{ color: "#4b5563" }}>
              Decision answers will appear here.
            </div>
          ) : (
            <div className="space-y-4">
              <div className="grid grid-cols-2 gap-2 text-xs sm:grid-cols-3">
                <Metric label="Resolved model" value={result.model ?? "Unknown"} />
                <Metric label="Latency" value={latencyMs === null ? "—" : `${Math.round(latencyMs)} ms`} />
                <Metric label="Provider" value={result.provider ?? "—"} />
                <Metric label="Input tokens" value={String(result.usage?.input_tokens ?? "—")} />
                <Metric label="Output tokens" value={String(result.usage?.output_tokens ?? "—")} />
                <Metric label="Cost" value={typeof result.usage?.cost === "number" ? `$${result.usage.cost.toFixed(6)}` : "Unavailable"} />
              </div>
              <BoxResults answers={result.answers} sent={sent} />
              <section>
                <h3 className="mb-2 text-sm font-medium" style={{ color: "#d1d5db" }}>Answers</h3>
                <div className="space-y-2">
                  {Object.entries(result.answers ?? {}).map(([id, answer]) => {
                    const summary = summarizeDecisionAnswer(answer);
                    return (
                      <div key={id} className="rounded-lg p-3" style={{ background: "#111827", border: "1px solid #273244" }}>
                        <div className="font-mono text-xs" style={{ color: "#6b7280" }}>{id}</div>
                        <div className="mt-1 text-lg font-medium" style={{ color: "#e5e7eb" }}>{summary.value}</div>
                        {summary.detail && <div className="mt-1 text-xs" style={{ color: "#70CD96" }}>{summary.detail}</div>}
                      </div>
                    );
                  })}
                </div>
              </section>
              <details>
                <summary className="cursor-pointer text-xs" style={{ color: "#9ca3af" }}>Raw JSON</summary>
                <pre className="mt-2 overflow-auto rounded-md p-3 text-xs" style={{ background: "#020617", color: "#9ca3af" }}>{JSON.stringify(result, null, 2)}</pre>
              </details>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function BoxResults({
  answers,
  sent,
}: {
  answers: Record<string, unknown> | undefined;
  sent: { images: DecisionImage[]; stateText: string } | null;
}) {
  const boxes = Object.entries(answers ?? {}).flatMap(([id, answer]) => {
    const box = boxFromAnswer(answer);
    return box ? [{ id, box }] : [];
  });
  if (boxes.length === 0) return null;
  const image = sent ? imageForBoxOverlay(sent.images, sent.stateText) : undefined;
  return (
    <section>
      <h3 className="mb-2 text-sm font-medium" style={{ color: "#d1d5db" }}>Boxes</h3>
      {image ? (
        <BoxOverlay image={image} boxes={boxes} />
      ) : (
        <div className="text-xs" style={{ color: "#6b7280" }}>
          The request did not name a single image, so the boxes are listed below without a preview.
        </div>
      )}
    </section>
  );
}

function fraction(value: number): number {
  return Math.min(1, Math.max(0, value));
}

function BoxOverlay({ image, boxes }: { image: DecisionImage; boxes: Array<{ id: string; box: DecisionBox }> }) {
  return (
    <div style={{ position: "relative", display: "inline-block", maxWidth: "100%", lineHeight: 0 }}>
      {/* eslint-disable-next-line @next/next/no-img-element -- local data URL preview */}
      <img src={imageSrc(image)} alt={image.name ?? "Request image"} style={{ display: "block", maxWidth: "100%", maxHeight: 480, borderRadius: 6 }} />
      {boxes.map(({ id, box }, index) => {
        const color = BOX_COLORS[index % BOX_COLORS.length];
        const left = fraction(Math.min(box.left, box.right));
        const right = fraction(Math.max(box.left, box.right));
        const top = fraction(Math.min(box.top, box.bottom));
        const bottom = fraction(Math.max(box.top, box.bottom));
        return (
          <div
            key={id}
            title={id}
            style={{
              position: "absolute",
              left: `${left * 100}%`,
              top: `${top * 100}%`,
              width: `${(right - left) * 100}%`,
              height: `${(bottom - top) * 100}%`,
              border: `2px solid ${color}`,
              background: `${color}1f`,
              boxSizing: "border-box",
              pointerEvents: "none",
            }}
          >
            <span
              className="font-mono"
              style={{
                position: "absolute",
                left: -2,
                // Put the label inside the box when there is no room above it.
                ...(top < 0.08 ? { top: -2 } : { bottom: "100%" }),
                padding: "2px 4px",
                fontSize: 11,
                lineHeight: 1.2,
                whiteSpace: "nowrap",
                background: color,
                color: "#020617",
                borderRadius: top < 0.08 ? "0 0 3px 0" : "3px 3px 0 0",
              }}
            >
              {id}
            </span>
          </div>
        );
      })}
    </div>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0 rounded-md p-2.5" style={{ background: "#111827", border: "1px solid #273244" }}>
      <div style={{ color: "#6b7280" }}>{label}</div>
      <div className="mt-1 truncate font-mono" title={value} style={{ color: "#d1d5db" }}>{value}</div>
    </div>
  );
}
