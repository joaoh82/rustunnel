'use client';

import type { Region } from '@/lib/types';
import type { RegionHealth } from '@/hooks/useRegionHealth';
import { Dot } from './ui/Dot';

interface HeaderProps {
  regions: Region[];
  regionHealth: RegionHealth;
  onSignOut: () => void;
}

export function Header({ regions, regionHealth, onSignOut }: HeaderProps) {
  const totalSessions = Array.from(regionHealth.values())
    .filter(Boolean)
    .reduce((sum, s) => sum + (s?.active_sessions ?? 0), 0);
  const totalTunnels = Array.from(regionHealth.values())
    .filter(Boolean)
    .reduce((sum, s) => sum + (s?.active_tunnels ?? 0), 0);

  return (
    <header
      style={{
        display: 'flex',
        alignItems: 'center',
        padding: '0 20px',
        height: 50,
        background: 'var(--surface)',
        borderBottom: '1px solid var(--border)',
        gap: 12,
        position: 'sticky',
        top: 0,
        zIndex: 100,
      }}
    >
      <span style={{ fontSize: 18 }}>🔗</span>
      <span style={{ fontWeight: 600, fontSize: 14, letterSpacing: '-0.2px', marginRight: 'auto' }}>
        Rustunnel Dashboard
      </span>

      {/* Per-region health dots */}
      {regions.length > 0 && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
          {regions.map((r) => {
            const status = regionHealth.get(r.id);
            const ok = status?.ok === true;
            const unknown = status === undefined;
            const color = unknown ? 'var(--muted)' : ok ? 'var(--green)' : 'var(--red)';
            const label = unknown ? 'connecting…' : ok ? 'healthy' : 'offline';
            return (
              <div
                key={r.id}
                title={`${r.id.toUpperCase()} (${r.location}) — ${label}`}
                style={{ display: 'flex', alignItems: 'center', gap: 4, cursor: 'default' }}
              >
                <Dot color={color} pulse={ok} />
                <span style={{ fontSize: 11, color: 'var(--muted)', fontFamily: 'var(--mono)' }}>
                  {r.id.toUpperCase()}
                </span>
              </div>
            );
          })}
        </div>
      )}

      {/* Aggregate stats */}
      {regionHealth.size > 0 && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 14, color: 'var(--muted)', fontSize: 12 }}>
          <span title="Total active sessions across all regions">
            {totalSessions} session{totalSessions !== 1 ? 's' : ''}
          </span>
          <span title="Total active tunnels across all regions">
            {totalTunnels} tunnel{totalTunnels !== 1 ? 's' : ''}
          </span>
        </div>
      )}

      <button onClick={onSignOut} style={{ marginLeft: 8 }}>
        Sign Out
      </button>
    </header>
  );
}
