import { describe, expect, it } from "vitest";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { ModelRouteSchema, RouteGridSchema } from "@/gen/chalk/router/v1/model_route_pb";
import { modelRouteToJson } from "./serialize";
import { decodeTarget, routeReferencedTargets, routeConfigurationPayload, routeGridError, routeGridFromProto, routeGridToProto, type RouteGridConfig } from "./routeGrid";

export function exampleGrid(): RouteGridConfig {
  return {
    default_target: { connection_id: "openai-id", upstream_model_id: "astra" },
    fallbacks: { "openai-id/gpt-4o": ["openai-id/gpt-5.5", "anthropic-id/opus"] },
    judge: { mode: "live", classification: "difficulty", backend: { type: "heuristic", added_hard_keywords: ["complex"], removed_hard_keywords: ["prove "], use_default_keywords: false, code_blocks_are_hard: false, max_easy_input_bytes: 0 }, outcomes: {
      "0": { model: "openai-id/gpt-4o", description: "Easy", fallbacks: null },
      "1": { model: "openai-id/astra", description: "Hard", fallbacks: [] },
    } },
  };
}
function roundTrip(grid: RouteGridConfig) {
  return routeGridFromProto(fromBinary(RouteGridSchema, toBinary(RouteGridSchema, routeGridToProto(grid))));
}
describe("route configuration transport", () => {
  it("preserves heuristic false/zero overrides, keyword whitespace and absent vs empty outcome chains on the wire", () => {
    const grid = exampleGrid();
    expect(routeGridError(grid)).toBeNull();
    expect(roundTrip(grid)).toEqual(grid);
  });
  it.each(["completion", "decisions"] as const)("round-trips %s targets, slash-containing upstream IDs and zero timeout", type => {
    const grid = exampleGrid();
    grid.judge!.backend = { type, target: { connection_id: "connection", upstream_model_id: "org/model" }, timeout_ms: 0 };
    grid.judge!.outcomes["0"].fallbacks = ["connection/org/model"];
    expect(roundTrip(grid)).toEqual(grid);
    expect(decodeTarget("connection/org/model")).toEqual({ connection_id: "connection", upstream_model_id: "org/model" });
  });
  it("includes routing when listing a model route and represents simple routes with null", () => {
    expect(modelRouteToJson(create(ModelRouteSchema)).routing).toBeNull();
    expect(modelRouteToJson(create(ModelRouteSchema, { routing: routeGridToProto(exampleGrid()) })).routing).toEqual(exampleGrid());
  });
  it("indexes custom targets for usage filters without retaining an inactive simple target", () => {
    const grid = exampleGrid();
    grid.judge!.backend = { type: "completion", target: { connection_id: "judge-id", upstream_model_id: "classifier" }, timeout_ms: 1000 };
    expect(routeReferencedTargets({ connection_id: "old", upstream_model_id: "unused", routing: grid })).toEqual([
      { connection_id: "openai-id", upstream_model_id: "astra" },
      { connection_id: "openai-id", upstream_model_id: "gpt-4o" },
      { connection_id: "openai-id", upstream_model_id: "gpt-5.5" },
      { connection_id: "anthropic-id", upstream_model_id: "opus" },
      { connection_id: "judge-id", upstream_model_id: "classifier" },
    ]);
  });
  it("explicitly clears custom routing when saving a simple route", () => {
    expect(routeConfigurationPayload({ connection_id: "a", upstream_model_id: " m ", routing: null })).toEqual({ connection_id: "a", upstream_model_id: "m", routing: null });
    const grid = exampleGrid();
    expect(routeConfigurationPayload({ connection_id: "", upstream_model_id: "", routing: grid })).toEqual({ routing: grid });
  });
});
describe("route configuration validation", () => {
  it.each([{}, { default_target: null }, { default_target: { connection_id: "a", upstream_model_id: "" } }, { default_target: { connection_id: "chalk", upstream_model_id: "other" } }])("requires a concrete default: %j", grid => expect(routeGridError(grid)).not.toBeNull());
  it("allows a custom target without a judge or fallbacks", () => expect(routeGridError({ default_target: exampleGrid().default_target })).toBeNull());
  it("rejects duplicate attempts, recursion through aliases, and overly long chains", () => {
    for (const alternatives of [["openai-id/gpt-4o"], ["a/b", "a/b"], ["chalk/other"], Array.from({ length: 17 }, (_, i) => `a/model${i}`)]) {
      const grid = exampleGrid(); grid.fallbacks["openai-id/gpt-4o"] = alternatives;
      expect(routeGridError(grid)).not.toBeNull();
    }
  });
  it("validates heuristic keyword overlap, cutoff and classification", () => {
    const grid = exampleGrid();
    grid.judge!.backend = { type: "heuristic", added_hard_keywords: ["PROOF"], removed_hard_keywords: ["proof"] };
    expect(routeGridError(grid)).toMatch(/both added and removed/);
    grid.judge!.backend = { type: "heuristic", added_hard_keywords: [], removed_hard_keywords: [], max_easy_input_bytes: -1 };
    expect(routeGridError(grid)).toMatch(/cutoff/);
    grid.judge!.backend.max_easy_input_bytes = 6000;
    grid.judge!.classification = "task";
    expect(routeGridError(grid)).toMatch(/difficulty/);
  });
  it("checks sparse difficulty outcomes and UTF-8 label byte limits", () => {
    const grid = exampleGrid(); grid.judge!.outcomes["2"] = grid.judge!.outcomes["1"]; delete grid.judge!.outcomes["1"];
    expect(routeGridError(grid)).toMatch(/consecutive/);
    grid.judge!.backend = { type: "completion", target: grid.default_target, timeout_ms: 1000 };
    grid.judge!.classification = "task";
    grid.judge!.outcomes = { ["é".repeat(65)]: { model: "a/b", description: "" } };
    expect(routeGridError(grid)).toMatch(/128 bytes/);
  });
  it("rejects malformed or unknown input instead of dropping configuration", () => {
    for (const patch of [{ fallbacks: [] }, { defaultTarget: {} }, { judge: { mode: "unspecified" } }]) expect(routeGridError({ ...exampleGrid(), ...patch })).not.toBeNull();
  });
  it.each([0, 1000, 30000])("accepts remote timeout %s", timeout_ms => {
    const grid = exampleGrid(); grid.judge!.backend = { type: "completion", target: grid.default_target, timeout_ms };
    expect(routeGridError(grid)).toBeNull();
  });
  it.each([-1, 30001, 1.5, Number.NaN])("rejects invalid timeout %s", timeout_ms => {
    const grid = exampleGrid(); grid.judge!.backend = { type: "decisions", target: grid.default_target, timeout_ms };
    expect(routeGridError(grid)).toMatch(/timeout/);
  });
});
