import { describe, expect, it } from "vitest";
import { buildImageRequest, readGeminiImages, type ImageOptions } from "./image-generation";
import { modelsForTab, usesGeminiImageInteractions, type Model } from "./playground-shared";

const native: Model = {
  id: "team/image-model",
  owned_by: "google",
  capabilities: {
    surfaces: { image_generation: "supported" },
    modalities: { input: { text: "supported" }, output: { image: "supported" } },
    interfaces: {
      gemini_interactions: { operations: { create: { support: "supported", features: { streaming: "unsupported" } } } },
      openai_images: { operations: { generate: { support: "unsupported" } } },
    },
  },
};
const options: ImageOptions = { size: "1024x1024", quality: "high", n: 2, aspectRatio: "16:9", imageSize: "2K" };
const image = (data: string, mime_type: string) => ({ type: "image", data, mime_type });
const output = (...content: unknown[]) => ({ type: "model_output", content });

describe("image request routing", () => {
  it("includes native-only image models without requiring streaming", () => {
    expect(modelsForTab([native], "images").supported).toEqual([native]);
    expect(usesGeminiImageInteractions(native)).toBe(true);
  });

  it("uses the advertised native interface when image-only models have unknown Create support", () => {
    const model = structuredClone(native);
    model.capabilities!.interfaces!.gemini_interactions!.operations!.create!.support = "unknown";
    model.capabilities!.interfaces!.openai_images!.operations!.generate!.support = "supported";
    expect(usesGeminiImageInteractions(model)).toBe(true);
    expect(buildImageRequest(model.id, "A panda", usesGeminiImageInteractions(model), options).path)
      .toBe("/api/v1beta/interactions");
  });

  it("does not infer native support from the model name or override disabled interfaces", () => {
    expect(usesGeminiImageInteractions(undefined)).toBe(false);
    expect(usesGeminiImageInteractions({ id: "gemini/image", owned_by: "google" })).toBe(false);
    const model = structuredClone(native);
    model.capabilities!.interfaces!.gemini_interactions!.operations!.create!.support = "unsupported";
    expect(usesGeminiImageInteractions(model)).toBe(false);
    expect(modelsForTab([model], "images").supported).toEqual([]);
  });

  it("sends native image settings with a quota bound and no OpenAI-only fields", () => {
    expect(buildImageRequest(native.id, " A panda ", true, options)).toEqual({
      path: "/api/v1beta/interactions",
      body: {
        model: native.id, input: "A panda", store: false, stream: false,
        generation_config: { max_output_tokens: 8192 },
        response_format: { type: "image", delivery: "inline", aspect_ratio: "16:9", image_size: "2K" },
      },
    });
    const small = buildImageRequest(native.id, "A panda", true, { ...options, imageSize: "512px" });
    expect(small.body.response_format?.image_size).toBe("512");
  });

  it("routes Imagen and its aliases through the images endpoint using capabilities", () => {
    for (const id of ["gemini/imagen-4.0-generate-001", "gemini/models/imagen-4.0-generate-001", "team/illustrator"]) {
      const model = structuredClone(native);
      model.id = id;
      model.capabilities!.interfaces!.gemini_interactions!.operations!.create!.support = "unsupported";
      model.capabilities!.interfaces!.openai_images!.operations!.generate!.support = "supported";
      expect(modelsForTab([model], "images").supported).toEqual([model]);
      expect(usesGeminiImageInteractions(model)).toBe(false);
      expect(buildImageRequest(model.id, "A panda", usesGeminiImageInteractions(model), options).path)
        .toBe("/api/v1/images/generations");
    }
  });

  it("leaves unspecified image settings to Gemini", () => {
    const request = buildImageRequest(native.id, "A panda", true, { ...options, imageSize: "", aspectRatio: "" });
    expect(request.body.response_format).toEqual({ type: "image", delivery: "inline" });
  });

  it("preserves OpenAI image requests", () => {
    expect(buildImageRequest("openai/gpt-image-1", " A panda ", false, options)).toEqual({
      path: "/api/v1/images/generations",
      body: {
        model: "openai/gpt-image-1", prompt: "A panda", size: "1024x1024", quality: "high", n: 2,
        aspect_ratio: "16:9", image_size: "2K",
      },
    });
  });
});

describe("native image responses", () => {
  it("renders image content in output order with its MIME type, excluding input and thought images", () => {
    expect(readGeminiImages({ status: "completed", steps: [
      { type: "user_input", content: [image("input", "image/png")] },
      { type: "thought", signature: "private", summary: [image("thought", "image/png")] },
      output({ type: "text", text: "Two pandas." }, image("first", "image/jpeg")),
      output(image("second", "image/webp"), { type: "text", text: "Enjoy!" }),
    ] })).toEqual({ images: ["data:image/jpeg;base64,first", "data:image/webp;base64,second"], text: "Two pandas.\nEnjoy!" });
  });

  it("supports image URIs and prefers inline data when both are present", () => {
    expect(readGeminiImages({ status: "completed", steps: [output(
      { type: "image", uri: "https://example.com/image.png" },
      { ...image("inline", "image/png"), uri: "https://example.com/unused.png" },
    )] }).images).toEqual(["https://example.com/image.png", "data:image/png;base64,inline"]);
  });

  it.each(["failed", "incomplete", "cancelled", "requires_action", "in_progress"])("reports %s instead of displaying partial images", (status) => {
    expect(() => readGeminiImages({ status, steps: [output(image("partial", "image/png"))] })).toThrow(status);
  });

  it("shows provider diagnostics and text-only refusals", () => {
    expect(() => readGeminiImages({ status: "failed", errors: [{ message: "Image generation failed." }] })).toThrow("Image generation failed.");
    expect(() => readGeminiImages({ status: "completed", steps: [output({ type: "text", text: "I cannot generate that image." })] })).toThrow("I cannot generate that image.");
    expect(() => readGeminiImages({ status: "completed", steps: [] })).toThrow("no images");
    expect(() => readGeminiImages(null)).toThrow("Invalid Gemini image response");
  });

  it("rejects unusable images and non-image data URLs", () => {
    for (const part of [
      image("bad", "text/html"), image("bad", "image/svg+xml"), { type: "image", data: "missing MIME" },
      { type: "image", uri: "javascript:alert(1)" }, { type: "image", uri: "gs://private/image" }, { type: "image" },
    ]) {
      expect(() => readGeminiImages({ status: "completed", steps: [output(part)] })).toThrow();
    }
  });
});
