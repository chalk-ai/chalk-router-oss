import { describe, expect, it } from "vitest";
import {
  formatCompactCount,
  formatCompactTokenLimit,
  formatDate,
  formatDateTime,
  formatExactCount,
  formatNumber,
  formatUtcDateTime,
} from "./format";

describe("formatNumber", () => {
  it("adds thousands separators to a plain number", () => {
    expect(formatNumber(1234567)).toBe("1,234,567");
  });

  it("adds thousands separators to a bigint", () => {
    expect(formatNumber(1234567890123n)).toBe("1,234,567,890,123");
  });

  it("passes small values through unchanged", () => {
    expect(formatNumber(42)).toBe("42");
  });
});

describe("formatCompactTokenLimit", () => {
  it("leaves sub-thousand values as-is", () => {
    expect(formatCompactTokenLimit(500)).toBe("500");
  });

  it("uses a K suffix for thousands, dropping a trailing .0", () => {
    expect(formatCompactTokenLimit(2_000)).toBe("2K");
  });

  it("keeps one decimal place for a non-round thousands value", () => {
    expect(formatCompactTokenLimit(2_500)).toBe("2.5K");
  });

  it("uses an M suffix for millions, dropping a trailing .0", () => {
    expect(formatCompactTokenLimit(2_000_000)).toBe("2M");
  });

  it("keeps one decimal place for a non-round millions value", () => {
    expect(formatCompactTokenLimit(2_500_000)).toBe("2.5M");
  });
});

describe("formatCompactCount", () => {
  it("leaves sub-thousand values as-is", () => {
    expect(formatCompactCount(500n)).toBe("500");
  });

  it("always keeps one decimal once a K suffix applies", () => {
    expect(formatCompactCount(2_000n)).toBe("2.0K");
  });

  it("always keeps one decimal once an M suffix applies", () => {
    expect(formatCompactCount(2_000_000n)).toBe("2.0M");
  });

  it("uses a B suffix at a billion", () => {
    expect(formatCompactCount(2_500_000_000n)).toBe("2.5B");
  });
});

describe("formatExactCount", () => {
  it("locale-formats a bigint", () => {
    expect(formatExactCount(1_234_567n)).toBe("1,234,567");
  });
});

describe("formatUtcDateTime", () => {
  it("returns an em dash placeholder for an empty string", () => {
    expect(formatUtcDateTime("")).toBe("—");
  });

  it("formats an ISO timestamp in UTC regardless of local timezone", () => {
    const formatted = formatUtcDateTime("2026-01-05T15:04:00Z");
    expect(formatted).toContain("Jan 5, 2026");
    expect(formatted).toContain("UTC");
  });
});

describe("formatDateTime", () => {
  it("includes both the date and the time", () => {
    const formatted = formatDateTime("2026-01-05T15:04:00Z");
    expect(formatted).toContain("Jan 5, 2026");
  });
});

describe("formatDate", () => {
  it("formats a date without a time component", () => {
    expect(formatDate("2026-01-05T15:04:00Z")).toBe("Jan 5, 2026");
  });
});
