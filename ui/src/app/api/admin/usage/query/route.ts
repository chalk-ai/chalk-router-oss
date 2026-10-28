import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import type { Timestamp } from "@bufbuild/protobuf/wkt";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import {
  type QueryUsageRequest,
  UsageDimension,
} from "@/gen/chalk/router/v1/usage_pb";
import { connectErrorResponse } from "@/lib/connectError";
import { usageClient } from "@/lib/grpc";

function parseTimestamp(raw: string | null): Timestamp | undefined {
  if (!raw) return undefined;
  const date = new Date(raw);
  return Number.isNaN(date.getTime()) ? undefined : timestampFromDate(date);
}

function parseGrouping(
  raw: string | null,
  apiKeyLabel: string | null,
): QueryUsageRequest["grouping"] {
  if (raw === null) return { case: undefined };
  if (raw === "global") return { case: "global", value: true };
  if (raw === "api_key_label") {
    return { case: "apiKeyLabel", value: apiKeyLabel ?? "" };
  }
  const dimension = {
    connection: UsageDimension.CONNECTION,
    model: UsageDimension.MODEL,
    api_key: UsageDimension.API_KEY,
    usage_pool: UsageDimension.USAGE_POOL,
  }[raw ?? ""] ?? UsageDimension.UNSPECIFIED;
  return { case: "dimension", value: dimension };
}

function parseLabels(values: string[]): Record<string, string> {
  return Object.fromEntries(
    values.map((value) => {
      const separator = value.indexOf("=");
      return separator > 0
        ? [value.slice(0, separator), value.slice(separator + 1)]
        : ["", ""];
    }),
  );
}

function timestampIso(timestamp: Timestamp | undefined): string {
  return timestamp ? new Date(Number(timestamp.seconds) * 1000).toISOString() : "";
}

async function handleGET(request: NextRequest) {
  const params = request.nextUrl.searchParams;
  try {
    const response = await usageClient().queryUsage({
      start: parseTimestamp(params.get("start")),
      end: parseTimestamp(params.get("end")),
      bucketSize: params.get("bucket_size") ?? "1h",
      grouping: parseGrouping(
        params.get("group_by"),
        params.get("group_by_api_key_label"),
      ),
      filters: {
        apiKeyHashes: params.getAll("api_key"),
        usagePoolIds: params.getAll("usage_pool"),
        connectionIds: params.getAll("connection"),
        models: params.getAll("model"),
        apiKeyLabels: parseLabels(params.getAll("api_key_label")),
      },
    });
    return NextResponse.json({
      data: response.rows.map((row) => ({
        bucket: timestampIso(row.bucket),
        dimension_value: row.dimensionValue,
        prompt_tokens: row.promptTokens.toString(),
        completion_tokens: row.completionTokens.toString(),
        cached_tokens: row.cachedTokens.toString(),
        cache_write_tokens: row.cacheWriteTokens.toString(),
        reasoning_tokens: row.reasoningTokens.toString(),
        total_tokens: row.totalTokens.toString(),
        request_count: row.requestCount.toString(),
      })),
    });
  } catch (error) {
    return connectErrorResponse(error);
  }
}

export const GET = withSession(handleGET);
