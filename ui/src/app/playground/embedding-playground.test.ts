import { afterEach, describe, expect, it, vi } from "vitest";
import { EMBEDDING_REQUEST_MAX_BYTES, utf8ByteLength } from "../../lib/embedding-request-limits";
import { assertEmbeddingRequestFits, buildEmbeddingRequest, embeddingEligibility, embeddingRequestBytes, embeddingResponseError, encodeEmbeddingImage, imageMimeFromSignature, readEmbeddingResult, serializeEmbeddingRequest, validateEmbeddingImage } from "./embedding-playground";
import { modelsForTab, type Model } from "./playground-shared";

const png = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10, 0]);
const jpeg = new Uint8Array([255, 216, 255, 224]);
const image = { dataUrl: "data:image/png;base64,aGk=" };
const model: Model = {
  id: "custom/aliased-image-model", owned_by: "custom",
  capabilities: {
    surfaces: { embeddings: "supported" },
    modalities: { input: { text: "supported", image: "supported" }, output: { embeddings: "supported" } },
    interfaces: { openai_embeddings: { operations: { create: { support: "supported" } } } },
    limits: { max_input_tokens: 8192, embedding_input: { max_images_per_document: 6, image_mime_types: ["image/png", "image/jpeg"] } },
  },
};
const copy = () => structuredClone(model);
const file = (bytes: Uint8Array, type: string) => new File([bytes as BlobPart], "image", { type });
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("embedding document requests", () => {
  it("keeps text-only wire shape and forms one image-only or combined document", () => {
    expect(buildEmbeddingRequest(" custom/model ", "  hello  ", [])).toEqual({ model: "custom/model", input: "hello" });
    expect(buildEmbeddingRequest(model.id, "", [image])).toEqual({ model: model.id, input: [{ type: "image_url", image_url: { url: image.dataUrl } }] });
    expect(buildEmbeddingRequest(model.id, " caption ", [image, image])).toEqual({ model: model.id, input: [
      { type: "text", text: "caption" }, { type: "image_url", image_url: { url: image.dataUrl } }, { type: "image_url", image_url: { url: image.dataUrl } },
    ] });
    expect(() => buildEmbeddingRequest("", "hello", [])).toThrow("model");
    expect(() => buildEmbeddingRequest(model.id, " \n ", [])).toThrow("Enter text");
  });
  it("preflights exact UTF-8 and JSON escaping including base64 padding", () => {
    const text = 'caption 雪😀 "quotes" \\slash\nline\u0000';
    for (const size of [1, 2, 3, 4, 5, 6, 50]) {
      const dataUrl = `data:image/jpeg;base64,${Buffer.alloc(size).toString("base64")}`;
      const body = serializeEmbeddingRequest('model/雪"', text, [{ dataUrl }]);
      expect(embeddingRequestBytes('model/雪"', text, [{ size, mimeType: "image/jpeg" }])).toBe(utf8ByteLength(body));
    }
    const textOnly = serializeEmbeddingRequest(model.id, text, []);
    expect(embeddingRequestBytes(model.id, text, [])).toBe(utf8ByteLength(textOnly));
    expect(utf8ByteLength(textOnly)).toBeGreaterThan(textOnly.length);
  });
  it("counts UTF-8 bytes exactly like TextEncoder without encoding", () => {
    for (const text of ["", "ascii", "é", "雪", "😀", "\ud800", "\udc00x", "a\ud83d", "\ud83d\ude00\ud83d"]) {
      expect(utf8ByteLength(text)).toBe(new TextEncoder().encode(text).byteLength);
    }
  });
  it("accepts exact encoded limit and rejects another base64 group before encoding", () => {
    const overhead = embeddingRequestBytes("m", "", [{ size: 0, mimeType: "image/png" }]);
    const fittingSize = Math.floor((EMBEDDING_REQUEST_MAX_BYTES - overhead) / 4) * 3;
    const fitting = { size: fittingSize, mimeType: "image/png" as const };
    expect(() => assertEmbeddingRequestFits("m", "", [fitting])).not.toThrow();
    expect(() => assertEmbeddingRequestFits("m", "", [{ ...fitting, size: fittingSize + 1 }])).toThrow("4 MiB");
    const dataUrl = `data:image/png;base64,${Buffer.alloc(fittingSize).toString("base64")}`;
    const slack = EMBEDDING_REQUEST_MAX_BYTES - embeddingRequestBytes("m", "", [fitting]);
    const exactModel = `m${"x".repeat(slack)}`;
    expect(utf8ByteLength(serializeEmbeddingRequest(exactModel, "", [{ dataUrl }]))).toBe(EMBEDDING_REQUEST_MAX_BYTES);
    expect(() => serializeEmbeddingRequest(`${exactModel}x`, "", [{ dataUrl }])).toThrow("4 MiB");
  });
});

describe("metadata-driven embedding eligibility", () => {
  it("supports aliases and image-only models without inferring names", () => {
    expect(embeddingEligibility(model, true, ["image/png"])).toEqual({ unverified: false });
    const imageOnly = copy();
    imageOnly.capabilities!.modalities!.input!.text = "unsupported";
    expect(modelsForTab([imageOnly], "embeddings").supported).toEqual([imageOnly]);
    expect(embeddingEligibility(imageOnly, false, ["image/png"])).toEqual({ unverified: false });
    expect(embeddingEligibility(imageOnly, true, ["image/png"]).reason).toContain("text input");
  });
  it("revalidates retained files against count and format rules after model changes", () => {
    expect(embeddingEligibility(model, false, Array(6).fill("image/png")).reason).toBeUndefined();
    expect(embeddingEligibility(model, false, Array(7).fill("image/png")).reason).toContain("at most 6");
    const textOnly = copy();
    textOnly.capabilities!.modalities!.input!.image = "unsupported";
    expect(embeddingEligibility(textOnly, false, ["image/png"]).reason).toContain("image input");
    expect(embeddingEligibility(textOnly, true, []).reason).toBeUndefined();
    const jpegOnly = copy();
    jpegOnly.capabilities!.limits!.embedding_input!.image_mime_types = ["image/jpeg"];
    expect(embeddingEligibility(jpegOnly, false, ["image/png"]).reason).toContain("format");
    expect(embeddingEligibility(model, false, ["image/webp"]).reason).toContain("PNG and JPEG");
    expect(embeddingEligibility(model, false, ["image/jpeg"]).reason).toBeUndefined();
  });
  it("permits unknown models with a notice and honors explicit exclusions", () => {
    expect(embeddingEligibility(undefined, true, ["image/png"])).toEqual({ unverified: true });
    const unknown = copy();
    unknown.capabilities!.modalities!.input!.image = "unknown";
    expect(embeddingEligibility(unknown, false, ["image/png"])).toEqual({ unverified: true });
    for (const field of ["surface", "output", "operation"] as const) {
      const excluded = copy();
      if (field === "surface") excluded.capabilities!.surfaces!.embeddings = "unsupported";
      if (field === "output") excluded.capabilities!.modalities!.output!.embeddings = "unsupported";
      if (field === "operation") excluded.capabilities!.interfaces!.openai_embeddings!.operations!.create!.support = "unsupported";
      expect(embeddingEligibility(excluded, true, ["image/png"]).reason).toBeTruthy();
      expect(modelsForTab([excluded], "embeddings")).toEqual({ supported: [], unverified: [] });
    }
    const legacy: Model = { id: "custom/embedding", owned_by: "custom" };
    expect(modelsForTab([legacy], "embeddings").unverified).toEqual([legacy]);
  });
});

describe("local image inspection and encoding", () => {
  it("sniffs real PNG/JPEG signatures and rejects other content", () => {
    expect(imageMimeFromSignature(png)).toBe("image/png");
    expect(imageMimeFromSignature(jpeg)).toBe("image/jpeg");
    expect(imageMimeFromSignature(new TextEncoder().encode("GIF89a"))).toBeUndefined();
    expect(imageMimeFromSignature(new Uint8Array())).toBeUndefined();
  });
  it("requires browser decoding and releases bitmap resources", async () => {
    const close = vi.fn();
    const decode = vi.fn().mockResolvedValue({ width: 1, height: 2, close });
    vi.stubGlobal("createImageBitmap", decode);
    expect(await validateEmbeddingImage(file(png, "image/png"))).toBe("image/png");
    expect(await validateEmbeddingImage(file(jpeg, ""))).toBe("image/jpeg");
    expect(close).toHaveBeenCalledTimes(2);
    await expect(validateEmbeddingImage(file(png, "image/jpeg"))).rejects.toThrow("contents match");
    await expect(validateEmbeddingImage(file(new Uint8Array([1]), "image/png"))).rejects.toThrow("PNG or JPEG");
    expect(decode).toHaveBeenCalledTimes(2);
    decode.mockRejectedValueOnce(new Error("bad pixels"));
    await expect(validateEmbeddingImage(file(png, "image/png"))).rejects.toThrow("could not decode");
    decode.mockResolvedValueOnce({ width: 0, height: 2, close });
    await expect(validateEmbeddingImage(file(png, "image/png"))).rejects.toThrow("could not decode");
    expect(close).toHaveBeenCalledTimes(3);
  });
  it("releases temporary decode URLs on success and failure in the Image fallback", async () => {
    vi.stubGlobal("createImageBitmap", undefined);
    const create = vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:local-preview");
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    let fail = false;
    vi.stubGlobal("Image", class {
      naturalWidth = 1; naturalHeight = 1;
      onload?: () => void; onerror?: () => void;
      set src(_value: string) { queueMicrotask(() => fail ? this.onerror?.() : this.onload?.()); }
    });
    await expect(validateEmbeddingImage(file(png, "image/png"))).resolves.toBe("image/png");
    fail = true;
    await expect(validateEmbeddingImage(file(png, "image/png"))).rejects.toThrow("could not decode");
    expect(create).toHaveBeenCalledTimes(2);
    expect(revoke.mock.calls).toEqual([["blob:local-preview"], ["blob:local-preview"]]);
  });
  it("supports active and already-cancelled FileReader work", async () => {
    const read = vi.fn(); const abort = vi.fn();
    vi.stubGlobal("FileReader", class { readAsDataURL = read; abort = abort; });
    const controller = new AbortController();
    const encoding = encodeEmbeddingImage(file(png, "image/png"), "image/png", controller.signal);
    expect(read).toHaveBeenCalledTimes(1);
    controller.abort();
    await expect(encoding).rejects.toMatchObject({ name: "AbortError" });
    await expect(encodeEmbeddingImage(file(png, "image/png"), "image/png", controller.signal)).rejects.toMatchObject({ name: "AbortError" });
    expect(read).toHaveBeenCalledTimes(1);
    expect(abort).toHaveBeenCalledTimes(2);
  });
});

describe("embedding responses", () => {
  it("reads one vector and only valid reported counts", () => {
    expect(readEmbeddingResult({ data: [{ embedding: [0, -0.5, 1] }], usage: { prompt_tokens: 0, total_tokens: 3 } })).toEqual({ vector: [0, -0.5, 1], promptTokens: 0, totalTokens: 3 });
    expect(readEmbeddingResult({ data: [{ embedding: [1] }], usage: { prompt_tokens: -1, total_tokens: "2" } })).toEqual({ vector: [1], promptTokens: undefined, totalTokens: undefined });
  });
  it("rejects missing, empty, nonnumeric and multiple vectors", () => {
    for (const value of [null, {}, { data: [] }, { data: [{ embedding: [] }] }, { data: [{ embedding: ["1"] }] }, { data: [{ embedding: [Infinity] }] }, { data: [{ embedding: [1] }, { embedding: [2] }] }]) {
      expect(() => readEmbeddingResult(value)).toThrow("expected one nonempty numeric vector");
    }
  });
  it("keeps provider errors and Retry-After including non-JSON errors", () => {
    expect(embeddingResponseError(429, '{"error":{"message":"quota exceeded"}}', "30")).toBe("429: quota exceeded (Retry-After: 30)");
    expect(embeddingResponseError(502, "upstream unavailable", null)).toBe("502: upstream unavailable");
    expect(embeddingResponseError(503, "", null)).toContain("request failed");
  });
});
