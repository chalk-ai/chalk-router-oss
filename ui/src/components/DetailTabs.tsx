"use client";

import type { ReactNode } from "react";
import { DetailLayout } from "@/components/router/RouterPrimitives";
import { CONNECTION_DETAIL_TABS, KEY_DETAIL_TABS, ROUTE_DETAIL_TABS, type RouterTabItem } from "@/components/router/router-tabs";

const DETAIL_ITEMS: readonly RouterTabItem[] = [...CONNECTION_DETAIL_TABS, ...ROUTE_DETAIL_TABS, ...KEY_DETAIL_TABS];

export function DetailTabs({ tabs, sidebar }: { tabs: { id: string; label: string; content: ReactNode }[]; sidebar?: ReactNode }) {
  const items = tabs.map((tab) => ({ id: tab.id, label: tab.label, icon: DETAIL_ITEMS.find((item) => item.id === tab.id)?.icon ?? "settings" })) satisfies RouterTabItem[];
  const defaultTab = tabs[0]?.id ?? "";
  return <DetailLayout tabs={items} defaultTab={defaultTab} sidebar={sidebar ?? <p className="text-xs text-cds-ink-subtle">No metadata available.</p>}>
    {(active) => tabs.find((tab) => tab.id === active)?.content ?? tabs[0]?.content}
  </DetailLayout>;
}
