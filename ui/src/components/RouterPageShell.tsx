import { UnderlineTabs } from "@/components/router/RouterPrimitives";
import type { RouterTabItem } from "@/components/router/router-tabs";

export function RouterPageShell({
  basePath,
  currentTab,
  tabs,
  children,
}: {
  basePath: "/routing" | "/access";
  currentTab: string;
  tabs: readonly RouterTabItem[];
  children: React.ReactNode;
}) {
  return (
    <div className="space-y-4">
      <UnderlineTabs items={tabs} active={currentTab} basePath={basePath} />
      {children}
    </div>
  );
}
