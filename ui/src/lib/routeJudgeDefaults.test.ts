import { afterEach, describe, expect, it, vi } from "vitest";
import { defaultJudgeOutcomes, loadRouteJudgeDefaults, type RouteJudgeDefaults } from "./routeJudgeDefaults";

const defaults: RouteJudgeDefaults = {
  difficulty: [{ label: "0", description: "Simple criteria from backend" }, { label: "1", description: "Complex criteria from backend" }],
  task: ["coding", "summarization", "extraction", "other"].map(label => ({ label, description: `${label} criteria from backend` })),
};
afterEach(() => vi.unstubAllGlobals());
describe("backend judge defaults", () => {
  it("seeds task buckets with backend descriptions and the current default target", () => {
    const outcomes = defaultJudgeOutcomes(defaults, "task", { connection_id: "c", upstream_model_id: "org/model" });
    expect(Object.keys(outcomes)).toEqual(["coding", "summarization", "extraction", "other"]);
    expect(outcomes.coding).toEqual({ model: "c/org/model", description: defaults.task[0].description, fallbacks: null });
    outcomes.coding.description = "Edited";
    expect(defaultJudgeOutcomes(defaults, "task", { connection_id: "c", upstream_model_id: "m" }).coding.description).toBe(defaults.task[0].description);
  });
  it("loads defaults without substituting frontend criteria", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json(defaults)));
    expect(await loadRouteJudgeDefaults()).toEqual(defaults);
  });
  it("rejects missing or unavailable defaults", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(Response.json({ difficulty: [], task: [] })).mockResolvedValueOnce(new Response(null, { status: 502 })));
    await expect(loadRouteJudgeDefaults()).rejects.toThrow("did not return");
    await expect(loadRouteJudgeDefaults()).rejects.toThrow("Could not load");
  });
});
