export interface ImageOptions {
  size: string;
  quality: string;
  aspectRatio: string;
  imageSize: string;
  n: number | null;
}

export function buildImageRequest(
  model: string,
  prompt: string,
  nativeGemini: boolean,
  options: ImageOptions,
) {
  const { size, quality, aspectRatio, imageSize, n } = options;
  if (nativeGemini) {
    return {
      path: "/api/v1beta/interactions",
      body: {
        model,
        input: prompt.trim(),
        stream: false,
        store: false,
        // Supply an output bound for quota reservation, even for new models
        // whose catalog entry has no known maximum output token count.
        generation_config: { max_output_tokens: 8192 },
        response_format: {
          type: "image",
          delivery: "inline",
          ...(aspectRatio ? { aspect_ratio: aspectRatio } : {}),
          ...(imageSize ? { image_size: imageSize === "512px" ? "512" : imageSize } : {}),
        },
      },
    };
  }
  return {
    path: "/api/v1/images/generations",
    body: {
      model,
      prompt: prompt.trim(),
      ...(size ? { size } : {}),
      ...(quality ? { quality } : {}),
      ...(aspectRatio ? { aspect_ratio: aspectRatio } : {}),
      ...(imageSize ? { image_size: imageSize } : {}),
      ...(n !== null ? { n } : {}),
    },
  };
}

function object(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function imageSource(part: Record<string, unknown>): string {
  if (typeof part.data === "string" && part.data.length > 0) {
    // Use the returned MIME type rather than assuming all images are PNGs.
    if (typeof part.mime_type !== "string" ||
        !/^image\/(png|jpeg|webp|heic|heif|gif|bmp|tiff)$/.test(part.mime_type)) {
      throw new Error("Gemini returned an image with an unsupported MIME type.");
    }
    return `data:${part.mime_type};base64,${part.data}`;
  }
  if (typeof part.uri === "string") {
    try {
      const url = new URL(part.uri);
      if (url.protocol === "https:" || url.protocol === "http:") return url.href;
    } catch {
      // Fall through to the same error for missing or unusable image data.
    }
  }
  throw new Error("Gemini returned an image without displayable data.");
}

export function readGeminiImages(value: unknown): { images: string[]; text: string } {
  const response = object(value);
  if (!response) throw new Error("Invalid Gemini image response.");
  const diagnostics = Array.isArray(response.errors)
    ? response.errors.map((error) => object(error)?.message).filter((message) => typeof message === "string").join("\n")
    : "";
  if (response.status !== "completed") {
    throw new Error(diagnostics || `Gemini image generation ${String(response.status ?? "did not complete")}.`);
  }
  const images: string[] = [];
  const text: string[] = [];
  for (const step of Array.isArray(response.steps) ? response.steps : []) {
    const output = object(step);
    if (output?.type !== "model_output" || !Array.isArray(output.content)) continue;
    for (const value of output.content) {
      const part = object(value);
      if (part?.type === "image") images.push(imageSource(part));
      else if (part?.type === "text" && typeof part.text === "string") text.push(part.text);
    }
  }
  if (images.length === 0) {
    throw new Error(text.join("\n") || diagnostics || "Gemini returned no images. Try a different prompt or an image-generation model.");
  }
  return { images, text: text.join("\n") };
}
