'use client';

import { useState, useCallback } from 'react';
import type { ApiClient, ApiToken } from '@/lib/types';
import { useInterval } from './useInterval';

export function useTokens(api: ApiClient, enabled: boolean) {
  const [tokens, setTokens] = useState<ApiToken[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (!enabled) return;
    try {
      const data = await api.get('/api/tokens');
      setTokens(data as ApiToken[]);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [api, enabled]);

  useInterval(refresh, 5000);

  return { tokens, error, refresh };
}
