export interface UsageCsvRow {
  bucket: string;
  dimension_value: string;
  prompt_tokens: string;
  completion_tokens: string;
  cached_tokens: string;
  cache_write_tokens: string;
  reasoning_tokens: string;
  total_tokens: string;
  request_count: string;
}

const COLUMNS: Array<keyof UsageCsvRow> = [
  "bucket",
  "dimension_value",
  "prompt_tokens",
  "completion_tokens",
  "cached_tokens",
  "cache_write_tokens",
  "reasoning_tokens",
  "total_tokens",
  "request_count",
];

function escapeCsvCell(value: string): string {
  const safeValue = /^[=+\-@]/.test(value) ? `'${value}` : value;
  return `"${safeValue.replaceAll('"', '""')}"`;
}

export function usageCsv(rows: UsageCsvRow[]): string {
  return [
    COLUMNS.join(","),
    ...rows.map((row) => COLUMNS.map((column) => escapeCsvCell(row[column])).join(",")),
  ].join("\r\n");
}

function isoDate(date: Date): string {
  return date.toISOString().slice(0, 10);
}

export function usageCsvFilename(start: Date, end: Date): string {
  return `chalk-usage-${isoDate(start)}-to-${isoDate(end)}.csv`;
}
