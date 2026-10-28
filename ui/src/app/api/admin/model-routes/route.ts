import { withSession } from "@/lib/auth";
import { routeGridError, routeGridToProto, type RouteGridConfig } from "@/lib/routeGrid";
import { NextRequest, NextResponse } from "next/server";
import { modelRouteClient } from "@/lib/grpc";
import { adminPageRequest } from "@/lib/adminPagination";
import { modelRouteToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { parseJsonObjectBody } from "@/lib/bff";

const CHALK_PREFIX = "chalk/";

async function handleGET(req: NextRequest) {
  try {
    const connectionIds = req.nextUrl.searchParams.getAll("connection_id");
    const rawPublished = req.nextUrl.searchParams.get("published");
    if (rawPublished !== null && rawPublished !== "true" && rawPublished !== "false") {
      return NextResponse.json(
        { error: { message: "published must be true or false" } },
        { status: 400 },
      );
    }
    const page = await modelRouteClient().listModelRoutes({
      ...adminPageRequest(req),
      filters: {
        connectionIds,
        published: rawPublished === null ? undefined : rawPublished === "true",
      },
    });
    return NextResponse.json({
      data: page.routes.map(modelRouteToJson),
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
    if (body.routing != null) {
      const error = routeGridError(body.routing);
      if (error) return NextResponse.json({ error: { message: error } }, { status: 400 });
    }
    const routing = body.routing == null ? undefined : routeGridToProto(body.routing as RouteGridConfig);
    const publicModelId = String(body.public_model_id ?? body.publicModelId ?? "").trim();
    const connectionId = String(body.connection_id ?? body.connectionId ?? "").trim();
    const upstreamModelId = String(body.upstream_model_id ?? body.upstreamModelId ?? "").trim();
    if (!publicModelId.startsWith(CHALK_PREFIX)) {
      return NextResponse.json(
        { error: { message: "public_model_id must start with 'chalk/'" } },
        { status: 400 },
      );
    }
    if (!publicModelId.slice(CHALK_PREFIX.length).trim()) {
      return NextResponse.json(
        { error: { message: "public_model_id must not be empty after the 'chalk/' prefix" } },
        { status: 400 },
      );
    }
    if (!routing && !connectionId) {
      return NextResponse.json(
        { error: { message: "connection_id is required" } },
        { status: 400 },
      );
    }
    if (!routing && !upstreamModelId) {
      return NextResponse.json(
        { error: { message: "upstream_model_id is required" } },
        { status: 400 },
      );
    }
    if (body.published !== undefined && typeof body.published !== "boolean") {
      return NextResponse.json({ error: { message: "published must be a boolean" } }, { status: 400 });
    }
    const res = await modelRouteClient().createModelRoute({
      routing,
      publicModelId,
      connectionId,
      upstreamModelId,
      published: (body.published as boolean | undefined) ?? true,
    });
    if (!res.route) {
      return NextResponse.json(
        { error: { message: "Backend returned no route" } },
        { status: 500 },
      );
    }
    return NextResponse.json(modelRouteToJson(res.route), { status: 201 });
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const POST = withSession(handlePOST);
