import { describe, expect, it } from "vitest";
import { fetchAllGrpcPages } from "./grpcPagination";

interface Page {
  items: number[];
  nextCursor?: string;
}

describe("fetchAllGrpcPages", () => {
  it("returns everything from a single page with no cursor", async () => {
    const result = await fetchAllGrpcPages<number, Page>(
      async () => ({ items: [1, 2, 3] }),
      (page) => page.items,
    );
    expect(result).toEqual([1, 2, 3]);
  });

  it("follows next_cursor across multiple pages and concatenates in order", async () => {
    const pages: Record<string, Page> = {
      start: { items: [1, 2], nextCursor: "p2" },
      p2: { items: [3, 4], nextCursor: "p3" },
      p3: { items: [5] },
    };
    let calls = 0;
    const result = await fetchAllGrpcPages<number, Page>(
      async (cursor) => {
        calls++;
        return pages[cursor ?? "start"];
      },
      (page) => page.items,
    );
    expect(result).toEqual([1, 2, 3, 4, 5]);
    expect(calls).toBe(3);
  });

  it("throws instead of looping forever when a cursor repeats", async () => {
    const call = fetchAllGrpcPages<number, Page>(
      async () => ({ items: [1], nextCursor: "same" }),
      (page) => page.items,
    );
    await expect(call).rejects.toThrow(/same cursor twice/);
  });
});
