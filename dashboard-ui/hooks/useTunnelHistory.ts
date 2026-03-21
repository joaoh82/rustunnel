'use client';

import { useState, useCallback, useEffect, useRef } from 'react';
import type { ApiClient, TunnelLogEntry } from '@/lib/types';

const PAGE_SIZE = 25;

export type SortBy = 'started' | 'duration' | 'protocol';
export type SortDir = 'asc' | 'desc';
export type ActiveFilter = 'all' | 'active' | 'closed';
export type ProtocolFilter = 'all' | 'http' | 'tcp';

interface Options {
  /** Lock the hook to a specific token ID — used for token drill-down. */
  tokenId?: string;
}

export function useTunnelHistory(api: ApiClient, enabled: boolean, options: Options = {}) {
  const [entries, setEntries] = useState<TunnelLogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [protocol, setProtocol] = useState<ProtocolFilter>('all');
  const [activeFilter, setActiveFilter] = useState<ActiveFilter>('all');
  const [sortBy, setSortBy] = useState<SortBy>('started');
  const [sortDir, setSortDir] = useState<SortDir>('desc');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const { tokenId } = options;

  // Keep a ref to avoid stale closures in the interval.
  const stateRef = useRef({ offset, protocol, activeFilter, sortBy, sortDir });
  stateRef.current = { offset, protocol, activeFilter, sortBy, sortDir };

  const fetch = useCallback(
    async (
      nextOffset: number,
      nextProtocol: ProtocolFilter,
      nextActive: ActiveFilter,
      nextSortBy: SortBy,
      nextSortDir: SortDir,
    ) => {
      if (!enabled) return;
      setLoading(true);
      setError(null);
      try {
        const params = new URLSearchParams({
          limit: String(PAGE_SIZE),
          offset: String(nextOffset),
          sort_by: nextSortBy,
          sort_dir: nextSortDir,
        });
        if (nextProtocol !== 'all') params.set('protocol', nextProtocol);
        if (nextActive === 'active') params.set('active', 'true');
        if (nextActive === 'closed') params.set('active', 'false');
        if (tokenId) params.set('token_id', tokenId);

        const data = (await api.get(`/api/history?${params}`)) as {
          entries: TunnelLogEntry[];
          total: number;
        };
        setEntries(data.entries);
        setTotal(data.total);
      } catch (e) {
        setError((e as Error).message);
      } finally {
        setLoading(false);
      }
    },
    [api, enabled, tokenId],
  );

  // Re-fetch when any filter/sort changes.
  useEffect(() => {
    fetch(offset, protocol, activeFilter, sortBy, sortDir);
  }, [fetch, offset, protocol, activeFilter, sortBy, sortDir]);

  // Polling: re-fetch the current page every 10 s.
  useEffect(() => {
    if (!enabled) return;
    const id = setInterval(() => {
      const s = stateRef.current;
      fetch(s.offset, s.protocol, s.activeFilter, s.sortBy, s.sortDir);
    }, 10_000);
    return () => clearInterval(id);
  }, [fetch, enabled]);

  function prevPage() {
    setOffset((o) => Math.max(0, o - PAGE_SIZE));
  }

  function nextPage() {
    setOffset((o) => o + PAGE_SIZE);
  }

  function changeProtocol(p: ProtocolFilter) {
    setProtocol(p);
    setOffset(0);
  }

  function changeActiveFilter(a: ActiveFilter) {
    setActiveFilter(a);
    setOffset(0);
  }

  function toggleSort(col: SortBy) {
    if (sortBy === col) {
      setSortDir((d) => (d === 'desc' ? 'asc' : 'desc'));
    } else {
      setSortBy(col);
      setSortDir('desc');
    }
    setOffset(0);
  }

  const page = Math.floor(offset / PAGE_SIZE) + 1;
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE));

  return {
    entries,
    total,
    loading,
    error,
    protocol,
    changeProtocol,
    activeFilter,
    changeActiveFilter,
    sortBy,
    sortDir,
    toggleSort,
    page,
    totalPages,
    prevPage,
    nextPage,
    hasPrev: offset > 0,
    hasNext: offset + PAGE_SIZE < total,
  };
}
