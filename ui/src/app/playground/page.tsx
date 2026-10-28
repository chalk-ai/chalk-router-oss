"use client";

import { Suspense, useEffect, useState } from "react";
import { useSearchParams } from "next/navigation";
import { PageHeader, UnderlineTabs } from "@/components/router/RouterPrimitives";
import { PLAYGROUND_TABS, isTab, type PlaygroundTab } from "@/components/router/router-tabs";
import ChatTab from "./ChatTab";
import DecisionsTab from "./DecisionsTab";
import EmbeddingsTab from "./EmbeddingsTab";
import ImagesTab from "./ImagesTab";
import type { Model } from "./playground-shared";

export default function PlaygroundPage() {
  return <Suspense><PlaygroundContent /></Suspense>;
}

function PlaygroundContent() {
  const searchParams = useSearchParams();
  const [models, setModels] = useState<Model[]>([]);
  const requested = searchParams.get("v") ?? searchParams.get("tab");
  const tab: PlaygroundTab = isTab(PLAYGROUND_TABS, requested) ? requested : "chat";

  useEffect(() => {
    fetch("/api/v1/models")
      .then((response) => response.json())
      .then((body) => setModels(body.data ?? []))
      .catch(() => {});
  }, []);

  return <div className="flex min-h-0 flex-col" style={{ height: "calc(100vh - 64px)" }}>
    <div className="mb-5"><PageHeader title="Playground" description="Test chat, structured decisions, embeddings, and image generation." /></div>
    <UnderlineTabs items={PLAYGROUND_TABS} active={tab} basePath="/playground" />
    <div className="mt-5 min-h-0 flex-1">
      {tab === "chat" && <ChatTab models={models} />}
      {tab === "decisions" && <DecisionsTab models={models} />}
      {tab === "embeddings" && <EmbeddingsTab models={models} />}
      {tab === "images" && <ImagesTab models={models} />}
    </div>
  </div>;
}
