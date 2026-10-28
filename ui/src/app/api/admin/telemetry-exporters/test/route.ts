import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { telemetrySettingsClient } from "@/lib/grpc";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, parseJsonObjectBody } from "@/lib/bff";

function durationToMillis(
  duration: { seconds: bigint; nanos: number } | undefined,
): number | null {
  if (!duration) return null;
  return Number(duration.seconds) * 1000 + Math.trunc(duration.nanos / 1_000_000);
}

async function handlePOST(request: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(request);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    const config = (body.config as Record<string, unknown> | undefined) ?? {};
    const apiKey = hasOwn(body, "api_key")
      ? String(body.api_key ?? "").trim() || undefined
      : undefined;
    const response = await telemetrySettingsClient().testTelemetryExporter({
      exporter: {
        id: String(body.id ?? ""),
        name: String(body.name ?? ""),
        exporterType: String(body.exporter_type ?? ""),
        configJson: JSON.stringify({ ...config, type: body.exporter_type }),
        enabled: true,
        apiKey,
      },
    });
    return NextResponse.json({
      ok: response.ok,
      error: response.error ?? null,
      latency_ms: durationToMillis(response.latency),
    });
  } catch (error) {
    return connectErrorResponse(error);
  }
}

export const POST = withSession(handlePOST);
