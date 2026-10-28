import { NextRequest } from "next/server";
import { afterEach, expect, it, vi } from "vitest";
import { GET } from "./route";
const getRouteJudgeDefaults = vi.hoisted(() => vi.fn());
vi.mock("@/lib/grpc", () => ({ modelRouteClient: () => ({ getRouteJudgeDefaults }) }));
afterEach(() => vi.resetAllMocks());
it("forwards the backend's labels and descriptions to the editor", async () => {
  const defaults = { difficulty: [{ label: "0", description: "Backend criteria" }], task: [{ label: "coding", description: "Backend coding criteria" }] };
  getRouteJudgeDefaults.mockResolvedValue(defaults);
  expect(await (await GET(new NextRequest("http://localhost/api/admin/model-routes/judge-defaults"))).json()).toEqual(defaults);
});
it("reports failure rather than making up client-side defaults", async () => {
  getRouteJudgeDefaults.mockRejectedValue(new Error("Unavailable"));
  expect((await GET(new NextRequest("http://localhost/api/admin/model-routes/judge-defaults"))).status).toBe(502);
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
