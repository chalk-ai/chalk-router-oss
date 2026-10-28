"use client";

import { useEffect, useMemo, useState } from "react";
import { usageCsv, usageCsvFilename, type UsageCsvRow } from "./usageCsv";
import { DataTable } from "@/components/router/RouterPrimitives";
import { formatCompactCount, formatExactCount } from "@/lib/format";

type GroupBy = "global" | "connection" | "model" | "api_key" | "usage_pool" | "api_key_label";
type Metric = "total_tokens" | "prompt_tokens" | "uncached_input_tokens" | "completion_tokens" | "cached_tokens" | "cache_write_tokens" | "reasoning_tokens" | "request_count";

type UsageRow = UsageCsvRow;

type SelectOption = {
  value: string;
  label: string;
  secondaryLabel?: string;
  title?: string;
};

export interface UsageExplorerOptions {
  apiKeys: Array<SelectOption & { labels: Record<string, string> }>;
  connections: SelectOption[];
  models: SelectOption[];
  usagePools: SelectOption[];
}

const GROUPS: Array<{ value: GroupBy; label: string }> = [
  { value: "model", label: "Model" },
  { value: "connection", label: "Connection" },
  { value: "api_key", label: "API key" },
  { value: "usage_pool", label: "Usage pool" },
  { value: "api_key_label", label: "API-key label" },
  { value: "global", label: "None" },
];

const METRICS: Array<{ value: Metric; label: string; unit: string }> = [
  { value: "total_tokens", label: "Total tokens", unit: "tokens" },
  { value: "prompt_tokens", label: "Total input tokens", unit: "tokens" },
  { value: "uncached_input_tokens", label: "Uncached input tokens", unit: "tokens" },
  { value: "completion_tokens", label: "Output tokens", unit: "tokens" },
  { value: "cached_tokens", label: "Cached input tokens", unit: "tokens" },
  { value: "cache_write_tokens", label: "Cache-write tokens", unit: "tokens" },
  { value: "reasoning_tokens", label: "Reasoning tokens", unit: "tokens" },
  { value: "request_count", label: "Requests", unit: "requests" },
];

const RANGES = [
  { value: 24, label: "Last 24 hours" },
  { value: 24 * 7, label: "Last 7 days" },
  { value: 24 * 30, label: "Last 30 days" },
  { value: 24 * 90, label: "Last 90 days" },
];

const BUCKETS = [
  { value: "1m", seconds: 60, label: "Minute" },
  { value: "5m", seconds: 300, label: "5 minutes" },
  { value: "15m", seconds: 900, label: "15 minutes" },
  { value: "1h", seconds: 3_600, label: "Hour" },
  { value: "6h", seconds: 21_600, label: "6 hours" },
  { value: "1d", seconds: 86_400, label: "Day" },
  { value: "7d", seconds: 604_800, label: "Week" },
];

const COLORS = ["#3b82f6", "#e86524", "#18a17a", "#d99400", "#9b59b6", "#e54c83", "#14b8a6", "#7c8a9d", "#6b7280"];

function defaultBucket(rangeHours: number): string {
  if (rangeHours <= 24) return "1h";
  if (rangeHours <= 24 * 30) return "1d";
  return "7d";
}

function asBigInt(value: string): bigint {
  try {
    return BigInt(value);
  } catch {
    return 0n;
  }
}

function metricValue(row: UsageRow, metric: Metric): bigint {
  if (metric !== "uncached_input_tokens") return asBigInt(row[metric]);
  const promptTokens = asBigInt(row.prompt_tokens);
  const cachedTokens = asBigInt(row.cached_tokens) + asBigInt(row.cache_write_tokens);
  return promptTokens > cachedTokens ? promptTokens - cachedTokens : 0n;
}

function bucketLabel(bucket: string, rangeHours: number): string {
  const date = new Date(bucket);
  return rangeHours <= 24
    ? date.toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit", timeZone: "UTC" })
    : date.toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
}

function SelectFilter({ label, values, options, onChange }: {
  label: string;
  values: string[];
  options: SelectOption[];
  onChange: (values: string[]) => void;
}) {
  const summary = values.length === 0
    ? "All"
    : values.length === 1
      ? options.find((option) => option.value === values[0])?.label ?? values[0]
      : `${values.length} selected`;

  return (
    <details className="relative">
      <summary className="list-none cursor-pointer select-none rounded-lg px-3 py-2 text-sm" style={{ background: "#111827", border: "1px solid #374151", color: "#e5e7eb" }}>
        <span style={{ color: "#9ca3af" }}>{label}</span> {summary} <span style={{ color: "#6b7280" }}>⌄</span>
      </summary>
      <div className="absolute z-20 mt-2 max-h-72 w-72 overflow-auto rounded-lg p-2 shadow-xl" style={{ background: "#1b1d24", border: "1px solid #374151" }}>
        <button type="button" onClick={() => onChange([])} className="w-full rounded px-2 py-1.5 text-left text-xs hover:bg-white/5" style={{ color: values.length === 0 ? "#70CD96" : "#9ca3af" }}>
          All {label.toLowerCase()}s
        </button>
        {options.length === 0 ? (
          <p className="px-2 py-2 text-xs" style={{ color: "#9ca3af" }}>No configured {label.toLowerCase()} values yet.</p>
        ) : options.map((option) => {
          const checked = values.includes(option.value);
          return (
            <label key={option.value} title={option.title} className="flex cursor-pointer items-start gap-2 rounded px-2 py-1.5 text-xs hover:bg-white/5">
              <input
                type="checkbox"
                checked={checked}
                onChange={() => onChange(checked ? values.filter((value) => value !== option.value) : [...values, option.value])}
                className="mt-0.5 accent-emerald-400"
              />
              <span className="min-w-0 flex-1">
                <span className="block truncate" style={{ color: "#d1d5db" }}>{option.label}</span>
                {option.secondaryLabel && (
                  <span className="mt-0.5 block truncate text-[11px] leading-4" style={{ color: "#7c8798" }}>
                    {option.secondaryLabel}
                  </span>
                )}
              </span>
            </label>
          );
        })}
      </div>
    </details>
  );
}

function LabelFilter({ values, labelKeys, onChange }: {
  values: Array<[string, string]>;
  labelKeys: string[];
  onChange: (values: Array<[string, string]>) => void;
}) {
  return (
    <details className="relative">
      <summary className="list-none cursor-pointer select-none rounded-lg px-3 py-2 text-sm" style={{ background: values.length ? "#163323" : "#111827", border: "1px solid #3f7655", color: "#d1fae5" }}>
        + {values.length === 0 ? "API-key label filter" : `${values.length} label filter${values.length === 1 ? "" : "s"}`} <span style={{ color: "#70CD96" }}>⌄</span>
      </summary>
      <div className="absolute z-20 mt-2 w-96 rounded-lg p-3 shadow-xl" style={{ background: "#1b1d24", border: "1px solid #374151" }}>
        <p className="mb-3 text-xs" style={{ color: "#9ca3af" }}>Every label filter narrows the result set.</p>
        {values.map(([key, value], index) => (
          <div key={index} className="mb-2 flex items-center gap-2">
            <input value={key} list="usage-label-keys" placeholder="Label key" onChange={(event) => onChange(values.map((item, itemIndex) => itemIndex === index ? [event.target.value, item[1]] : item))} className="min-w-0 flex-1 rounded px-2 py-1.5 text-xs outline-none" style={{ background: "#111827", border: "1px solid #4b5563", color: "#e5e7eb" }} />
            <span style={{ color: "#6b7280" }}>=</span>
            <input value={value} placeholder="Label value" onChange={(event) => onChange(values.map((item, itemIndex) => itemIndex === index ? [item[0], event.target.value] : item))} className="min-w-0 flex-1 rounded px-2 py-1.5 text-xs outline-none" style={{ background: "#111827", border: "1px solid #4b5563", color: "#e5e7eb" }} />
            <button type="button" onClick={() => onChange(values.filter((_, itemIndex) => itemIndex !== index))} aria-label="Remove label filter" style={{ color: "#9ca3af" }}>×</button>
          </div>
        ))}
        <datalist id="usage-label-keys">{labelKeys.map((key) => <option key={key} value={key} />)}</datalist>
        <button type="button" onClick={() => onChange([...values, ["", ""]])} className="mt-1 text-xs" style={{ color: "#70CD96" }}>+ Add label filter</button>
      </div>
    </details>
  );
}

function SummaryCard({ label, value, detail, loading }: {
  label: string;
  value: bigint;
  detail?: string;
  loading: boolean;
}) {
  return (
    <div className="rounded-xl px-5 py-4" style={{ background: "#111827", border: "1px solid #29303a" }}>
      <div className="text-sm" style={{ color: "#9ca3af" }}>{label}</div>
      <div className="mt-2 text-3xl font-semibold tabular-nums text-white">{loading ? "—" : formatExactCount(value)}</div>
      {detail && !loading && <div className="mt-1 text-xs tabular-nums" style={{ color: "#6b7280" }}>{detail}</div>}
    </div>
  );
}

function UsageChart({ rows, metric, groupBy, rangeHours, labels }: {
  rows: UsageRow[];
  metric: Metric;
  groupBy: GroupBy;
  rangeHours: number;
  labels: Map<string, string>;
}) {
  const metricInfo = METRICS.find((item) => item.value === metric)!;
  const data = useMemo(() => {
    const buckets = new Map<string, Map<string, bigint>>();
    const totals = new Map<string, bigint>();
    for (const row of rows) {
      const series = groupBy === "global" ? "__global__" : row.dimension_value || "__unattributed__";
      const value = metricValue(row, metric);
      const values = buckets.get(row.bucket) ?? new Map<string, bigint>();
      values.set(series, (values.get(series) ?? 0n) + value);
      buckets.set(row.bucket, values);
      totals.set(series, (totals.get(series) ?? 0n) + value);
    }
    const ranked = [...totals].sort(([, left], [, right]) => right > left ? 1 : right < left ? -1 : 0).map(([series]) => series);
    const visible = ranked.slice(0, 8);
    const hidden = new Set(ranked.slice(8));
    if (hidden.size > 0) visible.push("__other__");
    return {
      buckets,
      names: [...buckets.keys()].sort(),
      visible,
      hidden,
    };
  }, [groupBy, metric, rows]);

  if (rows.length === 0) return <div className="flex justify-center py-24 text-sm" style={{ color: "#6b7280" }}>No usage in this range.</div>;

  const label = (series: string) => {
    if (series === "__global__") return "All usage";
    if (series === "__unattributed__") return "Unattributed";
    if (series === "__other__") return "Other";
    return labels.get(series) ?? series;
  };
  const sums = data.names.map((bucket) => [...data.buckets.get(bucket)!.values()].reduce((sum, value) => sum + value, 0n));
  const max = sums.reduce((largest, value) => value > largest ? value : largest, 0n) || 1n;
  const width = 900;
  const height = 330;
  const margin = { top: 14, right: 18, bottom: 52, left: 78 };
  const plotWidth = width - margin.left - margin.right;
  const plotHeight = height - margin.top - margin.bottom;
  const slot = plotWidth / Math.max(data.names.length, 1);
  const barWidth = Math.max(4, Math.min(44, slot * 0.58));
  const scale = (value: bigint) => Number(value) / Number(max) * plotHeight;

  return (
    <>
      <div className="mb-4">
        <div>
          <h2 className="text-base font-medium text-white">{metricInfo.label} by {GROUPS.find((item) => item.value === groupBy)?.label.toLowerCase()}</h2>
          <p className="mt-1 text-sm" style={{ color: "#9ca3af" }}>{groupBy === "global" ? "Aggregated across all dimensions." : "Each bar is stacked by the selected group."}</p>
        </div>
      </div>
      <svg viewBox={`0 0 ${width} ${height}`} className="w-full" style={{ minHeight: 270, maxHeight: 380 }} role="img" aria-label={`${metricInfo.label} usage chart`}>
        {Array.from({ length: 5 }, (_, index) => {
          const value = max * BigInt(index) / 4n;
          const y = margin.top + plotHeight - scale(value);
          return <g key={index}><line x1={margin.left} x2={width - margin.right} y1={y} y2={y} stroke="#29303a" /><text x={margin.left - 10} y={y + 4} textAnchor="end" fill="#8490a0" fontSize="11">{formatCompactCount(value)}</text></g>;
        })}
        {data.names.map((bucket, bucketIndex) => {
          const values = data.buckets.get(bucket)!;
          const x = margin.left + slot * bucketIndex + (slot - barWidth) / 2;
          let offset = 0;
          return <g key={bucket}>
            {data.visible.map((series, seriesIndex) => {
              const value = series === "__other__" ? [...data.hidden].reduce((sum, hidden) => sum + (values.get(hidden) ?? 0n), 0n) : values.get(series) ?? 0n;
              const barHeight = scale(value);
              const y = margin.top + plotHeight - offset - barHeight;
              offset += barHeight;
              return value > 0n ? <rect key={series} x={x} y={y} width={barWidth} height={Math.max(1, barHeight)} rx="2" fill={COLORS[seriesIndex % COLORS.length]}><title>{`${bucketLabel(bucket, rangeHours)} · ${label(series)}: ${formatExactCount(value)} ${metricInfo.unit}`}</title></rect> : null;
            })}
            {(bucketIndex % Math.max(1, Math.ceil(data.names.length / 8)) === 0) && <text x={x + barWidth / 2} y={margin.top + plotHeight + 23} textAnchor="middle" fill="#8490a0" fontSize="11">{bucketLabel(bucket, rangeHours)}</text>}
          </g>;
        })}
      </svg>
      <div className="mt-3 flex flex-wrap gap-x-5 gap-y-2">{data.visible.map((series, index) => <div key={series} className="flex items-center gap-2 text-xs" style={{ color: "#c5cbd4" }}><span className="h-2.5 w-2.5 rounded-sm" style={{ background: COLORS[index % COLORS.length] }} />{label(series)}</div>)}</div>
    </>
  );
}

const EMPTY_OPTIONS: UsageExplorerOptions = { apiKeys: [], connections: [], models: [], usagePools: [] };

export default function UsageExplorer({ options = EMPTY_OPTIONS, fixedApiKey }: { options?: UsageExplorerOptions; fixedApiKey?: string }) {
  const [rangeHours, setRangeHours] = useState(24 * 7);
  const [bucketSize, setBucketSize] = useState("");
  const [groupBy, setGroupBy] = useState<GroupBy>("model");
  const [groupByLabel, setGroupByLabel] = useState("");
  const [metric, setMetric] = useState<Metric>("total_tokens");
  const [apiKeys, setApiKeys] = useState<string[]>(fixedApiKey ? [fixedApiKey] : []);
  const [usagePools, setUsagePools] = useState<string[]>([]);
  const [connections, setConnections] = useState<string[]>([]);
  const [models, setModels] = useState<string[]>([]);
  const [apiKeyLabels, setApiKeyLabels] = useState<Array<[string, string]>>([]);
  const [rows, setRows] = useState<UsageRow[]>([]);
  const [queryRange, setQueryRange] = useState<{ start: Date; end: Date } | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);

  const summary = useMemo(() => rows.reduce((totals, row) => ({
    input: totals.input + asBigInt(row.prompt_tokens),
    output: totals.output + asBigInt(row.completion_tokens),
    cacheRead: totals.cacheRead + asBigInt(row.cached_tokens),
    cacheWrite: totals.cacheWrite + asBigInt(row.cache_write_tokens),
  }), {
    input: 0n,
    output: 0n,
    cacheRead: 0n,
    cacheWrite: 0n,
  }), [rows]);
  const breakdown = useMemo(() => {
    const totals = new Map<string, { input: bigint; output: bigint; cacheRead: bigint; cacheWrite: bigint; requests: bigint }>();
    for (const row of rows) {
      const key = row.dimension_value || "All usage";
      const current = totals.get(key) ?? { input: 0n, output: 0n, cacheRead: 0n, cacheWrite: 0n, requests: 0n };
      current.input += asBigInt(row.prompt_tokens);
      current.output += asBigInt(row.completion_tokens);
      current.cacheRead += asBigInt(row.cached_tokens);
      current.cacheWrite += asBigInt(row.cache_write_tokens);
      current.requests += asBigInt(row.request_count);
      totals.set(key, current);
    }
    return [...totals.entries()].sort((a, b) => Number((b[1].input + b[1].output) - (a[1].input + a[1].output)));
  }, [rows]);

  const validBuckets = useMemo(() => BUCKETS.filter((bucket) => rangeHours * 3_600 / bucket.seconds <= 10_000), [rangeHours]);
  const effectiveBucket = bucketSize && validBuckets.some((bucket) => bucket.value === bucketSize) ? bucketSize : defaultBucket(rangeHours);
  const labelKeys = useMemo(() => [...new Set(options.apiKeys.flatMap((key) => Object.keys(key.labels)))].sort(), [options.apiKeys]);
  const displayLabels = useMemo(() => new Map([
    ...options.apiKeys,
    ...options.usagePools,
    ...options.connections,
    ...options.models,
  ].map((option) => [option.value, option.label])), [options]);
  const filterQuery = useMemo(() => {
    const params = new URLSearchParams();
    for (const value of apiKeys) params.append("api_key", value);
    for (const value of usagePools) params.append("usage_pool", value);
    for (const value of connections) params.append("connection", value);
    for (const value of models) params.append("model", value);
    for (const [key, value] of apiKeyLabels) if (key.trim() && value.trim()) params.append("api_key_label", `${key}=${value}`);
    return params.toString();
  }, [apiKeyLabels, apiKeys, connections, models, usagePools]);

  useEffect(() => {
    if (groupBy === "api_key_label" && !groupByLabel.trim()) {
      setRows([]);
      setQueryRange(null);
      setError("Enter the API-key label to group by.");
      setLoading(false);
      return;
    }
    const controller = new AbortController();
    const end = new Date();
    const params = new URLSearchParams(filterQuery);
    const start = new Date(end.getTime() - rangeHours * 3_600_000);
    params.set("start", start.toISOString());
    params.set("end", end.toISOString());
    params.set("bucket_size", effectiveBucket);
    params.set("group_by", groupBy);
    if (groupBy === "api_key_label") params.set("group_by_api_key_label", groupByLabel.trim());
    setLoading(true);
    setError(null);
    fetch(`/api/admin/usage/query?${params}`, { signal: controller.signal })
      .then(async (response) => {
        const body = await response.json();
        if (!response.ok) throw new Error(body?.error?.message ?? "Usage query failed");
        setRows(body.data ?? []);
        setQueryRange({ start, end });
      })
      .catch((reason) => {
        if (!(reason instanceof DOMException && reason.name === "AbortError")) setError(reason instanceof Error ? reason.message : String(reason));
      })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [effectiveBucket, filterQuery, groupBy, groupByLabel, rangeHours]);

  function downloadCsv() {
    if (!queryRange || rows.length === 0) return;
    setExporting(true);
    try {
      const href = URL.createObjectURL(new Blob(["\ufeff", usageCsv(rows)], { type: "text/csv;charset=utf-8" }));
      const anchor = document.createElement("a");
      anchor.href = href;
      anchor.download = usageCsvFilename(queryRange.start, queryRange.end);
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      window.setTimeout(() => URL.revokeObjectURL(href), 0);
    } finally {
      setExporting(false);
    }
  }

  const canExport = !loading && !error && queryRange !== null && rows.length > 0;

  return (
    <section>
      <div className="mb-6 grid gap-3 md:grid-cols-2 xl:grid-cols-4">
        <SummaryCard label="Total input tokens" value={summary.input} loading={loading} />
        <SummaryCard label="Total output tokens" value={summary.output} loading={loading} />
        <SummaryCard label="Cache-read tokens" value={summary.cacheRead} loading={loading} />
        <SummaryCard label="Cache-write tokens" value={summary.cacheWrite} loading={loading} />
      </div>
      <div className="mb-3 flex flex-wrap gap-3">
        <label className="flex items-center gap-2 rounded-lg px-3 py-2 text-sm" style={{ background: "#111827", border: "1px solid #374151", color: "#e5e7eb" }}><span style={{ color: "#9ca3af" }}>Range</span><select value={rangeHours} onChange={(event) => { setRangeHours(Number(event.target.value)); setBucketSize(""); }} className="bg-transparent outline-none">{RANGES.map((range) => <option key={range.value} value={range.value} style={{ background: "#111827" }}>{range.label}</option>)}</select></label>
        <label className="flex items-center gap-2 rounded-lg px-3 py-2 text-sm" style={{ background: "#111827", border: "1px solid #374151", color: "#e5e7eb" }}><span style={{ color: "#9ca3af" }}>View by</span><select value={bucketSize} onChange={(event) => setBucketSize(event.target.value)} className="bg-transparent outline-none"><option value="" style={{ background: "#111827" }}>Auto ({BUCKETS.find((bucket) => bucket.value === defaultBucket(rangeHours))?.label})</option>{validBuckets.map((bucket) => <option key={bucket.value} value={bucket.value} style={{ background: "#111827" }}>{bucket.label}</option>)}</select></label>
      </div>
      <div className="mb-6 flex flex-wrap items-center gap-3">
        {!fixedApiKey && <SelectFilter label="API key" values={apiKeys} options={options.apiKeys} onChange={setApiKeys} />}
        <SelectFilter label="Usage pool" values={usagePools} options={options.usagePools} onChange={setUsagePools} />
        <SelectFilter label="Connection" values={connections} options={options.connections} onChange={setConnections} />
        <SelectFilter label="Model" values={models} options={options.models} onChange={setModels} />
        <LabelFilter values={apiKeyLabels} labelKeys={labelKeys} onChange={setApiKeyLabels} />
        <span className="h-7 w-px" style={{ background: "#374151" }} />
        <label className="rounded-lg px-3 py-2 text-sm" style={{ background: "#111827", border: "1px solid #374151", color: "#e5e7eb" }}><span className="mr-2" style={{ color: "#9ca3af" }}>Group by</span><select value={groupBy} onChange={(event) => setGroupBy(event.target.value as GroupBy)} className="bg-transparent outline-none">{GROUPS.map((group) => <option key={group.value} value={group.value} style={{ background: "#111827" }}>{group.label}</option>)}</select></label>
        <label className="rounded-lg px-3 py-2 text-sm" style={{ background: "#111827", border: "1px solid #374151", color: "#e5e7eb" }}><span className="mr-2" style={{ color: "#9ca3af" }}>Y-axis</span><select value={metric} onChange={(event) => setMetric(event.target.value as Metric)} className="bg-transparent outline-none">{METRICS.map((item) => <option key={item.value} value={item.value} style={{ background: "#111827" }}>{item.label}</option>)}</select></label>
        {groupBy === "api_key_label" && <label className="rounded-lg px-3 py-2 text-sm" style={{ background: "#111827", border: "1px solid #4b5563", color: "#e5e7eb" }}><span className="mr-2" style={{ color: "#9ca3af" }}>Label</span><input value={groupByLabel} list="usage-group-label-keys" placeholder="team" onChange={(event) => setGroupByLabel(event.target.value)} className="w-28 bg-transparent outline-none" /><datalist id="usage-group-label-keys">{labelKeys.map((key) => <option key={key} value={key} />)}</datalist></label>}
        <span className="group relative ml-auto">
          <button type="button" onClick={downloadCsv} disabled={!canExport || exporting} aria-label="Export CSV" className="grid h-9 w-9 place-items-center rounded-lg disabled:cursor-not-allowed disabled:opacity-50" style={{ background: "#1f2937", color: "#d1d5db", border: "1px solid #374151" }}>
            <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true"><path d="M12 3v12m0 0 4.5-4.5M12 15 7.5 10.5M4 15.5V19a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3.5" /></svg>
          </button>
          <span role="tooltip" className="pointer-events-none absolute right-0 top-full z-20 mt-2 whitespace-nowrap rounded px-2 py-1 text-xs opacity-0 shadow-lg transition-opacity group-hover:opacity-100 group-focus-within:opacity-100" style={{ background: "#1b1d24", border: "1px solid #374151", color: "#e5e7eb" }}>Export CSV</span>
        </span>
      </div>
      <div className="rounded-xl px-5 py-5" style={{ background: "#111827", border: "1px solid #29303a" }}>
        {loading ? <div className="flex justify-center py-24 text-sm" style={{ color: "#6b7280" }}>Loading usage…</div> : error ? <div className="rounded-lg px-4 py-3 text-sm" style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}>{error}</div> : <UsageChart rows={rows} metric={metric} groupBy={groupBy} rangeHours={rangeHours} labels={displayLabels} />}
      </div>
      {!loading && !error && <div className="mt-6"><DataTable rows={breakdown} rowKey={([name]) => name} empty="No usage in this range." columns={[
        { id: "name", header: GROUPS.find((item) => item.value === groupBy)?.label ?? "Breakdown", cell: ([name]) => <span className="text-white">{displayLabels.get(name) ?? name}</span> },
        { id: "requests", header: "Requests", align: "right", className: "font-mono text-xs", cell: ([, totals]) => formatExactCount(totals.requests) },
        { id: "input", header: "Input", align: "right", className: "font-mono text-xs", cell: ([, totals]) => formatExactCount(totals.input) },
        { id: "output", header: "Output", align: "right", className: "font-mono text-xs", cell: ([, totals]) => formatExactCount(totals.output) },
        { id: "cacheRead", header: "Cache read", align: "right", className: "font-mono text-xs", cell: ([, totals]) => formatExactCount(totals.cacheRead) },
        { id: "cacheWrite", header: "Cache write", align: "right", className: "font-mono text-xs", cell: ([, totals]) => formatExactCount(totals.cacheWrite) },
      ]} /></div>}
      <p className="mt-3 text-xs" style={{ color: "#6b7280" }}>Values inside a filter are combined with “any”; separate filter types, including API-key labels, are combined with “and”.</p>
    </section>
  );
}
