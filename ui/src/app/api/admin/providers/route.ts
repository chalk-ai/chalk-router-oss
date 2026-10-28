import { withSession } from "@/lib/auth";
import type { JsonObject } from "@bufbuild/protobuf";
import { NextRequest, NextResponse } from "next/server";
import { providerConnectionClient } from "@/lib/grpc";
import { adminPageRequest } from "@/lib/adminPagination";
import { exposureFromString, providerConnectionToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { parseJsonObjectBody } from "@/lib/bff";

async function handleGET(req: NextRequest) {
  try {
    const page =
      await providerConnectionClient().listProviderConnections(adminPageRequest(req));
    return NextResponse.json({
      data: page.connections.map(providerConnectionToJson),
      next_cursor: page.nextCursor,
    });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handlePOST(req: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const name = ((body.name as string | undefined) ?? "").trim();
    const providerKind = ((body.provider_kind ?? body.providerKind) as string | undefined ?? "").trim();
    if (!name) {
      return NextResponse.json(
        { error: { message: "name is required" } },
        { status: 400 },
      );
    }
    if (!providerKind) {
      return NextResponse.json(
        { error: { message: "provider_kind is required" } },
        { status: 400 },
      );
    }
    const baseUrl = ((body.base_url ?? body.baseUrl) as string | undefined ?? "").trim();
    if (providerKind === "openai-compatible" && !baseUrl) {
      return NextResponse.json(
        { error: { message: "base_url is required for openai-compatible" } },
        { status: 400 },
      );
    }
    const prefix = ((body.prefix as string | undefined) ?? "").trim();
    const apiKey = ((body.api_key ?? body.apiKey) as string | undefined ?? "").trim();
    // vLLM and arbitrary OpenAI-compatible endpoints may run keyless (the
    // backend substitutes an encrypted placeholder). Mirror the server rule so
    // other kinds fail fast with a clean 400.
    if (!apiKey && providerKind !== "vllm" && providerKind !== "openai-compatible") {
      return NextResponse.json(
        { error: { message: "api_key is required" } },
        { status: 400 },
      );
    }
    const declaredCapabilities = body.declared_capabilities ?? body.declaredCapabilities;
    if (
      declaredCapabilities != null &&
      (typeof declaredCapabilities !== "object" || Array.isArray(declaredCapabilities))
    ) {
      return NextResponse.json(
        { error: { message: "declared_capabilities must be an object" } },
        { status: 400 },
      );
    }
    const routingEnabledRaw = body.routing_enabled ?? body.routingEnabled;
    const res = await providerConnectionClient().createProviderConnection({
      name,
      providerKind,
      baseUrl: baseUrl || undefined,
      prefix: prefix || undefined,
      exposure: exposureFromString(body.exposure as string | undefined),
      routingEnabled: typeof routingEnabledRaw === "boolean" ? routingEnabledRaw : true,
      apiKey,
      // Validated above to be a non-null, non-array object; the request body
      // was already deserialized from JSON, so its shape satisfies JsonObject.
      declaredCapabilities: declaredCapabilities as JsonObject | undefined,
    });
    if (!res.connection) {
      return NextResponse.json(
        { error: { message: "Backend returned no connection" } },
        { status: 500 },
      );
    }
    return NextResponse.json(providerConnectionToJson(res.connection), {
      status: 201,
    });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const POST = withSession(handlePOST);
