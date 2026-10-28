import { describe, expect, it } from "vitest";
import {
  buildDecisionsRequest,
  imageForBoxOverlay,
  imageFormatForMime,
  insertPlaceholderIntoState,
  stripDataUrlPrefix,
  summarizeDecisionAnswer,
  type DecisionImage,
  type DecisionQuestionDraft,
} from "./decisions-playground";

const draft = (value: Partial<DecisionQuestionDraft> = {}): DecisionQuestionDraft => ({
  id: "approve",
  type: "noul",
  instructions: "Should this be approved?",
  criteriaText: "",
  trueCriterion: "Meets policy",
  falseCriterion: "Violates policy",
  ...value,
});

describe("Decisions playground request builder", () => {
  it("builds Noul, Choice, and Score questions from the editor", () => {
    expect(buildDecisionsRequest("typesafe/jev-latest", '{"amount":120}', "json", [
      draft(),
      draft({ id: "team", type: "choice", instructions: "Which team?", criteriaText: "risk: Risk review\nops", trueCriterion: "", falseCriterion: "" }),
      draft({ id: "severity", type: "score", instructions: "How severe?", criteriaText: "low\nmedium\nhigh", trueCriterion: "", falseCriterion: "" }),
    ])).toEqual({
      model: "typesafe/jev-latest",
      state: { amount: 120 },
      questions: {
        approve: { type: "noul", instructions: "Should this be approved?", criteria: { true: "Meets policy", false: "Violates policy" } },
        team: { type: "choice", instructions: "Which team?", criteria: { risk: "Risk review", ops: null } },
        severity: { type: "score", instructions: "How severe?", criteria: ["low", "medium", "high"] },
      },
    });
  });

  it("reports editor errors before sending", () => {
    expect(() => buildDecisionsRequest("typesafe/jev-latest", "ticket", "text", [draft({ falseCriterion: "" })]))
      .toThrow("both true and false criteria");
    expect(() => buildDecisionsRequest("typesafe/jev-latest", "{", "json", [draft()]))
      .toThrow("valid JSON");
  });
});

describe("Decisions playground images", () => {
  const image = (value: Partial<DecisionImage> = {}): DecisionImage => ({
    placeholder: "img-1",
    format: "png",
    dataBase64: "iVBORw0KGgo=",
    name: "camera.png",
    ...value,
  });
  const box = draft({ id: "object", type: "box", instructions: "Where is the dog?", trueCriterion: "", falseCriterion: "" });

  it("sends images as assets and box questions without criteria", () => {
    expect(buildDecisionsRequest("typesafe/jev-preview", '{"camera":"img-1"}', "json", [box], [image()])).toEqual({
      model: "typesafe/jev-preview",
      state: { camera: "img-1" },
      questions: { object: { type: "box", instructions: "Where is the dog?" } },
      assets: { "img-1": { type: "image", format: "png", data_base64: "iVBORw0KGgo=" } },
    });
  });

  it("omits assets when there are no images", () => {
    expect(buildDecisionsRequest("typesafe/jev-latest", "ticket", "text", [draft()])).not.toHaveProperty("assets");
    expect(buildDecisionsRequest("typesafe/jev-latest", "ticket", "text", [draft()], [])).not.toHaveProperty("assets");
  });

  it("validates image formats and placeholders", () => {
    expect(() => buildDecisionsRequest("typesafe/jev-preview", "img-1", "text", [draft()], [image({ format: "gif" })]))
      .toThrow("unsupported format “gif”");
    expect(() => buildDecisionsRequest("typesafe/jev-preview", "img-1", "text", [draft()], [image(), image()]))
      .toThrow("duplicated");
    for (const format of ["png", "jpg", "jpeg", "webp"]) {
      expect(buildDecisionsRequest("typesafe/jev-preview", "img-1", "text", [draft()], [image({ format })]).assets?.["img-1"].format)
        .toBe(format);
    }
    expect(imageFormatForMime("image/png")).toBe("png");
    expect(imageFormatForMime("image/jpeg")).toBe("jpeg");
    expect(imageFormatForMime("image/webp")).toBe("webp");
    expect(() => imageFormatForMime("image/gif", "anim.gif")).toThrow("PNG, JPEG, or WebP");
    expect(stripDataUrlPrefix("data:image/png;base64,AAAA")).toBe("AAAA");
  });

  it("rejects a box question when the request has no image", () => {
    expect(() => buildDecisionsRequest("typesafe/jev-preview", "find it", "text", [box]))
      .toThrow("needs at least one image");
  });

  it("inserts placeholders into JSON objects and text", () => {
    expect(JSON.parse(insertPlaceholderIntoState('{"image":"a"}', "json", "b"))).toEqual({ image: "a", image_2: "b" });
    expect(insertPlaceholderIntoState("Look at", "text", "b")).toBe("Look at\nb");
    expect(insertPlaceholderIntoState("[1]", "json", "b")).toBe("[1]\nb");
    expect(insertPlaceholderIntoState('"see"', "json", "b")).toBe('"see b"');
  });

  it("picks the overlay image from the request or the state", () => {
    const first = image();
    const second = image({ placeholder: "img-2" });
    expect(imageForBoxOverlay([first], "")).toBe(first);
    expect(imageForBoxOverlay([first, second], '{"camera":"img-2"}')).toBe(second);
    expect(imageForBoxOverlay([first, second], "img-1 img-2")).toBeUndefined();
  });
});

describe("Decisions answer summaries", () => {
  it("renders probability, choice, and score answers", () => {
    expect(summarizeDecisionAnswer({ type: "noul", noul: 0.875 })).toEqual({ value: "87.5% yes probability" });
    expect(summarizeDecisionAnswer({ type: "choice", choice: "risk", confidence: 0.9 })).toEqual({ value: "risk", detail: "90.0% confidence" });
    expect(summarizeDecisionAnswer({ type: "score", score: 4 })).toEqual({ value: "4", detail: undefined });
  });

  it("renders box edges as percentages", () => {
    expect(summarizeDecisionAnswer({ type: "box", box: { top: 0.12, bottom: 0.581, left: 0.3, right: 0.709 }, confidence: 0.82 }))
      .toEqual({ value: "top 12% · left 30% · bottom 58% · right 71%", detail: "82.0% confidence" });
    expect(summarizeDecisionAnswer({ type: "box", box: { top: 0, bottom: 1, left: 0, right: 1 }, confidence: null }))
      .toEqual({ value: "top 0% · left 0% · bottom 100% · right 100%", detail: undefined });
  });
});
