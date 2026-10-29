import type { Metadata } from "next";
import { AppShell } from "@/components/AppShell";
import "./globals.css";

export const metadata: Metadata = {
  title: "chalk-router",
  description: "LLM proxy & router admin UI",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body className="bg-cds-inset text-cds-ink antialiased">
        <AppShell>{children}</AppShell>
      </body>
    </html>
  );
}
