import { encodeTarget, type RouteJudge, type RouteTarget } from "./routeGrid";

export type JudgeOutcomeDefault = { label: string; description: string };
export type RouteJudgeDefaults = Record<RouteJudge["classification"], JudgeOutcomeDefault[]>;

export async function loadRouteJudgeDefaults(): Promise<RouteJudgeDefaults> {
  const response = await fetch("/api/admin/model-routes/judge-defaults");
  if (!response.ok) throw new Error("Could not load default judge categories.");
  const defaults: RouteJudgeDefaults = await response.json();
  for (const classification of ["difficulty", "task"] as const) {
    if (!Array.isArray(defaults[classification]) || !defaults[classification].length ||
      defaults[classification].some(entry => typeof entry.label !== "string" || !entry.label || typeof entry.description !== "string" || !entry.description)) {
      throw new Error("The router did not return default judge categories.");
    }
  }
  return defaults;
}

export function defaultJudgeOutcomes(defaults: RouteJudgeDefaults, classification: RouteJudge["classification"], target: RouteTarget): RouteJudge["outcomes"] {
  return Object.fromEntries(defaults[classification].map(({ label, description }) => [label, {
    model: encodeTarget(target), description, fallbacks: null,
  }]));
}
