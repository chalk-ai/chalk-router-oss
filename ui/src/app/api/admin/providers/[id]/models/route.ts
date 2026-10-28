import { withSession } from "@/lib/auth";
import { NextRequest, NextResponse } from "next/server";
import { providerConnectionClient } from "@/lib/grpc";

async function handleGET(
  _req: NextRequest,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;
  try {
    const response = await providerConnectionClient().listProviderConnectionModels({
      filters: { connectionIds: [id] },
    });
    return NextResponse.json({
      data: response.models.map((model) => ({
        public_model_id: model.publicModelId,
        upstream_model_id: model.upstreamModelId,
        connection_id: model.connectionId,
        capabilities: model.capabilities,
      })),
    });
  } catch (error) {
    return NextResponse.json(
      { error: { message: error instanceof Error ? error.message : String(error) } },
      { status: 502 },
    );
  }
}

export const GET = withSession(handleGET);
