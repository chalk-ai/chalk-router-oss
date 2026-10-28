import { afterEach, expect, it, vi } from "vitest";
import { validateJudgeBackend } from "./judgeValidation";
const backend = { type: "decisions" as const, target: { connection_id: "c", upstream_model_id: "m" }, timeout_ms: 1000 };
afterEach(() => vi.unstubAllGlobals());
it("reports the backend's compatibility error and forwards cancellation", async () => {
  const fetcher = vi.fn().mockResolvedValue(Response.json({ error: { message: "Does not support Decisions" } }, { status: 400 }));
  vi.stubGlobal("fetch", fetcher);
  const signal = new AbortController().signal;
  await expect(validateJudgeBackend(backend, signal)).rejects.toThrow("Does not support Decisions");
  expect(fetcher).toHaveBeenCalledWith("/api/admin/model-routes/validate-judge", expect.objectContaining({ signal, body: JSON.stringify({ backend }) }));
});
it("requires an explicit successful validation response", async () => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(Response.json({ valid: true })).mockResolvedValueOnce(Response.json({})));
  await expect(validateJudgeBackend(backend, new AbortController().signal)).resolves.toBeUndefined();
  await expect(validateJudgeBackend(backend, new AbortController().signal)).rejects.toThrow("Could not check");
});
