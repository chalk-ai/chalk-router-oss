import type { ReactNode } from "react";

export type RouterIconName =
  | "sliders"
  | "route"
  | "share"
  | "scale"
  | "key"
  | "users"
  | "gauge"
  | "chart"
  | "message"
  | "vector"
  | "images"
  | "settings"
  | "edit"
  | "trash"
  | "activity"
  | "add";

export type RouterTabItem<T extends string = string> = {
  id: T;
  label: string;
  icon: RouterIconName;
  count?: number;
};

export type RoutingTab = "connections" | "routes" | "sampling";
export type AccessTab = "keys" | "pools" | "usage-budgets" | "rate-limits" | "usage";
export type PlaygroundTab = "chat" | "decisions" | "embeddings" | "images";
export type ConnectionDetailTab = "models" | "routes" | "settings";
export type RouteDetailTab = "keys" | "settings";
export type KeyDetailTab = "permissions" | "limits" | "usage" | "traces";

export const ROUTING_TABS = [
  { id: "connections", label: "Connections", icon: "sliders" },
  { id: "routes", label: "Routes", icon: "route" },
  { id: "sampling", label: "Sampling policy", icon: "activity" },
] satisfies readonly RouterTabItem<RoutingTab>[];

export const ACCESS_TABS = [
  { id: "keys", label: "API keys", icon: "key" },
  { id: "pools", label: "Usage pools", icon: "users" },
  { id: "usage-budgets", label: "Usage budgets", icon: "gauge" },
  { id: "rate-limits", label: "Rate limits", icon: "gauge" },
  { id: "usage", label: "Usage", icon: "chart" },
] satisfies readonly RouterTabItem<AccessTab>[];

export const PLAYGROUND_TABS = [
  { id: "chat", label: "Chat", icon: "message" },
  { id: "decisions", label: "Decisions", icon: "scale" },
  { id: "embeddings", label: "Embeddings", icon: "vector" },
  { id: "images", label: "Images", icon: "images" },
] satisfies readonly RouterTabItem<PlaygroundTab>[];

export const CONNECTION_DETAIL_TABS = [
  { id: "models", label: "Models", icon: "sliders" },
  { id: "routes", label: "Routes", icon: "route" },
  { id: "settings", label: "Settings", icon: "settings" },
] satisfies readonly RouterTabItem<ConnectionDetailTab>[];

export const ROUTE_DETAIL_TABS = [
  { id: "keys", label: "Keys with access", icon: "key" },
  { id: "settings", label: "Settings", icon: "settings" },
] satisfies readonly RouterTabItem<RouteDetailTab>[];

export const KEY_DETAIL_TABS = [
  { id: "permissions", label: "Permissions", icon: "key" },
  { id: "limits", label: "Limits", icon: "gauge" },
  { id: "usage", label: "Usage", icon: "chart" },
  { id: "traces", label: "Traces", icon: "route" },
] satisfies readonly RouterTabItem<KeyDetailTab>[];

export function isTab<T extends string>(
  items: readonly RouterTabItem<T>[],
  value: string | null | undefined,
): value is T {
  return value != null && items.some((item) => item.id === value);
}

export function RouterIcon({ name, size = 15 }: { name: RouterIconName; size?: number }) {
  const paths: Record<RouterIconName, ReactNode> = {
    sliders: <><path d="M4 6h16M4 18h16M8 3v6M16 15v6" /><circle cx="8" cy="12" r="2" /><circle cx="16" cy="12" r="2" /></>,
    route: <><circle cx="6" cy="6" r="2" /><circle cx="18" cy="12" r="2" /><circle cx="6" cy="18" r="2" /><path d="m8 7 8 4m-8 6 8-4" /></>,
    share: <><circle cx="6" cy="12" r="2" /><circle cx="18" cy="6" r="2" /><circle cx="18" cy="18" r="2" /><path d="m8 11 8-4m-8 6 8 4" /></>,
    scale: <><path d="M12 3v18M5 7h14M5 7l-3 6h6L5 7Zm14 0-3 6h6l-3-6Z" /><path d="M8 21h8" /></>,
    key: <><circle cx="8" cy="15" r="4" /><path d="m11 12 9-9m-4 4 2 2m-5 1 2 2" /></>,
    users: <><circle cx="9" cy="8" r="3" /><path d="M3 20c0-4 2-6 6-6s6 2 6 6M16 4a3 3 0 0 1 0 6m1 4c3 .4 4 2.4 4 6" /></>,
    gauge: <><path d="M4 18a8 8 0 1 1 16 0" /><path d="m12 18 4-7" /><path d="M7 18h10" /></>,
    chart: <><path d="M4 20V10m6 10V4m6 16v-7m4 7H2" /></>,
    message: <><path d="M4 5h16v11H8l-4 4V5Z" /></>,
    vector: <><rect x="4" y="4" width="16" height="16" rx="2" /><path d="m8 15 3-6 3 6 2-4" /></>,
    images: <><rect x="3" y="5" width="16" height="14" rx="2" /><path d="m6 16 4-4 3 3 2-2 4 4M17 3h4v14" /></>,
    settings: <><circle cx="12" cy="12" r="3" /><path d="M19 12a7 7 0 0 0-.1-1l2-1.5-2-3.4-2.5 1A7 7 0 0 0 15 6l-.4-2.7h-4L10 6a7 7 0 0 0-1.4 1.1l-2.5-1-2 3.4L6.1 11a7 7 0 0 0 0 2l-2 1.5 2 3.4 2.5-1A7 7 0 0 0 10 18l.5 2.7h4L15 18a7 7 0 0 0 1.4-1.1l2.5 1 2-3.4-2-1.5c.1-.3.1-.7.1-1Z" /></>,
    edit: <><path d="m4 20 4.5-1 10-10-3.5-3.5-10 10L4 20Z" /><path d="m13.5 7 3.5 3.5M14.5 5.5l2-2a2 2 0 0 1 3 3l-2 2" /></>,
    trash: <><path d="M4 7h16M9 7V4h6v3m3 0-1 14H7L6 7m4 4v6m4-6v6" /></>,
    activity: <path d="M3 12h4l2-6 4 12 2-6h6" />,
    add: <path d="M12 5v14M5 12h14" />,
  };
  return <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{paths[name]}</svg>;
}
