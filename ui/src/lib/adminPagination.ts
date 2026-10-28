import type { NextRequest } from "next/server";

export const ADMIN_PAGE_SIZE = 40;

export interface AdminPage<T> {
  data: T[];
  next_cursor?: string;
}

export function adminPageRequest(req: NextRequest): {
  limit: number;
  cursor?: string;
} {
  const rawLimit = req.nextUrl.searchParams.get("limit");
  const parsedLimit = rawLimit == null ? ADMIN_PAGE_SIZE : Number(rawLimit);
  const limit =
    Number.isSafeInteger(parsedLimit) && parsedLimit > 0
      ? Math.min(parsedLimit, 1000)
      : ADMIN_PAGE_SIZE;
  const cursor = req.nextUrl.searchParams.get("cursor") || undefined;
  return { limit, cursor };
}

export function adminPageUrl(path: string, cursor?: string, limit?: number): string {
  const params = new URLSearchParams();
  if (cursor) params.set("cursor", cursor);
  if (limit != null) params.set("limit", String(limit));
  const query = params.toString();
  return query ? `${path}?${query}` : path;
}

export async function fetchAdminPage<T>(
  path: string,
  cursor?: string,
  limit?: number,
): Promise<AdminPage<T>> {
  const res = await fetch(adminPageUrl(path, cursor, limit));
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

/** Load every page from an admin collection without silently truncating it. */
export async function fetchAllAdminPages<T>(
  path: string,
  limit = 1000,
): Promise<T[]> {
  const rows: T[] = [];
  const seenCursors = new Set<string>();
  let cursor: string | undefined;

  do {
    const page = await fetchAdminPage<T>(path, cursor, limit);
    rows.push(...page.data);
    cursor = page.next_cursor;
    if (cursor && seenCursors.has(cursor)) {
      throw new Error(`Admin pagination returned the same cursor twice for ${path}`);
    }
    if (cursor) seenCursors.add(cursor);
  } while (cursor);

  return rows;
}
