export type DecisionQuestionType = "noul" | "choice" | "score" | "box";

export const DECISION_IMAGE_FORMATS = ["png", "jpg", "jpeg", "webp"] as const;
export type DecisionImageFormat = (typeof DECISION_IMAGE_FORMATS)[number];

/** One image the playground sends as a System One asset. The placeholder is
 * the string written into `state` wherever the image belongs. */
export interface DecisionImage {
  placeholder: string;
  format: string;
  dataBase64: string;
  name?: string;
}

export interface DecisionImageAsset {
  type: "image";
  format: DecisionImageFormat;
  data_base64: string;
}

const MIME_FORMATS: Record<string, DecisionImageFormat> = {
  "image/png": "png",
  "image/jpeg": "jpeg",
  "image/webp": "webp",
};

/** Map a picked file's MIME type to an asset `format`, rejecting anything
 * the router does not accept. */
export function imageFormatForMime(mime: string, name = "This file"): DecisionImageFormat {
  const format = MIME_FORMATS[mime.trim().toLowerCase()];
  if (!format) {
    throw new Error(`${name} is ${mime ? `“${mime}”` : "of an unknown type"}; images must be PNG, JPEG, or WebP.`);
  }
  return format;
}

export function imageMimeForFormat(format: string): string {
  return format === "jpg" || format === "jpeg" ? "image/jpeg" : `image/${format}`;
}

/** FileReader yields `data:<mime>;base64,<data>`; the asset wants only `<data>`. */
export function stripDataUrlPrefix(dataUrl: string): string {
  const comma = dataUrl.indexOf(",");
  return dataUrl.startsWith("data:") && comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Put an image placeholder into the state editor: as a JSON string value under
 * a new key when the state is a JSON object, otherwise at the end of the text. */
export function insertPlaceholderIntoState(
  stateText: string,
  stateMode: "text" | "json",
  placeholder: string,
): string {
  if (stateMode === "json") {
    let parsed: unknown;
    try {
      parsed = JSON.parse(stateText);
    } catch {
      parsed = undefined;
    }
    if (isPlainObject(parsed)) {
      let key = "image";
      for (let n = 2; Object.hasOwn(parsed, key); n += 1) key = `image_${n}`;
      return JSON.stringify({ ...parsed, [key]: placeholder }, null, 2);
    }
    if (typeof parsed === "string") {
      return JSON.stringify(parsed ? `${parsed} ${placeholder}` : placeholder);
    }
  }
  if (!stateText) return placeholder;
  return `${stateText}${/\s$/.test(stateText) ? "" : "\n"}${placeholder}`;
}

export interface DecisionQuestionDraft {
  id: string;
  type: DecisionQuestionType;
  instructions: string;
  criteriaText: string;
  trueCriterion: string;
  falseCriterion: string;
}

export interface DecisionsRequestBody {
  model: string;
  state: unknown;
  questions: Record<string, unknown>;
  assets?: Record<string, DecisionImageAsset>;
}

function nonemptyLines(value: string): string[] {
  return value.split("\n").map((line) => line.trim()).filter(Boolean);
}

export function buildDecisionsRequest(
  model: string,
  stateText: string,
  stateMode: "text" | "json",
  drafts: DecisionQuestionDraft[],
  images: DecisionImage[] = [],
): DecisionsRequestBody {
  if (!model.trim()) throw new Error("Select or enter a model.");
  if (!stateText.trim()) throw new Error("State is required.");
  if (drafts.length === 0) throw new Error("Add at least one question.");

  let state: unknown = stateText;
  if (stateMode === "json") {
    try {
      state = JSON.parse(stateText);
    } catch {
      throw new Error("State must be valid JSON.");
    }
    if (state === null || !["string", "object"].includes(typeof state)) {
      throw new Error("JSON state must be a string, object, or array.");
    }
  }

  const questions: Record<string, unknown> = {};
  for (const draft of drafts) {
    const id = draft.id.trim();
    const instructions = draft.instructions.trim();
    if (!id) throw new Error("Every question needs an ID.");
    if (Object.hasOwn(questions, id)) throw new Error(`Question ID “${id}” is duplicated.`);
    if (!instructions) throw new Error(`Question “${id}” needs instructions.`);

    if (draft.type === "box") {
      if (images.length === 0) {
        throw new Error(`Question “${id}” is a box question and needs at least one image.`);
      }
      questions[id] = { type: "box", instructions };
      continue;
    }

    if (draft.type === "noul") {
      const yes = draft.trueCriterion.trim();
      const no = draft.falseCriterion.trim();
      if (Boolean(yes) !== Boolean(no)) {
        throw new Error(`Question “${id}” needs both true and false criteria, or neither.`);
      }
      questions[id] = {
        type: "noul",
        instructions,
        ...(yes ? { criteria: { true: yes, false: no } } : {}),
      };
      continue;
    }

    const lines = nonemptyLines(draft.criteriaText);
    if (draft.type === "score") {
      if (lines.length < 2 || lines.length > 10) {
        throw new Error(`Question “${id}” needs 2 to 10 score levels.`);
      }
      questions[id] = { type: "score", instructions, criteria: lines };
      continue;
    }

    if (lines.length === 0 || lines.length > 255) {
      throw new Error(`Question “${id}” needs 1 to 255 choices.`);
    }
    const criteria: Record<string, string | null> = {};
    for (const line of lines) {
      const [rawName, ...descriptionParts] = line.split(":");
      const name = rawName.trim();
      if (!name) throw new Error(`Question “${id}” has an empty choice name.`);
      if (Object.hasOwn(criteria, name)) throw new Error(`Choice “${name}” is duplicated.`);
      const description = descriptionParts.join(":").trim();
      criteria[name] = description || null;
    }
    questions[id] = { type: "choice", instructions, criteria };
  }

  const assets: Record<string, DecisionImageAsset> = {};
  for (const image of images) {
    const placeholder = image.placeholder.trim();
    const label = image.name ? `Image “${image.name}”` : "An image";
    if (!placeholder) throw new Error(`${label} needs a placeholder.`);
    if (Object.hasOwn(assets, placeholder)) throw new Error(`Image placeholder “${placeholder}” is duplicated.`);
    if (!(DECISION_IMAGE_FORMATS as readonly string[]).includes(image.format)) {
      throw new Error(`${label} has unsupported format “${image.format}”; use png, jpg, jpeg, or webp.`);
    }
    if (!image.dataBase64) throw new Error(`${label} has no data.`);
    assets[placeholder] = {
      type: "image",
      format: image.format as DecisionImageFormat,
      data_base64: image.dataBase64,
    };
  }

  return {
    model: model.trim(),
    state,
    questions,
    ...(images.length > 0 ? { assets } : {}),
  };
}

export interface DecisionBox {
  top: number;
  bottom: number;
  left: number;
  right: number;
}

/** The box from a `box` answer, or null when the answer is not one. */
export function boxFromAnswer(answer: unknown): DecisionBox | null {
  if (!isPlainObject(answer) || !isPlainObject(answer.box)) return null;
  const { top, bottom, left, right } = answer.box;
  const edges = [top, bottom, left, right];
  if (!edges.every((edge) => typeof edge === "number" && Number.isFinite(edge))) return null;
  return { top: top as number, bottom: bottom as number, left: left as number, right: right as number };
}

/** The image a box overlay is drawn on: the only image in the request, or else
 * the only image whose placeholder appears in the state. */
export function imageForBoxOverlay(images: DecisionImage[], stateText: string): DecisionImage | undefined {
  if (images.length === 1) return images[0];
  const named = images.filter((image) => image.placeholder && stateText.includes(image.placeholder));
  return named.length === 1 ? named[0] : undefined;
}

export interface DecisionAnswerSummary {
  value: string;
  detail?: string;
}

function confidence(value: unknown): string | undefined {
  return typeof value === "number" && Number.isFinite(value)
    ? `${(value * 100).toFixed(value >= 0.1 ? 1 : 2)}% confidence`
    : undefined;
}

export function summarizeDecisionAnswer(answer: unknown): DecisionAnswerSummary {
  if (typeof answer !== "object" || answer === null || Array.isArray(answer)) {
    return { value: typeof answer === "string" ? answer : JSON.stringify(answer) };
  }
  const record = answer as Record<string, unknown>;
  const answerConfidence = confidence(record.confidence);
  const box = boxFromAnswer(record);
  if (box) {
    const percent = (edge: number) => `${Math.round(edge * 100)}%`;
    return {
      value: `top ${percent(box.top)} · left ${percent(box.left)} · bottom ${percent(box.bottom)} · right ${percent(box.right)}`,
      detail: answerConfidence,
    };
  }
  if (record.type === "noul" || "noul" in record) {
    const noul = record.noul;
    return {
      value: typeof noul === "number" && Number.isFinite(noul)
        ? `${(noul * 100).toFixed(noul >= 0.1 ? 1 : 2)}% yes probability`
        : String(record.value ?? record.answer ?? "Noul"),
    };
  }
  if (record.type === "choice" || "choice" in record) {
    return { value: String(record.choice ?? record.value ?? "Choice"), detail: answerConfidence };
  }
  if (record.type === "score" || "score" in record) {
    return { value: String(record.score ?? record.value ?? "Score"), detail: answerConfidence };
  }
  return { value: JSON.stringify(answer), detail: answerConfidence };
}
