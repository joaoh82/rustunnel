'use client';

import type { ApiClient, TunnelLogEntry } from '@/lib/types';
import { useTunnelHistory } from '@/hooks/useTunnelHistory';
import { Panel } from './Panel';
import { relativeTime } from '@/lib/api';

interface Props {
  api: ApiClient;
  enabled: boolean;
}

// ── helpers ───────────────────────────────────────────────────────────────────

function protocolBadge(protocol: string) {
  const isHttp = protocol === 'http';
  return (
    <span
      style={{
        display: 'inline-block',
        padding: '1px 6px',
        borderRadius: 4,
        fontSize: 10,
        fontWeight: 700,
        letterSpacing: '0.04em',
        textTransform: 'uppercase',
        background: isHttp ? 'rgba(99,179,237,0.15)' : 'rgba(154,117,235,0.15)',
        color: isHttp ? 'var(--blue, #63b3ed)' : 'var(--purple, #9a75eb)',
        border: `1px solid ${isHttp ? 'rgba(99,179,237,0.3)' : 'rgba(154,117,235,0.3)'}`,
      }}
    >
      {protocol}
    </span>
  );
}

function statusDot(unregisteredAt: string | null) {
  const active = !unregisteredAt;
  return (
    <span
      title={active ? 'Active' : 'Closed'}
      style={{
        display: 'inline-block',
        width: 7,
        height: 7,
        borderRadius: '50%',
        background: active ? 'var(--green)' : 'var(--muted)',
        flexShrink: 0,
      }}
    />
  );
}

function duration(registered: string, unregistered: string | null): string {
  const end = unregistered ? new Date(unregistered) : new Date();
  const ms = end.getTime() - new Date(registered).getTime();
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

function shortId(id: string) {
  return id.slice(0, 8);
}

// ── filter tab ────────────────────────────────────────────────────────────────

interface TabProps {
  label: string;
  active: boolean;
  onClick: () => void;
}

function Tab({ label, active, onClick }: TabProps) {
  return (
    <button
      onClick={onClick}
      style={{
        padding: '2px 10px',
        fontSize: 11,
        borderRadius: 4,
        border: active ? '1px solid var(--border)' : '1px solid transparent',
        background: active ? 'var(--bg)' : 'transparent',
        color: active ? 'var(--fg)' : 'var(--muted)',
        cursor: 'pointer',
      }}
    >
      {label}
    </button>
  );
}

// ── row ───────────────────────────────────────────────────────────────────────

function HistoryRow({ entry }: { entry: TunnelLogEntry }) {
  return (
    <tr>
      <td style={{ paddingLeft: 16 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          {statusDot(entry.unregistered_at)}
          {protocolBadge(entry.protocol)}
        </div>
      </td>
      <td>
        <code style={{ fontSize: 11 }}>{entry.label}</code>
      </td>
      <td>{entry.token_label ?? <span style={{ color: 'var(--muted)' }}>admin</span>}</td>
      <td style={{ color: 'var(--muted)', fontSize: 11 }}>
        {relativeTime(entry.registered_at)}
      </td>
      <td style={{ color: 'var(--muted)', fontSize: 11 }}>
        {duration(entry.registered_at, entry.unregistered_at)}
      </td>
      <td>
        <code style={{ fontSize: 10, color: 'var(--muted)' }}>
          {shortId(entry.session_id)}
        </code>
      </td>
      <td style={{ paddingRight: 16 }}>
        <code style={{ fontSize: 10, color: 'var(--muted)' }}>
          {shortId(entry.tunnel_id)}
        </code>
      </td>
    </tr>
  );
}

// ── main panel ────────────────────────────────────────────────────────────────

export function TunnelHistoryPanel({ api, enabled }: Props) {
  const {
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
    hasPrev,
    hasNext,
  } = useTunnelHistory(api, enabled);

  const actions = (
    <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
      <Tab label="All" active={protocol === 'all'} onClick={() => changeProtocol('all')} />
      <Tab label="HTTP" active={protocol === 'http'} onClick={() => changeProtocol('http')} />
      <Tab label="TCP" active={protocol === 'tcp'} onClick={() => changeProtocol('tcp')} />
      <span style={{ fontSize: 11, color: 'var(--muted)', marginLeft: 6 }}>
        {total} total
      </span>
    </div>
  );

  return (
    <Panel title="Tunnel History" actions={actions}>
      {error && (
        <div
          style={{
            padding: '8px 16px',
            color: 'var(--red)',
            fontSize: 12,
            borderBottom: '1px solid var(--border)',
          }}
        >
          {error}
        </div>
      )}

      {entries.length === 0 && !loading ? (
        <div
          style={{
            padding: '24px 16px',
            textAlign: 'center',
            color: 'var(--muted)',
            fontSize: 12,
          }}
        >
          No tunnel history yet. Start a tunnel to see activity here.
        </div>
      ) : (
        <table
          style={{
            width: '100%',
            borderCollapse: 'collapse',
            fontSize: 12,
          }}
        >
          <thead>
            <tr
              style={{
                borderBottom: '1px solid var(--border)',
                color: 'var(--muted)',
                fontSize: 11,
                textTransform: 'uppercase',
                letterSpacing: '0.05em',
              }}
            >
              <th style={{ padding: '8px 8px 8px 16px', textAlign: 'left', fontWeight: 500 }}>
                Type
              </th>
              <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500 }}>Label</th>
              <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500 }}>Token</th>
              <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500 }}>Started</th>
              <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500 }}>Duration</th>
              <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500 }}>Session</th>
              <th style={{ padding: '8px 16px 8px 8px', textAlign: 'left', fontWeight: 500 }}>
                Tunnel ID
              </th>
            </tr>
          </thead>
          <tbody>
            {entries.map((entry) => (
              <HistoryRow key={entry.id} entry={entry} />
            ))}
          </tbody>
        </table>
      )}

      {/* Pagination */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'flex-end',
          gap: 8,
          padding: '8px 16px',
          borderTop: entries.length > 0 ? '1px solid var(--border)' : undefined,
          fontSize: 11,
          color: 'var(--muted)',
        }}
      >
        {loading && <span>Loading…</span>}
        <button
          onClick={prevPage}
          disabled={!hasPrev}
          style={{ padding: '2px 8px', fontSize: 11 }}
        >
          ← Prev
        </button>
        <span>
          Page {page} of {totalPages}
        </span>
        <button
          onClick={nextPage}
          disabled={!hasNext}
          style={{ padding: '2px 8px', fontSize: 11 }}
        >
          Next →
        </button>
      </div>
    </Panel>
  );
}
