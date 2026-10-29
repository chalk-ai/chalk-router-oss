"use client";

import Link from "next/link";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { RouterIcon, type RouterTabItem } from "./router-tabs";

/** InputV2's field box, for form controls styled inline across the admin UI. */
export const inputStyle = { background: "var(--color-cds-field)", border: "1px solid var(--color-cds-border)", color: "var(--color-cds-ink)", borderRadius: "var(--radius-cds-6)" };

export function UnderlineTabs<T extends string>({
  items,
  active,
  basePath,
  queryKey = "v",
}: {
  items: readonly RouterTabItem<T>[];
  active: T;
  basePath: string;
  queryKey?: string;
}) {
  const searchParams = useSearchParams();
  return <nav className="flex min-w-0 overflow-x-auto border-b border-cds-border" aria-label="Page sections">
    {items.map((item) => {
      const params = new URLSearchParams(searchParams.toString());
      if (queryKey === "v") params.delete("tab");
      params.set(queryKey, item.id);
      const selected = item.id === active;
      // UnderlineTabV2: 32px tab, hover surface inset 2px, a 1px indicator inset 4px from the tab's edges.
      return <Link key={item.id} href={`${basePath}?${params}`} aria-current={selected ? "page" : undefined} className={`group relative inline-flex h-8 shrink-0 items-center gap-1.5 px-2 text-cds-md font-medium transition-colors duration-75 ${selected ? "text-zinc-200" : "text-zinc-500 hover:text-zinc-300"}`}>
        <span aria-hidden className={`pointer-events-none absolute inset-x-0 inset-y-0.5 rounded-[4px] ${selected ? "" : "group-hover:bg-white/[0.05]"}`} />
        {selected && <span aria-hidden className="pointer-events-none absolute inset-x-1 -bottom-px h-px bg-zinc-300" />}
        <span className="relative inline-flex items-center gap-1.5"><RouterIcon name={item.icon} size={14} /><span>{item.label}</span></span>
        {item.count != null && <span className="relative rounded-cds-3 bg-cds-status-neutral-fill px-1 text-cds-2xs text-cds-ink-secondary">{item.count}</span>}
      </Link>;
    })}
  </nav>;
}

export function UrlTabs<T extends string>({
  items,
  defaultTab,
  children,
}: {
  items: readonly RouterTabItem<T>[];
  defaultTab: T;
  children: (active: T) => ReactNode;
}) {
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const value = searchParams.get("v");
  const active = (items.some((item) => item.id === value) ? value : defaultTab) as T;
  return <>
    <UnderlineTabs items={items} active={active} basePath={pathname} />
    <div className="mt-5">{children(active)}</div>
  </>;
}

export function PageHeader({ title, description, action }: { title: string; description?: ReactNode; action?: ReactNode }) {
  return <div className="flex items-start justify-between gap-5">
    <div className="min-w-0"><h1 className="page-title">{title}</h1>{description != null && <div className="mt-0.5 text-cds-md text-cds-ink-secondary">{description}</div>}</div>
    {action != null && <div className="shrink-0">{action}</div>}
  </div>;
}

export function EntityHeader({ backHref, backLabel, label, name, metadata, actions }: { backHref: string; backLabel: string; label: string; name: string; metadata?: ReactNode; actions?: ReactNode }) {
  return <div className="flex items-start justify-between gap-5">
    <div className="flex min-w-0 items-center gap-3">
      <Link href={backHref} aria-label={backLabel} title={backLabel} className="btn btn-tertiary btn-icon shrink-0">«</Link>
      <div className="min-w-0"><div className="eyebrow">{label}</div><div className="mt-0.5 flex items-center gap-2"><h1 className="page-title truncate">{name}</h1>{metadata}</div></div>
    </div>
    {actions != null && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
  </div>;
}

export function DetailLayout<T extends string>({ tabs, defaultTab, sidebar, children }: { tabs: readonly RouterTabItem<T>[]; defaultTab: T; sidebar: ReactNode; children: (active: T) => ReactNode }) {
  const [sidebarOpen, setSidebarOpen] = useState(true);
  return <div className="mt-5 flex items-start gap-5">
    <div className="min-w-0 flex-1"><UrlTabs items={tabs} defaultTab={defaultTab}>{children}</UrlTabs></div>
    <aside className="sticky top-6 hidden shrink-0 lg:block" style={{ width: sidebarOpen ? 300 : 42 }}>
      {sidebarOpen ? <div className="card overflow-hidden"><div className="flex items-center justify-between border-b border-cds-border px-4 py-2.5"><span className="eyebrow">Details</span><button onClick={() => setSidebarOpen(false)} className="btn btn-tertiary btn-xs btn-icon" aria-label="Hide details">›</button></div><div className="p-4">{sidebar}</div></div> : <button onClick={() => setSidebarOpen(true)} className="btn btn-secondary btn-md btn-icon" aria-label="Show details">‹</button>}
    </aside>
  </div>;
}

export function DetailRows({ rows }: { rows: Array<{ label: string; value: ReactNode }> }) {
  return <dl className="space-y-3">{rows.map((row) => <div key={row.label}><dt className="text-cds-xs text-cds-ink-subtle">{row.label}</dt><dd className="mt-0.5 break-words text-cds-sm text-cds-ink">{row.value}</dd></div>)}</dl>;
}

export function StatusBadge({ tone = "neutral", children }: { tone?: "neutral" | "green" | "yellow" | "red"; children: ReactNode }) {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function IconButton({ icon, label, tone = "neutral", onClick, disabled }: { icon: "edit" | "trash" | "activity" | "add"; label: string; tone?: "neutral" | "danger" | "green"; onClick: () => void; disabled?: boolean }) {
  const ink = tone === "danger" ? "!text-cds-status-red-text" : tone === "green" ? "!text-cds-status-green-text" : "";
  return <button type="button" onClick={onClick} disabled={disabled} aria-label={label} title={label} className={`btn btn-secondary btn-icon ${ink}`}><RouterIcon name={icon} size={14} /></button>;
}

export function Modal({ open, title, size = "lg", onClose, children, footer }: { open: boolean; title: string; size?: "sm" | "lg" | "xl"; onClose: () => void; children: ReactNode; footer?: ReactNode }) {
  useEffect(() => {
    if (!open) return;
    const close = (event: KeyboardEvent) => { if (event.key === "Escape") onClose(); };
    window.addEventListener("keydown", close);
    return () => window.removeEventListener("keydown", close);
  }, [onClose, open]);
  if (!open) return null;
  const width = size === "sm" ? "28rem" : size === "xl" ? "64rem" : "42rem";
  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-8" role="dialog" aria-modal="true" aria-label={title} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}><div className="max-h-[calc(100vh-64px)] w-full overflow-auto rounded-cds-6 border border-cds-border-strong bg-cds-raised shadow-cds-m" style={{ maxWidth: width }}><div className="sticky top-0 z-10 flex items-center justify-between border-b border-cds-border bg-cds-raised px-4 py-3"><h2 className="section-title !text-cds-ink-strong">{title}</h2><button onClick={onClose} className="btn btn-tertiary btn-icon text-lg" aria-label="Close">×</button></div><div className="p-4">{children}</div>{footer != null && <div className="sticky bottom-0 flex justify-end gap-2 border-t border-cds-border bg-cds-raised px-4 py-3">{footer}</div>}</div></div>;
}

export function FormRow({ label, description, children }: { label: string; description?: string; children: ReactNode }) {
  return <div className="grid gap-3 border-b border-cds-border-subtle px-4 py-3 last:border-b-0 md:grid-cols-[minmax(180px,1fr)_minmax(260px,1.35fr)]"><div><div className="text-cds-md font-medium text-cds-ink">{label}</div>{description != null && <p className="mt-0.5 text-cds-sm text-cds-ink-subtle">{description}</p>}</div><div>{children}</div></div>;
}

export function ErrorBanner({ children }: { children: ReactNode }) { return <div className="rounded-cds-6 border border-cds-status-red-border bg-cds-status-red-fill px-3 py-2 text-cds-md text-cds-status-red-text">{children}</div>; }
export function SkeletonRows({ rows = 3 }: { rows?: number }) { return <div className="space-y-2" role="status" aria-label="Loading">{Array.from({ length: rows }, (_, index) => <div key={index} className="h-9 animate-pulse rounded-cds-6 bg-cds-raised" />)}</div>; }

export type DataColumn<T> = { id: string; header: string; cell: (row: T) => ReactNode; align?: "left" | "right"; className?: string };
export function DataTable<T>({ columns, rows, rowKey, rowHref, empty = "No items yet.", loading = false, actionColumn = false }: { columns: readonly DataColumn<T>[]; rows: readonly T[]; rowKey: (row: T) => string; rowHref?: (row: T) => string | undefined; empty?: string; loading?: boolean; actionColumn?: boolean }) {
  const router = useRouter();
  if (loading) return <SkeletonRows />;
  if (rows.length === 0) return <div className="card px-6 py-12 text-center text-cds-md text-cds-ink-subtle">{empty}</div>;
  return <div className="card overflow-x-auto"><table className="table-cds">
    <thead><tr>{columns.map((column, index) => <th key={column.id} className={`${column.align === "right" ? "!text-right" : ""} ${actionColumn && index === columns.length - 1 ? "sticky right-0 bg-cds-raised" : ""}`}>{column.header}</th>)}</tr></thead>
    <tbody>{rows.map((row) => { const href = rowHref?.(row); return <tr key={rowKey(row)} role={href ? "link" : undefined} tabIndex={href ? 0 : undefined} onClick={href ? (event) => { if ((event.target as HTMLElement).closest("a,button,input,select,textarea")) return; router.push(href); } : undefined} onKeyDown={href ? (event) => { if (event.key === "Enter") router.push(href); } : undefined} className={href ? "cursor-pointer" : ""}>{columns.map((column, columnIndex) => <td key={column.id} className={`${column.align === "right" ? "text-right" : "text-left"} ${column.className ?? ""} ${actionColumn && columnIndex === columns.length - 1 ? "sticky right-0 bg-cds-raised" : ""}`}>{column.cell(row)}</td>)}</tr>; })}</tbody>
  </table></div>;
}

export function InfiniteScroll({ hasMore, loading, onLoadMore }: { hasMore: boolean; loading: boolean; onLoadMore: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const node = ref.current;
    if (node == null || !hasMore) return;
    const observer = new IntersectionObserver((entries) => { if (entries[0]?.isIntersecting && !loading) onLoadMore(); }, { rootMargin: "240px" });
    observer.observe(node);
    return () => observer.disconnect();
  }, [hasMore, loading, onLoadMore]);
  return <div ref={ref} className="flex h-10 items-center justify-center text-cds-sm text-cds-ink-subtle">{loading ? "Loading…" : hasMore ? "Scroll for more" : "End of list"}</div>;
}
