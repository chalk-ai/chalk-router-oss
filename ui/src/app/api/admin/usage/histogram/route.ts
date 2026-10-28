import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import type { Timestamp } from "@bufbuild/protobuf/wkt";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { apiKeyClient } from "@/lib/grpc";
import { connectErrorResponse } from "@/lib/connectError";

function parseTs(raw: string | null): Timestamp | undefined {
  if (!raw) return undefined;
  const d = new Date(raw);
  if (isNaN(d.getTime())) return undefined;
  return timestampFromDate(d);
}

function isoFromTimestamp(ts: Timestamp | undefined): string {
  if (!ts) return "";
  return new Date(Number(ts.seconds) * 1000).toISOString();
}

async function handleGET(req: NextRequest) {
  const sp = req.nextUrl.searchParams;
  try {
    const res = await apiKeyClient().getUsageHistogram({
      bucketSize: sp.get("bucket_size") ?? "",
      start: parseTs(sp.get("start")),
      end: parseTs(sp.get("end")),
      apiToken: sp.get("api_token") ?? undefined,
    });
    return NextResponse.json({
      data: res.rows.map((r) => ({
        connection_id: r.connectionId || null,
        bucket: isoFromTimestamp(r.bucket),
        prompt_tokens: Number(r.promptTokens),
        completion_tokens: Number(r.completionTokens),
        cached_tokens: Number(r.cachedTokens),
        cache_write_tokens: Number(r.cacheWriteTokens),
        reasoning_tokens: Number(r.reasoningTokens),
        total_tokens: Number(r.totalTokens),
        request_count: Number(r.requestCount),
      })),
    });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
