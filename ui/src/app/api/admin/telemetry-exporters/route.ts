import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { telemetrySettingsClient } from "@/lib/grpc";
import { connectErrorResponse } from "@/lib/connectError";
import { telemetryExporterToJson } from "@/lib/serialize";
import { hasOwn, parseJsonObjectBody } from "@/lib/bff";

async function handleGET() {
  try {
    const response = await telemetrySettingsClient().listTelemetryExporters({});
    return NextResponse.json({
      exporters: response.exporters.map(telemetryExporterToJson),
    });
  } catch (error) {
    return connectErrorResponse(error);
  }
}

async function handlePUT(request: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(request);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const config = (body.config as Record<string, unknown> | undefined) ?? {};
    const apiKey = hasOwn(body, "api_key")
      ? String(body.api_key ?? "").trim() || undefined
      : undefined;
    const response = await telemetrySettingsClient().upsertTelemetryExporter({
      exporter: {
        id: String(body.id ?? ""),
        name: String(body.name ?? ""),
        exporterType: String(body.exporter_type ?? ""),
        configJson: JSON.stringify({ ...config, type: body.exporter_type }),
        enabled: !!body.enabled,
        apiKey,
      },
    });
    return NextResponse.json(
      response.exporter ? telemetryExporterToJson(response.exporter) : null,
    );
  } catch (error) {
    return connectErrorResponse(error);
  }
}

async function handleDELETE(request: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(request);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const response = await telemetrySettingsClient().deleteTelemetryExporter({
      id: String(body.id ?? ""),
    });
    return NextResponse.json({ id: response.id });
  } catch (error) {
    return connectErrorResponse(error);
  }
}

export const GET = withSession(handleGET);
export const PUT = withSession(handlePUT);
export const DELETE = withSession(handleDELETE);
