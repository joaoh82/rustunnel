import type { Region } from './types';

// ── built-in fallback ─────────────────────────────────────────────────────────
// Mirrors the hardcoded list in crates/rustunnel-client/src/regions.rs.

const BUILTIN_REGIONS: Region[] = [
  {
    id: 'eu',
    name: 'Europe',
    location: 'Helsinki, FI',
    host: 'eu.edge.rustunnel.com',
    control_port: 4040,
    active: true,
  },
  {
    id: 'us',
    name: 'US East',
    location: 'Hillsboro, OR',
    host: 'us.edge.rustunnel.com',
    control_port: 4040,
    active: true,
  },
  {
    id: 'ap',
    name: 'Asia Pacific',
    location: 'Singapore',
    host: 'ap.edge.rustunnel.com',
    control_port: 4040,
    active: true,
  },
];

// ── URL helpers ───────────────────────────────────────────────────────────────

/** Dashboard REST API base URL for the given region. */
export function regionApiUrl(region: Region): string {
  return `https://${region.host}:8443`;
}

// ── loading ───────────────────────────────────────────────────────────────────

/**
 * Load the active region list.
 *
 * Strategy:
 *   1. Call GET /api/regions on the primary API (NEXT_PUBLIC_API_URL or same-origin).
 *   2. If that fails or returns an empty list, fall back to BUILTIN_REGIONS.
 */
export async function loadRegions(): Promise<Region[]> {
  const base = process.env.NEXT_PUBLIC_API_URL ?? '';
  try {
    const r = await fetch(`${base}/api/regions`);
    if (r.ok) {
      const regions = (await r.json()) as Region[];
      if (Array.isArray(regions) && regions.length > 0) return regions;
    }
  } catch {
    // network error — fall through to built-in
  }
  return BUILTIN_REGIONS;
}

export { BUILTIN_REGIONS };
