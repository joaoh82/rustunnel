'use client';

import { useState, useCallback, useEffect } from 'react';
import type { ApiClient, Tunnel } from '@/lib/types';
import { useInterval } from './useInterval';

export interface RegionApi {
  regionId: string;
  api: ApiClient;
}

/**
 * Poll `/api/tunnels` on all supplied regional API clients in parallel.
 * Results from all regions are merged into a single flat list.
 * Regions that fail are skipped silently; their errors are collected in `errors`.
 */
export function useTunnels(regionApis: RegionApi[], enabled: boolean) {
  const [tunnels, setTunnels] = useState<Tunnel[]>([]);
  const [errors, setErrors] = useState<string[]>([]);

  const poll = useCallback(async () => {
    if (!enabled || regionApis.length === 0) return;

    const results = await Promise.allSettled(
      regionApis.map(({ api }) => api.get('/api/tunnels') as Promise<Tunnel[]>)
    );

    const all: Tunnel[] = [];
    const errs: string[] = [];
    results.forEach((r, i) => {
      if (r.status === 'fulfilled') {
        all.push(...r.value);
      } else {
        errs.push(`${regionApis[i].regionId}: ${(r.reason as Error).message}`);
      }
    });

    setTunnels(all);
    setErrors(errs);
  }, [regionApis, enabled]);

  useEffect(() => {
    poll();
  }, [poll]);
  useInterval(poll, 2000);

  // Expose first error for backwards-compat with single-region callers.
  const error = errors.length > 0 ? errors[0] : null;

  return { tunnels, error, errors, refresh: poll };
}
