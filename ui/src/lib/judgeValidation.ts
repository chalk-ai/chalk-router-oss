import type { JudgeBackend } from "./routeGrid";

export async function validateJudgeBackend(backend: JudgeBackend, signal: AbortSignal): Promise<void> {
  const response = await fetch("/api/admin/model-routes/validate-judge", {
    method: "POST", headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ backend }), signal,
  });
  const body = await response.json();
  if (!response.ok) throw new Error(body?.error?.message ?? "Could not check judge compatibility. Try again.");
  if (body?.valid !== true) throw new Error("Could not check judge compatibility. Try again.");
}
