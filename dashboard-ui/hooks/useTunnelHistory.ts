'use client';

import { useState, useCallback, useEffect, useRef } from 'react';
import type { ApiClient, TunnelLogEntry } from '@/lib/types';

const PAGE_SIZE = 25;

export function useTunnelHistory(api: ApiClient, enabled: boolean) {
  const [entries, setEntries] = useState<TunnelLogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [protocol, setProtocol] = useState<'all' | 'http' | 'tcp'>('all');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Keep a ref to avoid stale closures in the interval.
  const stateRef = useRef({ offset, protocol });
  stateRef.current = { offset, protocol };

  const fetch = useCallback(
    async (nextOffset: number, nextProtocol: 'all' | 'http' | 'tcp') => {
      if (!enabled) return;
      setLoading(true);
      setError(null);
      try {
        const params = new URLSearchParams({
          limit: String(PAGE_SIZE),
          offset: String(nextOffset),
        });
        if (nextProtocol !== 'all') params.set('protocol', nextProtocol);

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
    [api, enabled],
  );

  // Re-fetch when offset or protocol changes.
  useEffect(() => {
    fetch(offset, protocol);
  }, [fetch, offset, protocol]);

  // Polling: re-fetch the current page every 10 s.
  useEffect(() => {
    if (!enabled) return;
    const id = setInterval(() => {
      fetch(stateRef.current.offset, stateRef.current.protocol);
    }, 10_000);
    return () => clearInterval(id);
  }, [fetch, enabled]);

  function prevPage() {
    setOffset((o) => Math.max(0, o - PAGE_SIZE));
  }

  function nextPage() {
    setOffset((o) => o + PAGE_SIZE);
  }

  function changeProtocol(p: 'all' | 'http' | 'tcp') {
    setProtocol(p);
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
    page,
    totalPages,
    prevPage,
    nextPage,
    hasPrev: offset > 0,
    hasNext: offset + PAGE_SIZE < total,
  };
}
