import { beforeEach, describe, expect, it, vi } from "vitest";
import { NextRequest } from "next/server";
import type { MessageInitShape } from "@bufbuild/protobuf";
import type { TestProviderConnectionRequestSchema } from "@/gen/chalk/router/v1/provider_connection_pb";
import type { BffResult } from "@/lib/bff";
import { POST } from "./route";

const { testProviderConnection } = vi.hoisted(() => ({ testProviderConnection: vi.fn() }));
vi.mock("@/lib/grpc", () => ({
  providerConnectionClient: () => ({ testProviderConnection }),
}));

beforeEach(() => {
  testProviderConnection.mockReset().mockResolvedValue({ ok: true });
});

async function parseTestRequest(
  body: unknown,
): Promise<BffResult<MessageInitShape<typeof TestProviderConnectionRequestSchema>>> {
  const response = await POST(new NextRequest("http://localhost/api/admin/providers/test", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  }));
  if (response.status === 400) {
    expect(testProviderConnection).not.toHaveBeenCalled();
    return { error: response };
  }
  expect(response.status).toBe(200);
  expect(testProviderConnection).toHaveBeenCalledTimes(1);
  return { value: testProviderConnection.mock.calls[0][0] as MessageInitShape<typeof TestProviderConnectionRequestSchema> };
}

describe("POST /api/admin/providers/test", () => {
  const draft = { provider_kind: "anthropic", prefix: "anthropic/", exposure: "dynamic" };

  it("keeps ID-only requests on the existing target", async () => {
    expect(await parseTestRequest({ connection_id: " saved-id " })).toEqual({
      value: { target: { case: "connectionId", value: "saved-id" } },
    });
  });

  it("combines a saved ID with the draft instead of ignoring the draft", async () => {
    const result = await parseTestRequest({ connection_id: "saved-id", draft });
    expect(result).toMatchObject({ value: { target: {
      case: "savedDraft", value: { connectionId: "saved-id", draft: {
        providerKind: "anthropic", prefix: "anthropic/", apiKey: "",
      } },
    } } });
  });

  it("preserves cleared optional settings and a blank key after JSON serialization", async () => {
    const body = JSON.parse(JSON.stringify({
      connection_id: "saved-id",
      draft: { ...draft, base_url: undefined, prefix: undefined, exposure: "explicit", api_key: "" },
    }));
    const result = await parseTestRequest(body);
    expect("value" in result && result.value.target?.case).toBe("savedDraft");
    if ("value" in result && result.value.target?.case === "savedDraft") {
      expect(result.value.target.value.draft?.baseUrl).toBeUndefined();
      expect(result.value.target.value.draft?.prefix).toBeUndefined();
      expect(result.value.target.value.draft?.apiKey).toBe("");
    }
  });

  it("keeps new connections as standalone drafts", async () => {
    const api_key = "sk-new";
    const body = JSON.parse(JSON.stringify({ connection_id: undefined, draft: { ...draft, api_key } }));
    expect(await parseTestRequest(body)).toMatchObject({ value: { target: {
      case: "draft", value: { providerKind: "anthropic", apiKey: api_key },
    } } });
  });

  it("rejects a malformed draft even when a saved ID is supplied", async () => {
    const result = await parseTestRequest({ connection_id: "saved-id", draft: null });
    expect("error" in result && result.error.status).toBe(400);
  });
});

// Payload/translation unit tests isolate the handler; auth.test.ts tests every route boundary.
vi.mock("@/lib/auth", () => ({ withSession: (handler: unknown) => handler }));
