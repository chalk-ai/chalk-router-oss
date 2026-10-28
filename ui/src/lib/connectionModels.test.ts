import { afterEach, describe, expect, it, vi } from "vitest";
import { loadConnectionModels } from "./connectionModels";

afterEach(() => vi.unstubAllGlobals());

describe("connection model discovery", () => {
  it("shares simultaneous requests and uses upstream IDs rather than public aliases", async () => {
    const fetchMock = vi.fn().mockResolvedValue(Response.json({ data: [
      { upstream_model_id: "gpt-4o", public_model_id: "openai/gpt-4o" },
      { upstream_model_id: "gpt-4o" },
      { upstream_model_id: "gpt-4.1" },
    ] }));
    vi.stubGlobal("fetch", fetchMock);
    const first = loadConnectionModels("connection/a");
    const second = loadConnectionModels("connection/a");
    expect(first).toBe(second);
    expect(await first).toEqual(["gpt-4.1", "gpt-4o"]);
    expect(fetchMock).toHaveBeenCalledExactlyOnceWith("/api/admin/providers/connection%2Fa/models");
  });

  it("keeps connections separate and refreshes on later requests", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(Response.json({ data: [{ upstream_model_id: "gpt-4o" }] }))
      .mockResolvedValueOnce(Response.json({ data: [{ upstream_model_id: "claude-sonnet" }] }))
      .mockResolvedValueOnce(Response.json({ data: [{ upstream_model_id: "new-model" }] }));
    vi.stubGlobal("fetch", fetchMock);
    const results = await Promise.all([loadConnectionModels("openai"), loadConnectionModels("anthropic")]);
    expect(results).toEqual([["gpt-4o"], ["claude-sonnet"]]);
    expect(await loadConnectionModels("openai")).toEqual(["new-model"]);
  });

  it("allows retry after discovery fails, including empty model lists", async () => {
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(new Response("Unavailable", { status: 502 }))
      .mockResolvedValueOnce(Response.json({ data: [] })));
    await expect(loadConnectionModels("retry")).rejects.toThrow("Could not load models");
    await expect(loadConnectionModels("retry")).resolves.toEqual([]);
  });
});
