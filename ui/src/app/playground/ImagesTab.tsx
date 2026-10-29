"use client";

import { useEffect, useMemo, useState } from "react";
import { ModelSelect, modelsForTab, useQueryParam, usesGeminiImageInteractions, type Model } from "./playground-shared";
import { buildImageRequest, readGeminiImages } from "./image-generation";

function ParameterSelect({
  label,
  value,
  values,
  onChange,
}: {
  label: string;
  value: string;
  values: string[];
  onChange: (value: string) => void;
}) {
  return (
    <label className="flex items-center gap-2 text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>
      {label}
      <select
        value={value}
        onChange={(event) => onChange(event.target.value)}
        className="text-sm rounded-md px-3 py-2 outline-none"
        style={{ background: "var(--color-cds-field)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink)" }}
      >
        {values.map((candidate) => (
          <option key={candidate} value={candidate}>
            {candidate}
          </option>
        ))}
      </select>
    </label>
  );
}
export default function ImagesTab({ models }: { models: Model[] }) {
  const [model, setModel] = useQueryParam("model", "");
  const modelGroups = useMemo(() => modelsForTab(models, "images"), [models]);
  const selectedModel = models.find((candidate) => candidate.id === model);
  const nativeGemini = usesGeminiImageInteractions(selectedModel);
  const imageOperation =
    selectedModel?.capabilities?.interfaces?.openai_images?.operations?.generate;
  const parameters = imageOperation?.parameters ?? {};
  const [prompt, setPrompt] = useState("");
  const [size, setSize] = useState("");
  const [quality, setQuality] = useState("");
  const [aspectRatio, setAspectRatio] = useState("");
  const [imageSize, setImageSize] = useState("");
  const [n, setN] = useState<number | null>(null);
  const [images, setImages] = useState<string[]>([]);
  const [responseText, setResponseText] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const stringValues = (name: string): string[] => {
    const parameter = parameters[name];
    return parameter?.support === "supported" &&
      parameter.constraint?.type === "string_enum"
      ? parameter.constraint.values
      : [];
  };
  const sizeValues = nativeGemini ? [] : stringValues("size");
  const qualityValues = nativeGemini ? [] : stringValues("quality");
  const aspectRatioValues = stringValues("aspect_ratio");
  const imageSizeValues = stringValues("image_size");
  const countConstraint =
    !nativeGemini && parameters.n?.support === "supported" &&
    parameters.n.constraint?.type === "integer_range"
      ? parameters.n.constraint
      : undefined;

  useEffect(() => {
    setSize((current) =>
      sizeValues.includes(current) ? current : (sizeValues[0] ?? ""),
    );
    setQuality((current) =>
      qualityValues.includes(current) ? current : (qualityValues[0] ?? ""),
    );
    setAspectRatio((current) =>
      aspectRatioValues.includes(current) ? current : (aspectRatioValues[0] ?? ""),
    );
    setImageSize((current) =>
      imageSizeValues.includes(current) ? current : (imageSizeValues[0] ?? ""),
    );
    setN((current) => {
      if (!countConstraint) return null;
      if (
        current !== null &&
        current >= countConstraint.min &&
        current <= countConstraint.max
      ) {
        return current;
      }
      return countConstraint.min;
    });
  }, [
    model,
    sizeValues.join("\u0000"),
    qualityValues.join("\u0000"),
    aspectRatioValues.join("\u0000"),
    imageSizeValues.join("\u0000"),
    countConstraint?.min,
    countConstraint?.max,
  ]);

  const generate = async () => {
    if (!prompt.trim() || !model || loading) return;
    setError(null);
    setImages([]);
    setResponseText("");
    setLoading(true);
    try {
      const request = buildImageRequest(model, prompt, nativeGemini, {
        size, quality, aspectRatio, imageSize, n,
      });
      const res = await fetch(request.path, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(request.body),
      });
      if (!res.ok) {
        const errBody = await res.text();
        throw new Error(`${res.status}: ${errBody}`);
      }
      const data = await res.json();
      if (nativeGemini) {
        const result = readGeminiImages(data);
        setImages(result.images);
        setResponseText(result.text);
        return;
      }
      const urls = (data.data ?? []).map(
        (d: { url?: string; b64_json?: string }) =>
          d.url ?? (d.b64_json ? `data:image/png;base64,${d.b64_json}` : "")
      );
      setImages(urls.filter(Boolean));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex flex-col gap-4 flex-1">
      <div className="flex items-center gap-3 flex-wrap">
        <ModelSelect
          groups={modelGroups}
          value={model}
          onChange={setModel}
        />
        {models.length > 0 && modelGroups.supported.length === 0 && (
          <span className="text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>
            No models advertise verified image-generation support. You can try
            an unverified candidate or enter a model ID.
          </span>
        )}
        {sizeValues.length > 0 && (
          <ParameterSelect label="Size" value={size} values={sizeValues} onChange={setSize} />
        )}
        {qualityValues.length > 0 && (
          <ParameterSelect
            label="Quality"
            value={quality}
            values={qualityValues}
            onChange={setQuality}
          />
        )}
        {aspectRatioValues.length > 0 && (
          <ParameterSelect
            label="Aspect ratio"
            value={aspectRatio}
            values={aspectRatioValues}
            onChange={setAspectRatio}
          />
        )}
        {imageSizeValues.length > 0 && (
          <ParameterSelect
            label="Image size"
            value={imageSize}
            values={imageSizeValues}
            onChange={setImageSize}
          />
        )}
        {countConstraint && n !== null && (
          <div className="flex items-center gap-2">
            <label className="text-xs" style={{ color: "var(--color-cds-ink-subtle)" }}>Count</label>
            <input
              type="number"
              min={countConstraint.min}
              max={countConstraint.max}
              value={n}
              onChange={(event) => {
                const value = Number.parseInt(event.target.value, 10);
                setN(
                  Math.min(
                    countConstraint.max,
                    Math.max(countConstraint.min, Number.isNaN(value) ? countConstraint.min : value),
                  ),
                );
              }}
              className="text-sm rounded-md px-2 py-1.5 w-14 outline-none"
              style={{ background: "var(--color-cds-field)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink)" }}
            />
          </div>
        )}
      </div>

      <textarea
        value={prompt}
        onChange={(e) => setPrompt(e.target.value)}
        placeholder={model ? "Describe the image you want to generate..." : "Select a model first"}
        disabled={!model}
        rows={3}
        className="text-sm rounded-md px-4 py-3 outline-none resize-y"
        style={{
          background: "var(--color-cds-field)",
          border: "1px solid var(--color-cds-border)",
          color: "var(--color-cds-ink)",
          opacity: model ? 1 : 0.5,
        }}
      />

      <button
        onClick={generate}
        disabled={!prompt.trim() || !model || loading}
        className="self-start px-5 py-2 text-sm font-medium rounded-md transition-colors"
        style={{
          background: prompt.trim() && model && !loading ? "var(--color-cds-primary)" : "var(--color-cds-status-neutral-fill)",
          color: prompt.trim() && model && !loading ? "white" : "var(--color-cds-ink-muted)",
        }}
      >
        {loading ? "Generating..." : "Generate Image"}
      </button>

      {error && (
        <div
          className="px-4 py-2 rounded-lg text-sm"
          style={{ background: "var(--color-cds-status-red-fill)", border: "1px solid var(--color-cds-status-red-border)", color: "var(--color-cds-status-red-text)" }}
        >
          {error}
        </div>
      )}

      {responseText && (
        <div className="text-sm whitespace-pre-wrap" style={{ color: "var(--color-cds-ink-medium)" }}>
          {responseText}
        </div>
      )}

      {images.length > 0 && (
        <div className="grid grid-cols-2 gap-4">
          {images.map((src, i) => (
            <div
              key={i}
              className="rounded-lg overflow-hidden"
              style={{ border: "1px solid var(--color-cds-border)" }}
            >
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img
                src={src}
                alt={`Generated image ${i + 1}`}
                className="w-full h-auto"
              />
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
