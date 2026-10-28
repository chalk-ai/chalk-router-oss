"use client";

import { useState, type Dispatch, type SetStateAction } from "react";
import type { AdminPage } from "./adminPagination";

export interface UsePaginationResult<T> {
  items: T[];
  setItems: Dispatch<SetStateAction<T[]>>;
  loading: boolean;
  setLoading: Dispatch<SetStateAction<boolean>>;
  nextCursor: string | undefined;
  /**
   * Merges a fetched `AdminPage` into `items` and records its `next_cursor`.
   * Pass the same `cursor` used to fetch the page: when it is set, the page's
   * rows are appended (deduped by `keyOf`, in case a row shifted between the
   * previous and next page); when it is undefined, the page replaces `items`
   * outright (a first-page load or an explicit reset to the first page).
   */
  applyPage: (page: AdminPage<T>, cursor: string | undefined) => void;
}

/**
 * Shared state and merge logic for the admin list pages' "load more" cursor
 * pagination: an items array, a loading flag, the next page's cursor, and the
 * append-with-dedup used when a page comes back. Callers keep their own
 * fetch function (some load multiple resources per page) and their own
 * `handleNextPage`/`useEffect` wiring; this hook only removes the boilerplate
 * that was identical across every list page.
 */
export function usePagination<T>(
  keyOf: (row: T) => string | number,
  initial: { items?: T[]; nextCursor?: string; loading?: boolean } = {},
): UsePaginationResult<T> {
  const [items, setItems] = useState<T[]>(initial.items ?? []);
  const [loading, setLoading] = useState(initial.loading ?? true);
  const [nextCursor, setNextCursor] = useState<string | undefined>(initial.nextCursor);

  function applyPage(page: AdminPage<T>, cursor: string | undefined) {
    setItems((current) =>
      cursor
        ? [...current, ...page.data.filter((row) => !current.some((existing) => keyOf(existing) === keyOf(row)))]
        : page.data,
    );
    setNextCursor(page.next_cursor);
  }

  return { items, setItems, loading, setLoading, nextCursor, applyPage };
}
