import { withSession } from "@/lib/auth";
import { NextResponse } from "next/server";
import { modelRouteClient } from "@/lib/grpc";

async function handleGET() {
  try {
    const defaults = await modelRouteClient().getRouteJudgeDefaults({});
    const outcomes = (entries: typeof defaults.task) => entries.map(({ label, description }) => ({ label, description }));
    return NextResponse.json({ difficulty: outcomes(defaults.difficulty), task: outcomes(defaults.task) });
  } catch {
    return NextResponse.json({ error: { message: "Could not load default judge categories." } }, { status: 502 });
  }
}

export const GET = withSession(handleGET);
