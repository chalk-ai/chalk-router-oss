import { create } from "@bufbuild/protobuf";
import { Classification, JudgeMode, RouteGridSchema, RouteJudgeBackendSchema, type RouteGrid } from "@/gen/chalk/router/v1/model_route_pb";

export type RouteTarget = { connection_id: string; upstream_model_id: string };
export type HeuristicBackend = {
  type: "heuristic";
  use_default_keywords?: boolean;
  added_hard_keywords: string[];
  removed_hard_keywords: string[];
  code_blocks_are_hard?: boolean;
  max_easy_input_bytes?: number;
};
export type JudgeBackend = HeuristicBackend | { type: "completion" | "decisions"; target: RouteTarget; timeout_ms: number };
export type JudgeOutcomeConfig = { model: string; description: string; fallbacks?: string[] | null };
export type RouteJudge = { mode: "off" | "shadow" | "live"; classification: "task" | "difficulty"; backend: JudgeBackend; outcomes: Record<string, JudgeOutcomeConfig> };
export type RouteGridConfig = { default_target: RouteTarget; fallbacks: Record<string, string[]>; judge: RouteJudge | null };
export type RouteConfiguration = RouteTarget & { routing?: RouteGridConfig | null };

export const DEFAULT_HARD_KEYWORDS = ["step by step", "step-by-step", "prove ", "proof ", "theorem", "algorithm", "refactor", "debug", "stack trace", "regression"];
export const encodeTarget = (target: RouteTarget) => `${target.connection_id}/${target.upstream_model_id}`;
export function decodeTarget(value: string): RouteTarget {
  const slash = value.indexOf("/");
  return slash < 0 ? { connection_id: "", upstream_model_id: value } : { connection_id: value.slice(0, slash), upstream_model_id: value.slice(slash + 1) };
}
export const effectiveTarget = (route: RouteConfiguration): RouteTarget => route.routing?.default_target ?? route;
/** Concrete models used by custom routing, including its remote judge. */
export function routeReferencedTargets(route: RouteConfiguration): RouteTarget[] {
  const targets = new Map<string, RouteTarget>();
  const add = (target: RouteTarget) => targets.set(encodeTarget(target), target);
  add(effectiveTarget(route));
  const grid = route.routing;
  if (grid) {
    for (const [model, chain] of Object.entries(grid.fallbacks)) {
      for (const ref of [model, ...chain]) add(decodeTarget(ref));
    }
    if (grid.judge) {
      if (grid.judge.backend.type !== "heuristic") add(grid.judge.backend.target);
      for (const outcome of Object.values(grid.judge.outcomes)) {
        for (const ref of [outcome.model, ...(outcome.fallbacks ?? [])]) add(decodeTarget(ref));
      }
    }
  }
  return [...targets.values()];
}

export function routeConfigurationPayload(route: RouteConfiguration) {
  return route.routing
    ? { routing: route.routing }
    : { connection_id: route.connection_id.trim(), upstream_model_id: route.upstream_model_id.trim(), routing: null };
}

const bytes = (value: string) => new TextEncoder().encode(value).length;
export function heuristicKeywordError(added: unknown, removed: unknown): string | null {
  if (!Array.isArray(added) || !Array.isArray(removed) || added.length + removed.length > 256) return "Use at most 256 keyword overrides";
  for (const word of [...added, ...removed]) if (typeof word !== "string" || !word.trim() || bytes(word) > 256) return "Keywords must be nonblank and at most 256 bytes";
  const removedSet = new Set(removed.map((word: string) => word.toLowerCase()));
  return added.some((word: string) => removedSet.has(word.toLowerCase())) ? "A keyword cannot be both added and removed" : null;
}

function object(value: unknown, path: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`${path} must be an object`);
  return value as Record<string, unknown>;
}
function fields(value: Record<string, unknown>, allowed: string[], path: string) {
  const unknown = Object.keys(value).find(key => !allowed.includes(key));
  if (unknown) throw new Error(`${path}: unknown field ${unknown}`);
}
function concrete(value: unknown, path: string): asserts value is string {
  if (typeof value !== "string") throw new Error(`${path}: choose a connection and model`);
  const target = decodeTarget(value);
  if (!target.connection_id || !target.upstream_model_id || target.connection_id === "chalk" || target.upstream_model_id.startsWith("chalk/") || value !== value.trim() || bytes(value) > 768) throw new Error(`${path}: use a concrete connection and model, not a route alias`);
}
function target(value: unknown, path: string) {
  const v = object(value, path);
  fields(v, ["connection_id", "upstream_model_id"], path);
  if (typeof v.connection_id !== "string" || typeof v.upstream_model_id !== "string" || v.connection_id.includes("/") || v.connection_id !== v.connection_id.trim() || v.upstream_model_id !== v.upstream_model_id.trim()) throw new Error(`${path}: choose a connection and model`);
  concrete(`${v.connection_id}/${v.upstream_model_id}`, path);
}
function chain(model: string, value: unknown, path: string) {
  concrete(model, path);
  if (!Array.isArray(value) || value.length > 16) throw new Error(`${path}: use at most 16 alternatives`);
  const seen = new Set([model]);
  for (const item of value) {
    concrete(item, path);
    if (seen.has(item)) throw new Error(`${path}: an attempt list cannot repeat a model`);
    seen.add(item);
  }
}
export function judgeBackendError(value: unknown): string | null {
  try {
    const backend = object(value, "Judge backend");
    if (backend.type === "heuristic") {
      fields(backend, ["type", "use_default_keywords", "added_hard_keywords", "removed_hard_keywords", "code_blocks_are_hard", "max_easy_input_bytes"], "Heuristic");
      for (const flag of ["use_default_keywords", "code_blocks_are_hard"]) if (backend[flag] !== undefined && typeof backend[flag] !== "boolean") throw new Error(`${flag} must be a boolean`);
      const cutoff = backend.max_easy_input_bytes;
      if (cutoff !== undefined && (!Number.isInteger(cutoff) || Number(cutoff) < 0 || Number(cutoff) > 4294967295)) throw new Error("Length cutoff must be a nonnegative whole number of bytes");
      const added = backend.added_hard_keywords ?? [], removed = backend.removed_hard_keywords ?? [];
      const keywordError = heuristicKeywordError(added, removed);
      if (keywordError) throw new Error(keywordError);

    } else if (backend.type === "completion" || backend.type === "decisions") {
      fields(backend, ["type", "target", "timeout_ms"], "Judge backend");
      target(backend.target, "Judge target");
      const timeout = backend.timeout_ms ?? 1000;
      if (!Number.isInteger(timeout) || Number(timeout) < 0 || Number(timeout) > 30000) throw new Error("Judge timeout must be 0–30000 ms (0 uses 1000 ms)");
    } else throw new Error("Choose a judge backend");
    return null;
  } catch (error) { return error instanceof Error ? error.message : String(error); }
}

/** Validate browser/BFF input before protobuf conversion; preserve absent vs empty fallbacks. */
export function routeGridError(value: unknown): string | null {
  try {
    const grid = object(value, "Routing");
    fields(grid, ["default_target", "fallbacks", "judge"], "Routing");
    target(grid.default_target, "Default target");
    const fallbacks = object(grid.fallbacks ?? {}, "Fallbacks");
    if (Object.keys(fallbacks).length > 128) throw new Error("Use at most 128 fallback entries");
    for (const [model, alternatives] of Object.entries(fallbacks)) chain(model, alternatives, "Route fallback");
    if (grid.judge == null) return null;
    const judge = object(grid.judge, "Judge");
    fields(judge, ["mode", "classification", "backend", "outcomes"], "Judge");
    if (!["off", "shadow", "live"].includes(judge.mode as string)) throw new Error("Choose a judge mode");
    if (!["task", "difficulty"].includes(judge.classification as string)) throw new Error("Choose a classification type");
    const backendIssue = judgeBackendError(judge.backend);
    if (backendIssue) throw new Error(backendIssue);
    const backend = object(judge.backend, "Judge backend");
    if (backend.type === "heuristic" && judge.classification !== "difficulty") throw new Error("Local heuristic requires difficulty classification");
    const outcomes = object(judge.outcomes, "Outcomes");
    const labels = Object.keys(outcomes);
    if (!labels.length || labels.length > 32) throw new Error("Configure 1–32 outcomes");
    if (judge.classification === "difficulty" && (labels.length < 2 || labels.some((_, i) => !Object.hasOwn(outcomes, String(i))))) throw new Error("Difficulty outcomes must be consecutive integers starting at 0, with at least two outcomes");
    if (backend.type === "heuristic" && labels.length !== 2) throw new Error("Local heuristic requires exactly two outcomes: 0 (easy) and 1 (hard)");
    for (const [label, raw] of Object.entries(outcomes)) {
      if (!label.trim() || label !== label.trim() || bytes(label) > 128) throw new Error("Outcome labels must be nonblank, trimmed and at most 128 bytes");
      const outcome = object(raw, `Outcome ${label}`);
      fields(outcome, ["model", "description", "fallbacks"], `Outcome ${label}`);
      concrete(outcome.model, `Outcome ${label}`);
      if (outcome.description !== undefined && (typeof outcome.description !== "string" || bytes(outcome.description) > 2048)) throw new Error(`Outcome ${label}: description must be at most 2048 bytes`);
      if (outcome.fallbacks != null) chain(outcome.model, outcome.fallbacks, `Outcome ${label} fallback`);
    }
    return null;
  } catch (error) { return (error as Error).message; }
}

const targetToProto = (target: RouteTarget) => ({ connectionId: target.connection_id, upstreamModelId: target.upstream_model_id });
const targetFromProto = (target: { connectionId: string; upstreamModelId: string } | undefined): RouteTarget => ({ connection_id: target?.connectionId ?? "", upstream_model_id: target?.upstreamModelId ?? "" });
export function judgeBackendToProto(b: JudgeBackend) {
  return create(RouteJudgeBackendSchema, { kind: b.type === "heuristic" ? { case: "heuristic", value: {
        useDefaultKeywords: b.use_default_keywords, addedHardKeywords: b.added_hard_keywords,
        removedHardKeywords: b.removed_hard_keywords, codeBlocksAreHard: b.code_blocks_are_hard,
        maxEasyInputBytes: b.max_easy_input_bytes,
      } } : { case: b.type, value: { target: targetToProto(b.target), timeoutMs: b.timeout_ms ?? 1000 } } });
}

export function routeGridToProto(grid: RouteGridConfig): RouteGrid {
  const j = grid.judge;
  const b = j?.backend;
  return create(RouteGridSchema, {
    defaultTarget: targetToProto(grid.default_target),
    fallbacks: Object.fromEntries(Object.entries(grid.fallbacks ?? {}).map(([key, models]) => [key, { models }])),
    judge: j && b ? {
      mode: { off: JudgeMode.OFF, shadow: JudgeMode.SHADOW, live: JudgeMode.LIVE }[j.mode],
      classification: j.classification === "difficulty" ? Classification.DIFFICULTY : Classification.TASK,
      backend: judgeBackendToProto(b),
      outcomes: Object.fromEntries(Object.entries(j.outcomes).map(([key, o]) => [key, { model: o.model, description: o.description, fallbacks: o.fallbacks == null ? undefined : { models: o.fallbacks } }])),
    } : undefined,
  });
}
export function routeGridFromProto(grid?: RouteGrid): RouteGridConfig | null {
  if (!grid) return null;
  const j = grid.judge, kind = j?.backend?.kind;
  let backend: JudgeBackend;
  if (kind?.case === "heuristic") {
    const b = kind.value;
    backend = { type: "heuristic", use_default_keywords: b.useDefaultKeywords, added_hard_keywords: b.addedHardKeywords, removed_hard_keywords: b.removedHardKeywords, code_blocks_are_hard: b.codeBlocksAreHard, max_easy_input_bytes: b.maxEasyInputBytes };
  } else if (kind?.case === "completion" || kind?.case === "decisions") {
    backend = { type: kind.case, target: targetFromProto(kind.value.target), timeout_ms: kind.value.timeoutMs };
  } else {
    // Never silently rewrite an unknown backend into a valid heuristic policy.
    if (j) throw new Error("This route uses an unsupported judge backend. Update the frontend before editing it.");
    backend = { type: "heuristic", added_hard_keywords: [], removed_hard_keywords: [] };
  }
  if (j && (![JudgeMode.UNSPECIFIED, JudgeMode.OFF, JudgeMode.SHADOW, JudgeMode.LIVE].includes(j.mode) || ![Classification.TASK, Classification.DIFFICULTY].includes(j.classification))) throw new Error("This route uses an unsupported judge mode or classification");
  return {
    default_target: targetFromProto(grid.defaultTarget),
    fallbacks: Object.fromEntries(Object.entries(grid.fallbacks).map(([key, value]) => [key, value.models])),
    judge: j ? { mode: j.mode === JudgeMode.LIVE ? "live" : j.mode === JudgeMode.SHADOW ? "shadow" : "off", classification: j.classification === Classification.DIFFICULTY ? "difficulty" : "task", backend,
      outcomes: Object.fromEntries(Object.entries(j.outcomes).map(([key, o]) => [key, { model: o.model, description: o.description, fallbacks: o.fallbacks?.models ?? null }])) } : null,
  };
}
