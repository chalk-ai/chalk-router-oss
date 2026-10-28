import type { Metadata } from "next";
import { Sidebar } from "@/components/Sidebar";
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
      <body className="bg-gray-950 text-gray-100 min-h-screen antialiased">
        <Sidebar />
        <main
          style={{
            marginLeft: "240px",
            minHeight: "100vh",
            padding: "32px 40px",
          }}
        >
          {children}
        </main>
      </body>
    </html>
  );
}
