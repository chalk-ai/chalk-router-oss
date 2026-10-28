"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

type NavItem = { href: string; label: string; icon: React.ReactNode };

const routingIcon = <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round"><circle cx="6" cy="6" r="2"/><circle cx="18" cy="12" r="2"/><circle cx="6" cy="18" r="2"/><path d="m8 7 8 4m-8 6 8-4"/></svg>;
const accessIcon = <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round"><path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/></svg>;

const NAV_ITEMS: NavItem[] = [
  { href: "/routing", label: "Routing", icon: routingIcon },
  { href: "/access", label: "Access", icon: accessIcon },
  { href: "/telemetry", label: "Telemetry", icon: <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round"><path d="M22 12h-4l-3 9L9 3l-3 9H2"/></svg> },
  { href: "/playground", label: "Playground", icon: <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round"><path d="M14.5 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2V7.5L14.5 2z"/><polyline points="14 2 14 8 20 8"/><path d="m10 13-2 2 2 2m4 0 2-2-2-2"/></svg> },
];

export function Sidebar() {
  const pathname = usePathname();
  return (
    <aside className="fixed top-0 left-0 flex h-screen flex-col" style={{ width: "240px", background: "#111827", borderRight: "1px solid #1f2937", zIndex: 40 }}>
      <Link href="/routing" className="block" style={{ padding: "20px 20px 16px", borderBottom: "1px solid #1f2937" }}>
        <div className="flex items-center gap-2.5"><div className="rounded" style={{ width: "28px", height: "28px", background: "#ffffff" }} /><div className="text-sm font-semibold leading-none tracking-tight text-white">Chalk Router</div></div>
      </Link>
      <nav className="flex-1 py-4" style={{ overflowY: "auto" }}>
        {NAV_ITEMS.map((item) => {
          const isActive = pathname === item.href;
          return <Link key={item.href} href={item.href} className="mx-2 flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors" style={{ color: isActive ? "#e5e7eb" : "#9ca3af", background: isActive ? "#1f2937" : "transparent", fontWeight: isActive ? 500 : 400 }}>
            <span className="flex-shrink-0" style={{ color: isActive ? "#70CD96" : "#6b7280" }}>{item.icon}</span>{item.label}
          </Link>;
        })}
      </nav>
      {pathname !== "/login" && <button className="mx-4 mb-4 text-left text-sm text-gray-400" onClick={async () => {
        const response = await fetch("/api/session", { method: "DELETE" });
        if (response.ok) window.location.assign("/login");
      }}>Sign out</button>}
      <div style={{ padding: "12px 16px", borderTop: "1px solid #1f2937" }}><div className="flex items-center gap-2.5"><div className="flex h-7 w-7 items-center justify-center rounded-full text-xs font-medium" style={{ background: "#1f2937", color: "#70CD96" }}>CR</div><span className="text-xs" style={{ color: "#9ca3af" }}>Standalone router</span></div></div>
    </aside>
  );
}
