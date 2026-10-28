import { EMBEDDING_REQUEST_MAX_BYTES, EMBEDDING_REQUEST_LIMIT_LABEL, utf8ByteLength } from "../../lib/embedding-request-limits";
import type { Model } from "./playground-shared";

export type EmbeddingImageMime = "image/png" | "image/jpeg";
export interface EmbeddingImageSize { size: number; mimeType: EmbeddingImageMime }
export interface EncodedEmbeddingImage { dataUrl: string }

const requestTooLarge = () => new Error(`The encoded embedding request exceeds ${EMBEDDING_REQUEST_LIMIT_LABEL}. Remove images or shorten the text.`);

/** Flat content parts describe one document and therefore produce one vector. */
export function buildEmbeddingRequest(model: string, text: string, images: EncodedEmbeddingImage[]) {
  const caption = text.trim();
  if (!model.trim()) throw new Error("Select or enter a model.");
  if (!caption && images.length === 0) throw new Error("Enter text or add a PNG/JPEG image.");
  return {
    model: model.trim(),
    input: images.length === 0 ? caption : [
      ...(caption ? [{ type: "text", text: caption }] : []),
      ...images.map(({ dataUrl }) => ({ type: "image_url", image_url: { url: dataUrl } })),
    ],
  };
}

/** Exact size of our JSON shape before allocating base64 strings. */
export function embeddingRequestBytes(model: string, text: string, images: EmbeddingImageSize[]): number {
  const placeholders = images.map(({ mimeType }) => ({ dataUrl: `data:${mimeType};base64,` }));
  const body = JSON.stringify(buildEmbeddingRequest(model.trim() || "_", text, placeholders));
  return utf8ByteLength(body) + images.reduce((total, image) => total + 4 * Math.ceil(image.size / 3), 0);
}

export function assertEmbeddingRequestFits(model: string, text: string, images: EmbeddingImageSize[]): void {
  if (embeddingRequestBytes(model, text, images) > EMBEDDING_REQUEST_MAX_BYTES) throw requestTooLarge();
}

export function serializeEmbeddingRequest(model: string, text: string, images: EncodedEmbeddingImage[]): string {
  const body = JSON.stringify(buildEmbeddingRequest(model, text, images));
  if (utf8ByteLength(body) > EMBEDDING_REQUEST_MAX_BYTES) throw requestTooLarge();
  return body;
}

/** Unknown metadata permits an explicit unverified attempt; exclusions block it. */
export function embeddingEligibility(model: Model | undefined, hasText: boolean, imageMimes: string[]): { reason?: string; unverified: boolean } {
  const capabilities = model?.capabilities;
  const checks = [
    [capabilities?.surfaces?.embeddings, "This model does not support embeddings."],
    [capabilities?.modalities?.output?.embeddings, "This model does not produce embedding vectors."],
    [capabilities?.interfaces?.openai_embeddings?.operations?.create?.support, "This model does not support the embeddings endpoint."],
    ...(hasText ? [[capabilities?.modalities?.input?.text, "This model does not support text input."]] : []),
    ...(imageMimes.length > 0 ? [[capabilities?.modalities?.input?.image, "This model does not support image input. Remove the images or select another model."]] : []),
  ];
  const excluded = checks.find(([support]) => support === "unsupported");
  if (excluded) return { reason: excluded[1], unverified: false };
  if (imageMimes.some((mime) => mime !== "image/png" && mime !== "image/jpeg")) {
    return { reason: "Only PNG and JPEG images are supported.", unverified: false };
  }
  const limits = capabilities?.limits?.embedding_input;
  if (limits?.max_images_per_document !== undefined && imageMimes.length > limits.max_images_per_document) {
    return { reason: `This model accepts at most ${limits.max_images_per_document} images in one document. Remove images or select another model.`, unverified: false };
  }
  if (limits?.image_mime_types && imageMimes.some((mime) => !limits.image_mime_types!.some((allowed) => allowed.toLowerCase() === mime))) {
    return { reason: "An attached image format is not supported by this model. Remove it or select another model.", unverified: false };
  }
  return { unverified: checks.some(([support]) => support !== "supported") };
}

export function imageMimeFromSignature(bytes: Uint8Array): EmbeddingImageMime | undefined {
  if ([137, 80, 78, 71, 13, 10, 26, 10].every((byte, index) => bytes[index] === byte)) return "image/png";
  if (bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255) return "image/jpeg";
  return undefined;
}

/** Header sniffing is followed by a real browser decode, without base64 encoding. */
export async function validateEmbeddingImage(file: File): Promise<EmbeddingImageMime> {
  const mimeType = imageMimeFromSignature(new Uint8Array(await file.slice(0, 8).arrayBuffer()));
  if (!mimeType || (file.type && file.type.toLowerCase() !== mimeType)) {
    throw new Error("Choose a PNG or JPEG file whose contents match its image type.");
  }
  const blob = file.slice(0, file.size, mimeType);
  try {
    if (typeof createImageBitmap === "function") {
      const bitmap = await createImageBitmap(blob);
      try {
        if (bitmap.width < 1 || bitmap.height < 1) throw new Error("empty image");
      } finally { bitmap.close(); }
    } else {
      const url = URL.createObjectURL(blob);
      try {
        await new Promise<void>((resolve, reject) => {
          const image = new Image();
          image.onload = () => image.naturalWidth > 0 && image.naturalHeight > 0 ? resolve() : reject(new Error("empty image"));
          image.onerror = () => reject(new Error("invalid image"));
          image.src = url;
        });
      } finally { URL.revokeObjectURL(url); }
    }
  } catch {
    throw new Error("The browser could not decode this image. Choose a valid PNG or JPEG file.");
  }
  return mimeType;
}

/** Called only on Run. Abort releases FileReader work when a draft is cleared. */
export function encodeEmbeddingImage(file: File, mimeType: EmbeddingImageMime, signal: AbortSignal): Promise<EncodedEmbeddingImage> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    const cleanup = () => signal.removeEventListener("abort", abort);
    const abort = () => { reader.abort(); cleanup(); reject(new DOMException("Aborted", "AbortError")); };
    if (signal.aborted) { abort(); return; }
    signal.addEventListener("abort", abort, { once: true });
    reader.onerror = () => { cleanup(); reject(new Error(`Could not read ${file.name}.`)); };
    reader.onabort = () => { cleanup(); reject(new DOMException("Aborted", "AbortError")); };
    reader.onload = () => {
      cleanup();
      if (typeof reader.result !== "string" || !reader.result.startsWith(`data:${mimeType};base64,`)) {
        reject(new Error(`Could not encode ${file.name}.`));
      } else resolve({ dataUrl: reader.result });
    };
    reader.readAsDataURL(file.slice(0, file.size, mimeType));
  });
}

function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

export function readEmbeddingResult(value: unknown): { vector: number[]; promptTokens?: number; totalTokens?: number } {
  const response = record(value);
  const data = response?.data;
  const vector = Array.isArray(data) && data.length === 1 ? record(data[0])?.embedding : undefined;
  if (!Array.isArray(vector) || vector.length === 0 || !vector.every((entry) => typeof entry === "number" && Number.isFinite(entry))) {
    throw new Error("The provider returned an invalid embedding response: expected one nonempty numeric vector.");
  }
  const usage = record(response?.usage);
  const count = (value: unknown) => typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : undefined;
  return { vector, promptTokens: count(usage?.prompt_tokens), totalTokens: count(usage?.total_tokens) };
}

export function embeddingResponseError(status: number, body: string, retryAfter: string | null): string {
  let message = body.trim();
  try {
    const value = JSON.parse(body);
    const error = record(record(value)?.error);
    if (typeof error?.message === "string") message = error.message;
  } catch { /* Plain-text provider and BFF errors remain useful. */ }
  return `${status}: ${message.slice(0, 2000) || "Embedding request failed."}${retryAfter ? ` (Retry-After: ${retryAfter})` : ""}`;
}
