"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { EMBEDDING_REQUEST_LIMIT_LABEL } from "../../lib/embedding-request-limits";
import { ModelSelect, modelsForTab, useQueryParam, type Model } from "./playground-shared";
import {
  assertEmbeddingRequestFits, embeddingEligibility, embeddingResponseError,
  encodeEmbeddingImage, readEmbeddingResult, serializeEmbeddingRequest,
  validateEmbeddingImage, type EmbeddingImageMime,
} from "./embedding-playground";

interface Attachment {
  id: number;
  file: File;
  mimeType?: EmbeddingImageMime;
  preview?: string;
  error?: string;
}
interface Result extends ReturnType<typeof readEmbeddingResult> {
  model: string;
  imageCount: number;
}
const message = (error: unknown) => error instanceof Error ? error.message : String(error);
const controlStyle = { background: "var(--color-cds-status-neutral-fill)", border: "1px solid var(--color-cds-border-strong)", color: "var(--color-cds-ink)" };
const mutedStyle = { color: "var(--color-cds-ink-secondary)" };
const sizes = (items: Attachment[]) => items.map(({ file, mimeType }) => ({
  size: file.size,
  // JPEG has the longer data URL prefix, so this bounds pending files too.
  mimeType: mimeType ?? (file.type.toLowerCase() === "image/png" ? "image/png" : "image/jpeg") as EmbeddingImageMime,
}));

export default function EmbeddingsTab({ models }: { models: Model[] }) {
  const [model, setModel] = useQueryParam("model", "");
  const modelGroups = useMemo(() => modelsForTab(models, "embeddings"), [models]);
  const selectedModel = models.find((candidate) => candidate.id === model);
  const [input, setInput] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const attachmentsRef = useRef<Attachment[]>([]);
  const nextId = useRef(0);
  const mounted = useRef(true);
  const fileInput = useRef<HTMLInputElement>(null);
  const request = useRef<AbortController | null>(null);
  const runId = useRef(0);
  const busy = useRef(false);
  const [result, setResult] = useState<Result | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);

  const updateAttachments = (next: Attachment[]) => {
    attachmentsRef.current = next;
    setAttachments(next);
  };
  // A run belongs to the draft it was started from; any draft edit abandons it.
  const cancelRun = () => {
    runId.current += 1;
    request.current?.abort();
    request.current = null;
    busy.current = false;
    setLoading(false);
  };
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      runId.current += 1;
      request.current?.abort();
      for (const attachment of attachmentsRef.current) {
        if (attachment.preview) URL.revokeObjectURL(attachment.preview);
      }
    };
  }, []);

  const addFiles = (files: File[]) => {
    if (files.length === 0) return;
    const added = files.map((file) => ({ id: nextId.current++, file }));
    const next = [...attachmentsRef.current, ...added];
    const maximumImages = selectedModel?.capabilities?.limits?.embedding_input?.max_images_per_document;
    if (maximumImages !== undefined && next.length > maximumImages) {
      setError(`This model accepts at most ${maximumImages} images in one document. Remove images or select another model.`);
      return;
    }
    try {
      // Reject an oversized encoded request before any image decode or base64 allocation.
      assertEmbeddingRequestFits(model, input, sizes(next));
    } catch (error) { setError(message(error)); return; }
    cancelRun();
    setError(null);
    updateAttachments(next);
    for (const attachment of added) {
      void validateEmbeddingImage(attachment.file).then((mimeType) => {
        if (!mounted.current || !attachmentsRef.current.some(({ id }) => id === attachment.id)) return;
        const preview = URL.createObjectURL(attachment.file);
        updateAttachments(attachmentsRef.current.map((item) => item.id === attachment.id ? { ...item, mimeType, preview } : item));
      }).catch((error) => {
        if (!mounted.current) return;
        updateAttachments(attachmentsRef.current.map((item) => item.id === attachment.id ? { ...item, error: message(error) } : item));
      });
    }
  };
  const remove = (id: number) => {
    cancelRun();
    const removed = attachmentsRef.current.find((item) => item.id === id);
    if (removed?.preview) URL.revokeObjectURL(removed.preview);
    updateAttachments(attachmentsRef.current.filter((item) => item.id !== id));
    setError(null);
  };
  const clear = () => {
    cancelRun();
    for (const attachment of attachmentsRef.current) {
      if (attachment.preview) URL.revokeObjectURL(attachment.preview);
    }
    updateAttachments([]);
    setInput("");
    setResult(null);
    setError(null);
    if (fileInput.current) fileInput.current.value = "";
  };

  // Sizing serializes the whole draft, so recompute only when the draft changes.
  const { eligibility, disabledReason } = useMemo(() => {
    const eligibility = embeddingEligibility(selectedModel, Boolean(input.trim()), attachments.map((item) => item.mimeType ?? "image/jpeg"));
    let disabledReason = !model.trim() ? "Select or enter a model." :
      !input.trim() && attachments.length === 0 ? "Enter text or add a PNG/JPEG image." :
      attachments.some((item) => item.error) ? "Remove invalid images before running." :
      attachments.some((item) => !item.mimeType) ? "Checking images…" : eligibility.reason;
    if (!disabledReason) {
      try { assertEmbeddingRequestFits(model, input, sizes(attachments)); }
      catch (error) { disabledReason = message(error); }
    }
    return { eligibility, disabledReason };
  }, [selectedModel, model, input, attachments]);
  const limits = selectedModel?.capabilities?.limits;

  const embed = async () => {
    if (disabledReason || busy.current) return;
    busy.current = true;
    setLoading(true);
    setError(null);
    setResult(null);
    const currentRun = ++runId.current;
    const controller = new AbortController();
    request.current = controller;
    const submittedModel = model.trim();
    const submittedImages = [...attachmentsRef.current];
    const current = () => mounted.current && runId.current === currentRun;
    try {
      assertEmbeddingRequestFits(submittedModel, input, sizes(submittedImages));
      const encoded = await Promise.all(submittedImages.map((image) => encodeEmbeddingImage(image.file, image.mimeType!, controller.signal)));
      if (!current()) return;
      const body = serializeEmbeddingRequest(submittedModel, input, encoded);
      const response = await fetch("/api/v1/embeddings", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body,
        signal: controller.signal,
      });
      const text = await response.text();
      if (!current()) return;
      if (!response.ok) throw new Error(embeddingResponseError(response.status, text, response.headers.get("retry-after")));
      let data: unknown;
      try { data = JSON.parse(text); }
      catch { throw new Error("The provider returned an invalid embedding response: expected JSON."); }
      setResult({ ...readEmbeddingResult(data), model: submittedModel, imageCount: submittedImages.length });
    } catch (error) {
      if (current() && !controller.signal.aborted) setError(message(error));
    } finally {
      if (current()) {
        busy.current = false;
        request.current = null;
        setLoading(false);
      }
    }
  };

  return (
    <div className="flex flex-col gap-4 flex-1">
      <div className="flex items-center gap-3 flex-wrap">
        <ModelSelect groups={modelGroups} value={model} onChange={(value) => { cancelRun(); setModel(value); }} />
        {models.length > 0 && modelGroups.supported.length === 0 && (
          <span className="text-xs" style={mutedStyle}>No models advertise verified embedding support. Try an unverified candidate or enter a model ID.</span>
        )}
      </div>
      {model && eligibility.unverified && !eligibility.reason && (
        <p className="text-xs" style={{ color: "var(--color-cds-status-yellow-text)" }}>Support for this input is unverified. You can attempt the request; the provider may reject it.</p>
      )}
      {limits && (limits.max_input_tokens !== undefined || limits.embedding_input) && (
        <p className="text-xs" style={mutedStyle}>
          Model limits: {[
            limits.embedding_input?.max_images_per_document !== undefined ? `${limits.embedding_input.max_images_per_document} images per document` : null,
            limits.embedding_input?.image_mime_types?.join(", "),
            limits.max_input_tokens !== undefined ? `${limits.max_input_tokens.toLocaleString()} maximum input tokens` : null,
          ].filter(Boolean).join(" · ")}. Token usage is reported after the request.
        </p>
      )}
      <label className="flex flex-col gap-2 text-xs" style={mutedStyle}>
        Text or caption (optional with images)
        <textarea value={input} onChange={(event) => { cancelRun(); setInput(event.target.value); }} placeholder="Enter text to embed, or add images below…" rows={4}
          className="text-sm rounded-md px-4 py-3 outline-none resize-y" style={controlStyle} />
      </label>
      <div role="group" aria-label="Image attachments" className="rounded-lg p-4 flex flex-col gap-3"
        style={{ ...controlStyle, borderColor: dragging ? "var(--color-cds-primary)" : "var(--color-cds-border-strong)" }}
        onDragOver={(event) => { event.preventDefault(); setDragging(true); }}
        onDragLeave={(event) => {
          // Moving onto a child element also fires dragleave on this group.
          if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDragging(false);
        }}
        onDrop={(event) => { event.preventDefault(); setDragging(false); addFiles(Array.from(event.dataTransfer.files)); }}>
        <label className="text-sm flex flex-col gap-2">
          Add PNG/JPEG images or drop files here
          <input ref={fileInput} type="file" multiple accept="image/png,image/jpeg,.png,.jpg,.jpeg" aria-describedby="embedding-image-help"
            className="text-xs" onChange={(event) => {
              const files = Array.from(event.currentTarget.files ?? []);
              event.currentTarget.value = "";
              addFiles(files);
            }} />
        </label>
        <p id="embedding-image-help" className="text-xs" style={mutedStyle}>Text and images form one document and produce one vector. Maximum encoded request: {EMBEDDING_REQUEST_LIMIT_LABEL}. Images stay in this browser until Run. On Run, they are sent to the gateway and provider; their logging and retention settings apply.</p>
        {attachments.length > 0 && (
          <ul className="flex flex-wrap gap-3">
            {attachments.map((image) => (
              <li key={image.id} className="rounded-md p-3 flex flex-col gap-2 max-w-60" style={{ background: "var(--color-cds-raised)", border: "1px solid var(--color-cds-border-strong)" }}>
                {image.preview && <img src={image.preview} alt={`Preview of ${image.file.name}`} className="h-24 w-32 object-contain rounded" />}
                <span className="text-xs break-all">{image.file.name}</span>
                <span className="text-xs" style={mutedStyle}>{image.file.size.toLocaleString()} bytes{!image.mimeType && !image.error ? " · Checking…" : ""}</span>
                {image.error && <span role="alert" className="text-xs" style={{ color: "var(--color-cds-status-red-text)" }}>{image.error}</span>}
                <button type="button" onClick={() => remove(image.id)} aria-label={`Remove ${image.file.name}`} className="text-xs self-start underline">Remove</button>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="flex items-center gap-3 flex-wrap">
        <button type="button" onClick={embed} disabled={Boolean(disabledReason) || loading} aria-describedby={disabledReason ? "embedding-disabled-reason" : undefined}
          className="px-5 py-2 text-sm font-medium rounded-md transition-colors"
          style={{ background: !disabledReason && !loading ? "var(--color-cds-primary)" : "var(--color-cds-status-neutral-fill)", color: !disabledReason && !loading ? "white" : "var(--color-cds-ink-subtle)" }}>
          {loading ? "Computing…" : "Generate Embedding"}
        </button>
        <button type="button" onClick={clear} className="px-4 py-2 text-sm rounded-md" style={controlStyle}>Clear</button>
        {disabledReason && <p id="embedding-disabled-reason" className="text-xs" style={mutedStyle}>{disabledReason}</p>}
      </div>
      {error && <div role="alert" className="px-4 py-2 rounded-lg text-sm whitespace-pre-wrap break-words" style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}>{error}</div>}
      {result && (
        <div aria-label="Embedding result" className="rounded-lg p-4 overflow-auto flex-1" style={{ background: "var(--color-cds-elevated)", border: "1px solid var(--color-cds-border)" }}>
          <div className="mb-3 text-xs break-words" style={mutedStyle}>
            Submitted model: {result.model} · {result.imageCount} images · {result.vector.length} dimensions · {result.promptTokens !== undefined ? `${result.promptTokens} reported prompt tokens` : "Reported prompt tokens unavailable"}
            {result.totalTokens !== undefined && ` · ${result.totalTokens} reported total tokens`}
          </div>
          <div className="font-mono text-xs whitespace-pre-wrap break-all" style={mutedStyle}>
            [{result.vector.slice(0, 50).map((value) => value.toFixed(6)).join(", ")}{result.vector.length > 50 && `, … (${result.vector.length - 50} more)`}]
          </div>
        </div>
      )}
    </div>
  );
}
