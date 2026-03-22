'use client';

import type { Tunnel } from '@/lib/types';
import { relativeTime, copyToClipboard } from '@/lib/api';
import { Badge } from './ui/Badge';

interface TunnelTableProps {
  tunnels: Tunnel[];
  selected: Tunnel | null;
  onSelect: (t: Tunnel | null) => void;
  onClose: (t: Tunnel) => void;
}

export function TunnelTable({ tunnels, selected, onSelect, onClose }: TunnelTableProps) {
  if (tunnels.length === 0) {
    return (
      <div
        style={{
          padding: '60px 20px',
          textAlign: 'center',
          color: 'var(--muted)',
          fontSize: 13,
        }}
      >
        <div style={{ fontSize: 32, marginBottom: 10 }}>⟳</div>
        No active tunnels — connect a client to get started.
      </div>
    );
  }

  return (
    <div style={{ overflowX: 'auto' }}>
      <table
        style={{
          width: '100%',
          borderCollapse: 'collapse',
          fontSize: 12,
          fontFamily: 'var(--mono)',
        }}
      >
        <thead>
          <tr style={{ borderBottom: '1px solid var(--border)', color: 'var(--muted)' }}>
            {['Protocol', 'Public URL', 'Region', 'Client', 'Connected', 'Requests', ''].map((h) => (
              <th
                key={h}
                style={{
                  padding: '8px 14px',
                  textAlign: 'left',
                  fontWeight: 500,
                  fontSize: 11,
                  textTransform: 'uppercase',
                  letterSpacing: '0.05em',
                  whiteSpace: 'nowrap',
                }}
              >
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {tunnels.map((t) => {
            const isSelected = selected?.tunnel_id === t.tunnel_id;
            return (
              <tr
                key={t.tunnel_id}
                onClick={() => onSelect(isSelected ? null : t)}
                style={{
                  borderBottom: '1px solid var(--border)',
                  cursor: 'pointer',
                  background: isSelected ? 'var(--accent-dim)' : 'transparent',
                  transition: 'background 0.1s',
                }}
                onMouseEnter={(e) => {
                  if (!isSelected) e.currentTarget.style.background = 'var(--surface2)';
                }}
                onMouseLeave={(e) => {
                  if (!isSelected) e.currentTarget.style.background = 'transparent';
                }}
              >
                <td style={{ padding: '10px 14px' }}>
                  <Badge
                    label={t.protocol.toUpperCase()}
                    color={t.protocol === 'http' ? 'var(--accent)' : 'var(--purple)'}
                  />
                </td>
                <td style={{ padding: '10px 14px', color: 'var(--accent)', maxWidth: 260 }}>
                  <span
                    style={{ cursor: 'pointer' }}
                    title={t.public_url}
                    onClick={(e) => {
                      e.stopPropagation();
                      copyToClipboard(t.public_url);
                    }}
                  >
                    {t.public_url}
                  </span>
                </td>
                <td style={{ padding: '10px 14px' }}>
                  <span
                    style={{
                      fontFamily: 'var(--mono)',
                      fontSize: 11,
                      color: 'var(--muted)',
                      background: 'var(--surface2)',
                      padding: '1px 5px',
                      borderRadius: 3,
                    }}
                  >
                    {t.region_id || '—'}
                  </span>
                </td>
                <td style={{ padding: '10px 14px', color: 'var(--muted)' }}>
                  {t.client_addr ?? '—'}
                </td>
                <td style={{ padding: '10px 14px', color: 'var(--muted)', whiteSpace: 'nowrap' }}>
                  {relativeTime(t.connected_since)}
                </td>
                <td style={{ padding: '10px 14px' }}>
                  {t.request_count > 0 ? (
                    <span style={{ color: 'var(--text)' }}>{t.request_count.toLocaleString()}</span>
                  ) : (
                    <span style={{ color: 'var(--muted)' }}>0</span>
                  )}
                </td>
                <td style={{ padding: '10px 14px' }}>
                  <button
                    className="danger"
                    style={{ padding: '3px 8px', fontSize: 11 }}
                    onClick={(e) => {
                      e.stopPropagation();
                      if (confirm(`Force close tunnel ${t.label}?`)) onClose(t);
                    }}
                  >
                    ✕ Close
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
