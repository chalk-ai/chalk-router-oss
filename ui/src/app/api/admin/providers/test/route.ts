import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { providerConnectionClient } from "@/lib/grpc";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, invalidField, parseJsonObjectBody, type BffResult, type JsonObject } from "@/lib/bff";
import type { MessageInitShape } from "@bufbuild/protobuf";
import type { TestProviderConnectionRequestSchema } from "@/gen/chalk/router/v1/provider_connection_pb";
import { exposureFromString } from "@/lib/serialize";

/** Convert a protobuf Duration to whole milliseconds for the JSON admin API. */
function durationToMillis(
  d: { seconds: bigint; nanos: number } | undefined,
): number | null {
  if (!d) return null;
  return Number(d.seconds) * 1000 + Math.trunc(d.nanos / 1_000_000);
}

function testResultToJson(res: {
  ok: boolean;
  error?: string;
  discoveredModels?: number;
  latency?: { seconds: bigint; nanos: number };
}) {
  return {
    ok: res.ok,
    error: res.error ?? null,
    discovered_models:
      res.discoveredModels != null ? Number(res.discoveredModels) : null,
    latency_ms: durationToMillis(res.latency),
  };
}

/** A connection ID plus a complete draft tests edits without saving them. */
function providerConnectionTestFromJson(
  body: JsonObject,
): BffResult<MessageInitShape<typeof TestProviderConnectionRequestSchema>> {
  const rawId = body.connection_id ?? body.connectionId;
  if (rawId != null && (typeof rawId !== "string" || !rawId.trim())) {
    return invalidField("connection_id", "must be a nonempty string");
  }
  const connectionId = typeof rawId === "string" ? rawId.trim() : undefined;
  if (!hasOwn(body, "draft")) {
    return connectionId
      ? { value: { target: { case: "connectionId", value: connectionId } } }
      : invalidField("draft", "is required without a connection_id");
  }
  if (body.draft === null || typeof body.draft !== "object" || Array.isArray(body.draft)) {
    return invalidField("draft", "must be an object");
  }
  const draft = body.draft as JsonObject;
  const strings = {
    providerKind: draft.provider_kind ?? draft.providerKind,
    baseUrl: draft.base_url ?? draft.baseUrl,
    prefix: draft.prefix,
    apiKey: draft.api_key ?? draft.apiKey,
  };
  for (const [field, value] of Object.entries(strings)) {
    if (value != null && typeof value !== "string") {
      return invalidField(`draft.${field}`, "must be a string");
    }
  }
  const providerKind = (strings.providerKind as string | undefined)?.trim();
  if (!providerKind) return invalidField("draft.provider_kind", "is required");
  if (draft.exposure != null && (typeof draft.exposure !== "string" || !["dynamic", "unlisted", "explicit", "unspecified"].includes(draft.exposure))) {
    return invalidField("draft.exposure", "must be a supported exposure policy");
  }
  const value = {
    providerKind,
    baseUrl: (strings.baseUrl as string | undefined)?.trim() || undefined,
    prefix: (strings.prefix as string | undefined)?.trim() || undefined,
    exposure: exposureFromString(draft.exposure as string | undefined),
    apiKey: (strings.apiKey as string | undefined)?.trim() ?? "",
  };
  return {
    value: {
      target: connectionId
        ? { case: "savedDraft", value: { connectionId, draft: value } }
        : { case: "draft", value },
    },
  };
}

async function handlePOST(req: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const request = providerConnectionTestFromJson(parsed.body);
    if ("error" in request) return request.error;
    const res = await providerConnectionClient().testProviderConnection(request.value);
    return NextResponse.json(testResultToJson(res));
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const POST = withSession(handlePOST);
