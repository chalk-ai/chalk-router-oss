import { RouterPageShell } from "@/components/RouterPageShell";
import TokensPage from "../tokens/page";
import UsagePoolsPage from "../usage-pools/page";
import UsageBudgetsPage from "../usage-budgets/page";
import RateLimitsPage from "../rate-limits/page";
import UsagePage from "../usage/UsageContent";
import { ACCESS_TABS, isTab } from "@/components/router/router-tabs";

export const dynamic = "force-dynamic";

export default async function AccessPage({
  searchParams,
}: {
  searchParams: Promise<{ v?: string | string[]; usage_pool_id?: string | string[]; issue?: string | string[]; edit?: string | string[] }>;
}) {
  const query = await searchParams;
  const requested = Array.isArray(query.v) ? query.v[0] : query.v;
  const tab = isTab(ACCESS_TABS, requested) ? requested : "keys";
  let content: React.ReactNode;
  switch (tab) {
    case "pools":
      content = <UsagePoolsPage embedded />;
      break;
    case "usage-budgets":
      content = <UsageBudgetsPage embedded />;
      break;
    case "rate-limits":
      content = <RateLimitsPage embedded />;
      break;
    case "usage":
      content = await UsagePage({ embedded: true });
      break;
    default:
      content = await TokensPage({ searchParams: Promise.resolve({ usage_pool_id: query.usage_pool_id, issue: query.issue, edit: query.edit }), embedded: true });
  }
  return <RouterPageShell basePath="/access" currentTab={tab} tabs={ACCESS_TABS}>{content}</RouterPageShell>;
}
