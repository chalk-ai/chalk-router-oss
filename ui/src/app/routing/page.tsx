import { redirect } from "next/navigation";
import { RouterPageShell } from "@/components/RouterPageShell";
import ProvidersPage from "../providers/ProvidersClient";
import RoutesPage from "../routes/RoutesClient";
import { SamplingContent } from "../sampling/SamplingContent";
import { ROUTING_TABS, isTab } from "@/components/router/router-tabs";

export const dynamic = "force-dynamic";

export default async function RoutingPage({
  searchParams,
}: {
  searchParams: Promise<{ v?: string | string[] }>;
}) {
  const query = await searchParams;
  const requested = Array.isArray(query.v) ? query.v[0] : query.v;
  if (requested === "fallback" || requested === "judge") redirect("/routing?v=routes");
  const tab = isTab(ROUTING_TABS, requested) ? requested : "connections";
  let content: React.ReactNode;
  switch (tab) {
    case "routes":
      content = <RoutesPage embedded />;
      break;
    case "sampling":
      content = await SamplingContent({ embedded: true });
      break;
    default:
      content = <ProvidersPage embedded />;
  }
  return <RouterPageShell basePath="/routing" currentTab={tab} tabs={ROUTING_TABS}>{content}</RouterPageShell>;
}
