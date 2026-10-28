import { ConnectionDetailClient } from "./ConnectionDetailClient";

export default async function ConnectionDetailPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return <ConnectionDetailClient connectionId={id} />;
}
