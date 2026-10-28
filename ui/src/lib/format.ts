/**
 * Shared number/date formatting helpers for the admin UI.
 *
 * Several pages had reimplemented the same "compact K/M suffix" or
 * "UTC timestamp" logic locally (in one case, byte-for-byte identical
 * functions in two different files). Centralizing them here means a
 * rounding or locale fix only has to be made once, and gives item 12's
 * test suite a single module to cover instead of duplicated private
 * functions that can't be imported from a test file.
 */

/** Locale-formatted integer/bigint, e.g. `1234567` -> `"1,234,567"`. */
export function formatNumber(value: number | bigint): string {
  return value.toLocaleString("en-US");
}

/**
 * Compact "K/M" suffix for a plain token/limit count, dropping a trailing
 * ".0" when the value divides evenly.
 *
 * Examples: `500` -> `"500"`, `2_000_000` -> `"2M"`, `2_500_000` -> `"2.5M"`.
 */
export function formatCompactTokenLimit(limit: number): string {
  if (limit >= 1_000_000) {
    return `${Number.isInteger(limit / 1_000_000) ? limit / 1_000_000 : (limit / 1_000_000).toFixed(1)}M`;
  }
  if (limit >= 1_000) {
    return `${Number.isInteger(limit / 1_000) ? limit / 1_000 : (limit / 1_000).toFixed(1)}K`;
  }
  return limit.toLocaleString("en-US");
}

/**
 * Compact "K/M/B" suffix for a (possibly large) bigint metric count,
 * always keeping one decimal place once a suffix applies.
 *
 * Examples: `500n` -> `"500"`, `2_000_000n` -> `"2.0M"`.
 */
export function formatCompactCount(value: bigint): string {
  if (value >= 1_000_000_000n) return `${(Number(value) / 1_000_000_000).toFixed(1)}B`;
  if (value >= 1_000_000n) return `${(Number(value) / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000n) return `${(Number(value) / 1_000).toFixed(1)}K`;
  return value.toLocaleString("en-US");
}

/** Exact locale-formatted bigint, e.g. for a tooltip next to a compact value. */
export function formatExactCount(value: bigint): string {
  return value.toLocaleString("en-US");
}

/**
 * Full UTC date + time with an explicit timezone label, e.g.
 * `"Jan 5, 2026, 03:04 PM UTC"`. Used for timestamps (like quota resets)
 * that are meaningful only in UTC regardless of the viewer's locale.
 */
export function formatUtcDateTime(value: string): string {
  if (!value) return "—";
  return new Intl.DateTimeFormat("en-US", {
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    timeZone: "UTC",
    timeZoneName: "short",
  }).format(new Date(value));
}

/** Local date + time, e.g. `"Jan 5, 2026, 03:04 PM"`. */
export function formatDateTime(dateStr: string): string {
  return new Date(dateStr).toLocaleDateString("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Local date only, e.g. `"Jan 5, 2026"`. */
export function formatDate(dateStr: string): string {
  return new Date(dateStr).toLocaleDateString("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}
