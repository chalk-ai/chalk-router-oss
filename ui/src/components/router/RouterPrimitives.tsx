"use client";

import Link from "next/link";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { RouterIcon, type RouterTabItem } from "./router-tabs";

/** Shared dark-input styling for form controls across the admin UI. */
export const inputStyle = { background: "#1f2937", border: "1px solid #374151", color: "#e5e7eb" };

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
  return <nav className="flex min-w-0 gap-1 overflow-x-auto border-b border-slate-800" aria-label="Page sections">
    {items.map((item) => {
      const params = new URLSearchParams(searchParams.toString());
      if (queryKey === "v") params.delete("tab");
      params.set(queryKey, item.id);
      const selected = item.id === active;
      return <Link key={item.id} href={`${basePath}?${params}`} className="flex shrink-0 items-center gap-2 border-b-2 px-3 py-3 text-sm transition-colors" style={{ borderColor: selected ? "#70CD96" : "transparent", color: selected ? "#f3f4f6" : "#8a94a5", fontWeight: selected ? 500 : 400 }}>
        <RouterIcon name={item.icon} />
        <span>{item.label}</span>
        {item.count != null && <span className="rounded-full bg-slate-800 px-1.5 py-0.5 text-[10px] text-slate-400">{item.count}</span>}
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
    <div className="min-w-0"><h1 className="text-2xl font-semibold tracking-tight text-white">{title}</h1>{description != null && <div className="mt-1 text-sm text-slate-400">{description}</div>}</div>
    {action != null && <div className="shrink-0">{action}</div>}
  </div>;
}

export function EntityHeader({ backHref, backLabel, label, name, metadata, actions }: { backHref: string; backLabel: string; label: string; name: string; metadata?: ReactNode; actions?: ReactNode }) {
  return <div className="flex items-start justify-between gap-5">
    <div className="flex min-w-0 items-center gap-3">
      <Link href={backHref} aria-label={backLabel} title={backLabel} className="grid h-8 w-8 shrink-0 place-items-center rounded-md text-slate-400 transition hover:bg-slate-800 hover:text-white">«</Link>
      <div className="min-w-0"><div className="text-[11px] font-medium uppercase tracking-wider text-slate-500">{label}</div><div className="mt-0.5 flex items-center gap-2"><h1 className="truncate text-xl font-semibold text-white">{name}</h1>{metadata}</div></div>
    </div>
    {actions != null && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
  </div>;
}

export function DetailLayout<T extends string>({ tabs, defaultTab, sidebar, children }: { tabs: readonly RouterTabItem<T>[]; defaultTab: T; sidebar: ReactNode; children: (active: T) => ReactNode }) {
  const [sidebarOpen, setSidebarOpen] = useState(true);
  return <div className="mt-5 flex items-start gap-5">
    <div className="min-w-0 flex-1"><UrlTabs items={tabs} defaultTab={defaultTab}>{children}</UrlTabs></div>
    <aside className="sticky top-6 hidden shrink-0 lg:block" style={{ width: sidebarOpen ? 300 : 42 }}>
      {sidebarOpen ? <div className="overflow-hidden rounded-lg border border-slate-800 bg-slate-900"><div className="flex items-center justify-between border-b border-slate-800 px-4 py-3"><span className="text-xs font-semibold uppercase tracking-wider text-slate-400">Details</span><button onClick={() => setSidebarOpen(false)} className="text-slate-500 hover:text-white" aria-label="Hide details">›</button></div><div className="p-4">{sidebar}</div></div> : <button onClick={() => setSidebarOpen(true)} className="grid h-10 w-10 place-items-center rounded-lg border border-slate-800 bg-slate-900 text-slate-400" aria-label="Show details">‹</button>}
    </aside>
  </div>;
}

export function DetailRows({ rows }: { rows: Array<{ label: string; value: ReactNode }> }) {
  return <dl className="space-y-3">{rows.map((row) => <div key={row.label}><dt className="text-[11px] text-slate-500">{row.label}</dt><dd className="mt-0.5 break-words text-xs text-slate-200">{row.value}</dd></div>)}</dl>;
}

export function StatusBadge({ tone = "neutral", children }: { tone?: "neutral" | "green" | "yellow" | "red"; children: ReactNode }) {
  const colors = { neutral: ["#1f2937", "#cbd5e1"], green: ["#052e16", "#86efac"], yellow: ["#3b2305", "#fde68a"], red: ["#3f1010", "#fca5a5"] }[tone];
  return <span className="inline-flex rounded px-2 py-0.5 text-xs font-medium" style={{ background: colors[0], color: colors[1] }}>{children}</span>;
}

export function IconButton({ icon, label, tone = "neutral", onClick, disabled }: { icon: "edit" | "trash" | "activity" | "add"; label: string; tone?: "neutral" | "danger" | "green"; onClick: () => void; disabled?: boolean }) {
  const colors = tone === "danger" ? "border-red-900 bg-red-950 text-red-300" : tone === "green" ? "border-emerald-900 bg-emerald-950 text-emerald-300" : "border-slate-700 bg-slate-800 text-slate-300";
  return <button type="button" onClick={onClick} disabled={disabled} aria-label={label} title={label} className={`grid h-8 w-8 place-items-center rounded-md border transition hover:brightness-125 disabled:opacity-50 ${colors}`}><RouterIcon name={icon} size={14} /></button>;
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
  return <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-8" role="dialog" aria-modal="true" aria-label={title} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}><div className="max-h-[calc(100vh-64px)] w-full overflow-auto rounded-xl border border-slate-700 bg-slate-900 shadow-2xl" style={{ maxWidth: width }}><div className="sticky top-0 z-10 flex items-center justify-between border-b border-slate-800 bg-slate-900 px-5 py-4"><h2 className="text-base font-semibold text-white">{title}</h2><button onClick={onClose} className="text-xl text-slate-500 hover:text-white" aria-label="Close">×</button></div><div className="p-5">{children}</div>{footer != null && <div className="sticky bottom-0 flex justify-end gap-2 border-t border-slate-800 bg-slate-900 px-5 py-4">{footer}</div>}</div></div>;
}

export function FormRow({ label, description, children }: { label: string; description?: string; children: ReactNode }) {
  return <div className="grid gap-3 border-b border-slate-800 px-4 py-4 last:border-b-0 md:grid-cols-[minmax(180px,1fr)_minmax(260px,1.35fr)]"><div><div className="text-sm font-medium text-slate-200">{label}</div>{description != null && <p className="mt-1 text-xs leading-5 text-slate-500">{description}</p>}</div><div>{children}</div></div>;
}

export function ErrorBanner({ children }: { children: ReactNode }) { return <div className="rounded-lg border border-red-900 bg-red-950/60 px-4 py-3 text-sm text-red-300">{children}</div>; }
export function SkeletonRows({ rows = 3 }: { rows?: number }) { return <div className="space-y-2" role="status" aria-label="Loading">{Array.from({ length: rows }, (_, index) => <div key={index} className="h-11 animate-pulse rounded bg-slate-900" />)}</div>; }

export type DataColumn<T> = { id: string; header: string; cell: (row: T) => ReactNode; align?: "left" | "right"; className?: string };
export function DataTable<T>({ columns, rows, rowKey, rowHref, empty = "No items yet.", loading = false, actionColumn = false }: { columns: readonly DataColumn<T>[]; rows: readonly T[]; rowKey: (row: T) => string; rowHref?: (row: T) => string | undefined; empty?: string; loading?: boolean; actionColumn?: boolean }) {
  const router = useRouter();
  if (loading) return <SkeletonRows />;
  if (rows.length === 0) return <div className="rounded-lg border border-slate-800 bg-slate-900 px-6 py-12 text-center text-sm text-slate-500">{empty}</div>;
  return <div className="overflow-x-auto rounded-lg border border-slate-800"><table className="w-full text-sm"><thead><tr className="border-b border-slate-800 bg-slate-900">{columns.map((column, index) => <th key={column.id} className={`px-4 py-3 ${column.align === "right" ? "text-right" : "text-left"} text-xs font-medium text-slate-400 ${actionColumn && index === columns.length - 1 ? "sticky right-0 bg-slate-900" : ""}`}>{column.header}</th>)}</tr></thead><tbody>{rows.map((row, index) => { const href = rowHref?.(row); return <tr key={rowKey(row)} role={href ? "link" : undefined} tabIndex={href ? 0 : undefined} onClick={href ? (event) => { if ((event.target as HTMLElement).closest("a,button,input,select,textarea")) return; router.push(href); } : undefined} onKeyDown={href ? (event) => { if (event.key === "Enter") router.push(href); } : undefined} className={`${href ? "cursor-pointer hover:bg-slate-800/70" : ""} border-b border-slate-800 last:border-b-0`} style={{ background: index % 2 === 0 ? "#0f172a" : "#111827" }}>{columns.map((column, columnIndex) => <td key={column.id} className={`px-4 py-3 ${column.align === "right" ? "text-right" : "text-left"} ${column.className ?? ""} ${actionColumn && columnIndex === columns.length - 1 ? "sticky right-0 bg-inherit" : ""}`}>{column.cell(row)}</td>)}</tr>; })}</tbody></table></div>;
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
  return <div ref={ref} className="flex h-10 items-center justify-center text-xs text-slate-500">{loading ? "Loading…" : hasMore ? "Scroll for more" : "End of list"}</div>;
}
