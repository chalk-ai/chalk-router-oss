import { KeyDetailClient } from "./KeyDetailClient";

export default async function KeyDetailPage({ params }: { params: Promise<{ hash: string }> }) {
  const { hash } = await params;
  return <KeyDetailClient keyHash={hash} />;
}
