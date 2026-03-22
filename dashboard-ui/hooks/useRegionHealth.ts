'use client';

import { useState, useCallback, useEffect } from 'react';
import type { Region, ServerStatus } from '@/lib/types';
import { regionApiUrl } from '@/lib/regions';
import { useInterval } from './useInterval';

export type RegionHealth = Map<string, ServerStatus | null>;

/**
 * Polls `/api/status` on every known region in parallel every 10 seconds.
 * Returns a Map of regionId → ServerStatus (null = unreachable).
 */
export function useRegionHealth(regions: Region[]): RegionHealth {
  const [health, setHealth] = useState<RegionHealth>(new Map());

  const poll = useCallback(async () => {
    if (regions.length === 0) return;

    const results = await Promise.allSettled(
      regions.map((r) =>
        fetch(`${regionApiUrl(r)}/api/status`, { signal: AbortSignal.timeout(5000) })
          .then((res) => (res.ok ? (res.json() as Promise<ServerStatus>) : Promise.reject()))
      )
    );

    const map = new Map<string, ServerStatus | null>();
    regions.forEach((r, i) => {
      const result = results[i];
      map.set(r.id, result.status === 'fulfilled' ? result.value : null);
    });
    setHealth(map);
  }, [regions]);

  useEffect(() => {
    poll();
  }, [poll]);
  useInterval(poll, 10_000);

  return health;
}
