import { RouteDetailClient } from "./RouteDetailClient";

export default async function RouteDetailPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return <RouteDetailClient routeId={id} />;
}
