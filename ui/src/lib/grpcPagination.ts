/**
 * Fully drain a cursor-paginated gRPC list RPC by following `next_cursor`
 * until the server reports none.
 *
 * This is the gRPC-Web analogue of `fetchAllAdminPages` (lib/adminPagination.ts):
 * server components call the router's gRPC clients (lib/grpc.ts) directly and
 * have no REST `next_cursor` envelope to page through, so a single
 * `{ limit: 1000 }` call silently truncates any collection larger than the
 * limit. Use this whenever a full collection is needed (e.g. to populate a
 * filter dropdown or build a lookup map) rather than a single page for
 * infinite-scroll UI.
 *
 * Example:
 *
 *   const keys = await fetchAllGrpcPages(
 *     (cursor) => apiKeyClient().listKeys({ limit: 1000, cursor }),
 *     (page) => page.keys,
 *   );
 */
export async function fetchAllGrpcPages<Item, Page extends { nextCursor?: string }>(
  fetchPage: (cursor: string | undefined) => Promise<Page>,
  items: (page: Page) => Item[],
): Promise<Item[]> {
  const rows: Item[] = [];
  const seenCursors = new Set<string>();
  let cursor: string | undefined;

  do {
    const page = await fetchPage(cursor);
    rows.push(...items(page));
    cursor = page.nextCursor;
    if (cursor && seenCursors.has(cursor)) {
      throw new Error("gRPC pagination returned the same cursor twice");
    }
    if (cursor) seenCursors.add(cursor);
  } while (cursor);

  return rows;
}
