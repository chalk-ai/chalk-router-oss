"use client";

import { useEffect, useState } from "react";

type Tool = "curl" | "python";

interface ToolDef {
  id: Tool;
  label: string;
  icon: React.ReactNode;
}

const TOOLS: ToolDef[] = [
  {
    id: "python",
    label: "OpenAI Python SDK",
    icon: (
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
        <polyline points="16 18 22 12 16 6" />
        <polyline points="8 6 2 12 8 18" />
      </svg>
    ),
  },
  {
    id: "curl",
    label: "curl",
    icon: (
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
        <polyline points="4 17 10 11 4 5" />
        <line x1="12" y1="19" x2="20" y2="19" />
      </svg>
    ),
  },
];

interface CodeBlockProps {
  language: string;
  code: string;
}

function CodeBlock({ language, code }: CodeBlockProps) {
  const [copied, setCopied] = useState(false);

  function copy() {
    navigator.clipboard.writeText(code);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  }

  return (
    <div className="relative">
      <div
        className="flex items-center justify-between px-3 py-1.5 rounded-t-md text-xs"
        style={{
          background: "#0b1220",
          borderTop: "1px solid #1f2937",
          borderLeft: "1px solid #1f2937",
          borderRight: "1px solid #1f2937",
          color: "#6b7280",
        }}
      >
        <span style={{ fontFamily: "'Everett Mono', monospace" }}>{language}</span>
        <button
          onClick={copy}
          className="px-2 py-0.5 rounded transition-colors"
          style={{
            background: copied ? "#052e16" : "#172554",
            color: copied ? "#86efac" : "#93c5fd",
          }}
        >
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <pre
        className="overflow-x-auto rounded-b-md px-4 py-3 text-xs leading-relaxed"
        style={{
          background: "#03070d",
          border: "1px solid #1f2937",
          borderTop: "none",
          color: "#d1d5db",
          fontFamily: "'Everett Mono', monospace",
        }}
      >
        <code>{code}</code>
      </pre>
    </div>
  );
}

interface QuickstartProps {
  sampleToken: string | null;
}

export function Quickstart({ sampleToken }: QuickstartProps) {
  const [open, setOpen] = useState(true);
  const [tool, setTool] = useState<Tool>("curl");
  const [origin, setOrigin] = useState<string>("");

  useEffect(() => {
    if (typeof window !== "undefined") {
      setOrigin(window.location.origin);
    }
  }, []);

  const baseUrl = origin || "https://your-chalk-router";
  const apiKey = sampleToken ?? "ck_your_token_here";

  function snippet(): { language: string; code: string } {
    switch (tool) {
      case "python":
        return {
          language: "python",
          code: `from openai import OpenAI

client = OpenAI(
    base_url="${baseUrl}/v1",
    api_key="${apiKey}",
)

response = client.chat.completions.create(
    model="openai/gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(response.choices[0].message.content)`,
        };
      case "curl":
        return {
          language: "shell",
          code: `curl ${baseUrl}/v1/chat/completions \\
  -H "Authorization: Bearer ${apiKey}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "openai/gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'`,
        };
    }
  }

  const { language, code } = snippet();

  return (
    <div
      className="mb-6 rounded-lg overflow-hidden"
      style={{ background: "#111827", border: "1px solid #1f2937" }}
    >
      <button
        onClick={() => setOpen((o) => !o)}
        className="w-full flex items-center justify-between px-5 py-3.5"
        style={{
          background: "transparent",
          borderBottom: open ? "1px solid #1f2937" : "none",
        }}
      >
        <div className="flex items-center gap-3">
          <span
            className="flex items-center justify-center rounded-md"
            style={{
              width: "28px",
              height: "28px",
              background: "#052e16",
              color: "#70CD96",
            }}
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2" />
            </svg>
          </span>
          <div className="text-left">
            <div className="text-sm font-semibold text-white">Quickstart</div>
            <div className="text-xs" style={{ color: "#6b7280" }}>
              Send OpenAI-compatible requests through this router.
            </div>
          </div>
        </div>
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="#6b7280"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          style={{
            transform: open ? "rotate(180deg)" : "rotate(0deg)",
            transition: "transform 0.15s",
          }}
        >
          <polyline points="6 9 12 15 18 9" />
        </svg>
      </button>

      {open && (
        <div className="px-5 py-4 space-y-3">
          <div className="grid grid-cols-2 gap-3 text-xs">
            <div
              className="rounded-md px-3 py-2"
              style={{ background: "#0f172a", border: "1px solid #1f2937" }}
            >
              <div className="mb-1" style={{ color: "#6b7280" }}>
                Base URL
              </div>
              <code
                className="block break-all"
                style={{
                  color: "#70CD96",
                  fontFamily: "'Everett Mono', monospace",
                }}
              >
                {baseUrl}/v1
              </code>
            </div>
            <div
              className="rounded-md px-3 py-2"
              style={{ background: "#0f172a", border: "1px solid #1f2937" }}
            >
              <div className="mb-1" style={{ color: "#6b7280" }}>
                Auth header
              </div>
              <code
                className="block break-all"
                style={{
                  color: "#93c5fd",
                  fontFamily: "'Everett Mono', monospace",
                }}
              >
                Authorization: Bearer &lt;your-token&gt;
              </code>
            </div>
          </div>

          <div className="flex items-center gap-1 flex-wrap">
            {TOOLS.map((t) => {
              const active = tool === t.id;
              return (
                <button
                  key={t.id}
                  onClick={() => setTool(t.id)}
                  className="flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium transition-colors"
                  style={{
                    background: active ? "#052e16" : "#0f172a",
                    color: active ? "#70CD96" : "#9ca3af",
                    border: active ? "1px solid #166534" : "1px solid #1f2937",
                  }}
                >
                  {t.icon}
                  {t.label}
                </button>
              );
            })}
          </div>

          <CodeBlock language={language} code={code} />

          {!sampleToken && (
            <p className="text-xs" style={{ color: "#6b7280" }}>
              Issue a token below — the snippet will pre-fill with the new key,
              and it&rsquo;s the only time it will be shown.
            </p>
          )}
        </div>
      )}
    </div>
  );
}
