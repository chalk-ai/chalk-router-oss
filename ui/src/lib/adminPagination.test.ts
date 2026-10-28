import { afterEach, describe, expect, it, vi } from "vitest";
import { adminPageUrl, fetchAllAdminPages } from "./adminPagination";

describe("adminPageUrl", () => {
  it("returns the bare path when no cursor or limit is given", () => {
    expect(adminPageUrl("/api/admin/keys")).toBe("/api/admin/keys");
  });

  it("includes only the cursor when limit is omitted", () => {
    expect(adminPageUrl("/api/admin/keys", "abc")).toBe("/api/admin/keys?cursor=abc");
  });

  it("includes only the limit when cursor is omitted", () => {
    expect(adminPageUrl("/api/admin/keys", undefined, 100)).toBe("/api/admin/keys?limit=100");
  });

  it("includes both when given", () => {
    expect(adminPageUrl("/api/admin/keys", "abc", 100)).toBe(
      "/api/admin/keys?cursor=abc&limit=100",
    );
  });
});

describe("fetchAllAdminPages", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("concatenates every page until next_cursor is absent", async () => {
    const pages: Record<string, { data: number[]; next_cursor?: string }> = {
      "": { data: [1, 2], next_cursor: "p2" },
      p2: { data: [3, 4], next_cursor: "p3" },
      p3: { data: [5] },
    };
    const fetchMock = vi.fn(async (input: string | URL) => {
      const url = new URL(String(input), "http://localhost");
      const cursor = url.searchParams.get("cursor") ?? "";
      return new Response(JSON.stringify(pages[cursor]), { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);

    const rows = await fetchAllAdminPages<number>("/api/admin/things");
    expect(rows).toEqual([1, 2, 3, 4, 5]);
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("throws on a non-ok response instead of returning a partial result", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("boom", { status: 500 })),
    );
    await expect(fetchAllAdminPages("/api/admin/things")).rejects.toThrow("boom");
  });

  it("throws instead of looping forever when a cursor repeats", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ data: [1], next_cursor: "same" }), { status: 200 }),
      ),
    );
    await expect(fetchAllAdminPages("/api/admin/things")).rejects.toThrow(/same cursor twice/);
  });
});
