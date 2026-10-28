import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { providerConnectionClient } from "@/lib/grpc";
import { exposureFromString, providerConnectionToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, parseJsonObjectBody } from "@/lib/bff";
import type { JsonObject } from "@bufbuild/protobuf";

async function handlePATCH(
  req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const update: {
      name?: string;
      baseUrl?: string;
      prefix?: string;
      exposure?: ReturnType<typeof exposureFromString>;
      routingEnabled?: boolean;
      apiKey?: string;
      declaredCapabilities?: JsonObject;
    } = {};
    const paths: string[] = [];

    if (hasOwn(body, "name")) {
      if (typeof body.name !== "string") {
        return NextResponse.json(
          { error: { message: "name must be a string" } },
          { status: 400 },
        );
      }
      paths.push("name");
      update.name = body.name.trim();
    }
    if (hasOwn(body, "clear_base_url") && typeof body.clear_base_url !== "boolean") {
      return NextResponse.json(
        { error: { message: "clear_base_url must be a boolean" } },
        { status: 400 },
      );
    }
    if (body.clear_base_url === true) {
      paths.push("base_url");
    } else if (hasOwn(body, "base_url")) {
      if (typeof body.base_url !== "string") {
        return NextResponse.json(
          { error: { message: "base_url must be a string" } },
          { status: 400 },
        );
      }
      paths.push("base_url");
      const trimmed = body.base_url.trim();
      if (trimmed) update.baseUrl = trimmed;
    }
    if (hasOwn(body, "clear_prefix") && typeof body.clear_prefix !== "boolean") {
      return NextResponse.json(
        { error: { message: "clear_prefix must be a boolean" } },
        { status: 400 },
      );
    }
    if (body.clear_prefix === true) {
      paths.push("prefix");
    } else if (hasOwn(body, "prefix")) {
      if (typeof body.prefix !== "string") {
        return NextResponse.json(
          { error: { message: "prefix must be a string" } },
          { status: 400 },
        );
      }
      paths.push("prefix");
      const trimmed = body.prefix.trim();
      if (trimmed) update.prefix = trimmed;
    }
    if (body.exposure != null) {
      if (
        typeof body.exposure !== "string" ||
        !["dynamic", "unlisted", "explicit"].includes(body.exposure)
      ) {
        return NextResponse.json(
          {
            error: {
              message: "exposure must be one of: dynamic, unlisted, explicit",
            },
          },
          { status: 400 },
        );
      }
      paths.push("exposure");
      update.exposure = exposureFromString(body.exposure);
    }
    if (hasOwn(body, "routing_enabled")) {
      if (typeof body.routing_enabled !== "boolean") {
        return NextResponse.json(
          { error: { message: "routing_enabled must be a boolean" } },
          { status: 400 },
        );
      }
      paths.push("routing_enabled");
      update.routingEnabled = body.routing_enabled;
    }
    if (hasOwn(body, "api_key")) {
      if (typeof body.api_key !== "string") {
        return NextResponse.json(
          { error: { message: "api_key must be a string" } },
          { status: 400 },
        );
      }
      // Pushed even if blank so the backend's non-empty validation (rather
      // than a silent no-op) is what rejects an explicitly-empty key.
      paths.push("api_key");
      const trimmed = body.api_key.trim();
      if (trimmed) update.apiKey = trimmed;
    }
    if (hasOwn(body, "declared_capabilities")) {
      if (
        body.declared_capabilities !== null &&
        (typeof body.declared_capabilities !== "object" ||
          Array.isArray(body.declared_capabilities))
      ) {
        return NextResponse.json(
          { error: { message: "declared_capabilities must be an object or null" } },
          { status: 400 },
        );
      }
      paths.push("declared_capabilities");
      if (body.declared_capabilities !== null) {
        update.declaredCapabilities = body.declared_capabilities as JsonObject;
      }
    }

    if (paths.length === 0) {
      return NextResponse.json(
        { error: { message: "no updatable fields in request" } },
        { status: 400 },
      );
    }

    const res = await providerConnectionClient().updateProviderConnection({
      id,
      update,
      updateMask: { paths },
    });
    if (!res.connection) {
      return NextResponse.json(
        { error: { message: "Backend returned no connection" } },
        { status: 500 },
      );
    }
    return NextResponse.json(providerConnectionToJson(res.connection));
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handleDELETE(
  _req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const res = await providerConnectionClient().deleteProviderConnection({ id });
    return NextResponse.json({
      id: res.id,
      warnings: res.warnings,
    });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const PATCH = withSession(handlePATCH);
export const DELETE = withSession(handleDELETE);
