"use client";

import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { Fragment, Suspense, type ReactNode } from "react";
import {
  ACCESS_TABS,
  PLAYGROUND_TABS,
  ROUTING_TABS,
  RouterIcon,
  type RouterIconName,
  type RouterTabItem,
} from "@/components/router/router-tabs";

interface RailCategory {
  href: string;
  label: string;
  icon: RouterIconName;
  /** The category's `?v=` tabs, which become the breadcrumb after the category. */
  tabs: readonly RouterTabItem[];
  /** Standalone pages that belong to the category, keyed by first path segment, with their tab. */
  pages: Record<string, string>;
}

const RAIL_CATEGORIES: RailCategory[] = [
  {
    href: "/routing",
    label: "Routing",
    icon: "route",
    tabs: ROUTING_TABS,
    pages: {
      routing: "connections",
      providers: "connections",
      connections: "connections",
      routes: "routes",
      sampling: "sampling",
      fallback: "routes",
      judge: "routes",
    },
  },
  {
    href: "/access",
    label: "Access",
    icon: "key",
    tabs: ACCESS_TABS,
    pages: {
      access: "keys",
      tokens: "keys",
      keys: "keys",
      "usage-pools": "pools",
      "usage-budgets": "usage-budgets",
      "rate-limits": "rate-limits",
      usage: "usage",
    },
  },
  { href: "/telemetry", label: "Telemetry", icon: "activity", tabs: [], pages: { telemetry: "" } },
  {
    href: "/playground",
    label: "Playground",
    icon: "message",
    tabs: PLAYGROUND_TABS,
    pages: { playground: "chat" },
  },
];

/** Routes whose page sizes itself to the pane instead of scrolling inside it. */
function fillsPane(pathname: string): boolean {
  return pathname.startsWith("/playground");
}

function categoryFor(pathname: string): RailCategory | undefined {
  const segment = pathname.split("/")[1] ?? "";
  return RAIL_CATEGORIES.find((category) => segment in category.pages);
}

function LogoSquare() {
  return (
    <svg viewBox="0 0 81.79 81.79" className="h-3.5 w-3.5" aria-hidden>
      <rect width="81.79" height="81.79" fill="currentColor" />
    </svg>
  );
}

interface Crumb {
  label: string;
  href?: string;
  mono?: boolean;
}

function shortId(id: string): string {
  return id.length > 16 ? `${id.slice(0, 12)}…` : id;
}

function breadcrumbsFor(pathname: string, view: string | null): Crumb[] {
  const category = categoryFor(pathname);
  if (!category) return [];
  const crumbs: Crumb[] = [{ label: category.label, href: category.href }];
  const [segment = "", id] = pathname.split("/").filter(Boolean);
  // The category page names its tab in `?v=`; a standalone page implies one.
  const isCategoryPage = `/${segment}` === category.href;
  const tabId = (isCategoryPage && category.tabs.some((tab) => tab.id === view) ? view : null) ?? category.pages[segment];
  const tab = category.tabs.find((item) => item.id === tabId);
  if (tab) crumbs.push({ label: tab.label, href: `${category.href}?v=${tab.id}` });
  if (id) crumbs.push({ label: shortId(decodeURIComponent(id)), mono: true });
  return crumbs;
}

/** BreadcrumbsV2: muted ancestors, the current page in strong ink, slash separators. */
function Breadcrumbs() {
  const pathname = usePathname();
  const view = useSearchParams().get("v");
  const crumbs = breadcrumbsFor(pathname, view);
  return (
    <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-2 text-cds-sm font-medium">
      <span className="text-cds-ink-secondary">Router</span>
      {crumbs.map((crumb, i) => {
        const current = i === crumbs.length - 1;
        const className = `truncate whitespace-nowrap ${crumb.mono ? "font-mono" : ""} ${
          current ? "text-cds-ink-strong" : "text-cds-ink-secondary hover:text-cds-ink-medium"
        }`;
        return (
          <Fragment key={i}>
            <span className="text-zinc-700">/</span>
            {crumb.href && !current ? (
              <Link href={crumb.href} className={className}>
                {crumb.label}
              </Link>
            ) : (
              <span className={className} aria-current={current ? "page" : undefined}>
                {crumb.label}
              </span>
            )}
          </Fragment>
        );
      })}
    </nav>
  );
}

function SignOutIcon() {
  return (
    <svg width={18} height={18} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4m7 14 5-5-5-5m5 5H9" />
    </svg>
  );
}

/** Ends the admin session, from the rail's foot where the dashboard keeps account actions. */
function SignOutButton() {
  return (
    <button
      type="button"
      className="group mb-3 flex w-full flex-col items-center"
      onClick={async () => {
        const response = await fetch("/api/session", { method: "DELETE" });
        if (response.ok) window.location.assign("/login");
      }}
    >
      <span className="m-0.5 flex h-9 w-9 items-center justify-center rounded-cds-6 text-zinc-400 transition-colors duration-75 group-hover:bg-white/[0.08] group-hover:text-zinc-50">
        <SignOutIcon />
      </span>
      <span className="block px-1.5 pt-0.5 text-center text-[10px] font-medium leading-3 text-zinc-400 transition-colors duration-75 group-hover:text-zinc-50">
        Sign out
      </span>
    </button>
  );
}

/** The dashboard's category rail: 36px icon buttons with a 10px label under each. */
function CategoryRail({ pathname }: { pathname: string }) {
  const activeCategory = categoryFor(pathname);
  return (
    <nav aria-label="Router navigation" className="flex w-16 shrink-0 flex-col items-center">
      <Link
        href="/routing"
        aria-label="Chalk Router home"
        className="flex h-12 w-16 shrink-0 items-center justify-center text-cds-ink-strong"
      >
        <LogoSquare />
      </Link>
      <ul className="flex w-full flex-col items-center">
        {RAIL_CATEGORIES.map((category) => {
          const active = category === activeCategory;
          return (
            <li key={category.href} className="group flex w-full flex-col items-center">
              <Link
                href={category.href}
                aria-label={category.label}
                aria-current={active ? "page" : undefined}
                className={`m-0.5 flex h-9 w-9 items-center justify-center rounded-cds-6 transition-colors duration-75 ${
                  active
                    ? "bg-cds-selected text-white"
                    : "text-zinc-400 group-hover:bg-white/[0.08] group-hover:text-zinc-50"
                }`}
              >
                <RouterIcon name={category.icon} size={18} />
              </Link>
              <span
                aria-hidden
                className={`block truncate px-1.5 pb-3 pt-0.5 text-center text-[10px] font-medium leading-3 transition-colors duration-75 ${
                  active ? "text-white" : "text-zinc-400 group-hover:text-zinc-50"
                }`}
              >
                {category.label}
              </span>
            </li>
          );
        })}
      </ul>
      <div className="mt-auto w-full">
        <SignOutButton />
      </div>
    </nav>
  );
}

/**
 * The dashboard's logged-in shell (LayoutV2): the category rail and top rail on the inset
 * background, and the page on an elevated pane stroked on every side but the right.
 */
export function AppShell({ children }: { children: ReactNode }) {
  const pathname = usePathname();
  // Signed-out pages stand alone on the inset background, like the dashboard's login.
  if (pathname === "/login") {
    return <main className="flex min-h-screen items-center justify-center bg-cds-inset p-4">{children}</main>;
  }
  return (
    <div className="flex h-screen min-h-0 overflow-hidden bg-cds-inset">
      <CategoryRail pathname={pathname} />
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center px-4">
          {/* useSearchParams needs a boundary so statically rendered routes still prerender. */}
          <Suspense>
            <Breadcrumbs />
          </Suspense>
        </header>
        <div className="mb-2 flex min-h-0 flex-1 overflow-hidden rounded-l-cds-6 border-y border-l border-cds-border-subtle bg-cds-elevated">
          {fillsPane(pathname) ? (
            <main className="flex min-w-0 flex-1 flex-col px-4 pb-4 pt-3">{children}</main>
          ) : (
            <main className="min-w-0 flex-1 overflow-y-auto px-4 pb-16 pt-3">{children}</main>
          )}
        </div>
      </div>
    </div>
  );
}
