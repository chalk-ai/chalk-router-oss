import { withSession } from "@/lib/auth";
import { routeGridError, routeGridToProto, type RouteGridConfig } from "@/lib/routeGrid";
import { NextRequest, NextResponse } from "next/server";
import { modelRouteClient } from "@/lib/grpc";
import { modelRouteToJson } from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { hasOwn, parseJsonObjectBody } from "@/lib/bff";

async function handlePATCH(
  req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    if (body.routing != null) {
      const error = routeGridError(body.routing);
      if (error) return NextResponse.json({ error: { message: error } }, { status: 400 });
    }
    const routing = body.routing == null ? undefined : routeGridToProto(body.routing as RouteGridConfig);
    const update: {
      connectionId?: string;
      upstreamModelId?: string;
      published?: boolean;
      routing?: ReturnType<typeof routeGridToProto>;
    } = {};
    const paths: string[] = [];

    if (hasOwn(body, "connection_id")) {
      if (typeof body.connection_id !== "string") {
        return NextResponse.json(
          { error: { message: "connection_id must be a string" } },
          { status: 400 },
        );
      }
      const connectionId = body.connection_id.trim();
      if (!connectionId) {
        return NextResponse.json(
          { error: { message: "connection_id must not be empty" } },
          { status: 400 },
        );
      }
      paths.push("connection_id");
      update.connectionId = connectionId;
    }
    if (hasOwn(body, "upstream_model_id")) {
      if (typeof body.upstream_model_id !== "string") {
        return NextResponse.json(
          { error: { message: "upstream_model_id must be a string" } },
          { status: 400 },
        );
      }
      const upstreamModelId = body.upstream_model_id.trim();
      if (!upstreamModelId) {
        return NextResponse.json(
          { error: { message: "upstream_model_id must not be empty" } },
          { status: 400 },
        );
      }
      paths.push("upstream_model_id");
      update.upstreamModelId = upstreamModelId;
    }
    if (hasOwn(body, "published")) {
      if (typeof body.published !== "boolean") {
        return NextResponse.json(
          { error: { message: "published must be a boolean" } },
          { status: 400 },
        );
      }
      paths.push("published");
      update.published = body.published;
    }

    if (hasOwn(body, "routing")) {
      paths.push("routing");
      update.routing = routing; // Mask + omitted message explicitly clears custom routing.
    }

    if (paths.length === 0) {
      return NextResponse.json(
        { error: { message: "no updatable fields in request" } },
        { status: 400 },
      );
    }

    const res = await modelRouteClient().updateModelRoute({
      id,
      update,
      updateMask: { paths },
    });
    if (!res.route) {
      return NextResponse.json(
        { error: { message: "Backend returned no route" } },
        { status: 500 },
      );
    }
    return NextResponse.json(modelRouteToJson(res.route));
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
    const res = await modelRouteClient().deleteModelRoute({ id });
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
