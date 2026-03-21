'use client';

import type { ApiClient, TunnelLogEntry } from '@/lib/types';
import { useTunnelHistory, type SortBy } from '@/hooks/useTunnelHistory';
import { Panel } from './Panel';
import { relativeTime } from '@/lib/api';

interface Props {
  api: ApiClient;
  enabled: boolean;
  /** When set, the panel shows history for that token only (no token column). */
  tokenId?: string;
  /** Compact mode — used when embedded inside the TokensPanel drill-down. */
  compact?: boolean;
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

// ── filter/sort tab ───────────────────────────────────────────────────────────

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

// ── sortable column header ────────────────────────────────────────────────────

interface SortHeaderProps {
  label: string;
  col: SortBy;
  activeSortBy: SortBy;
  sortDir: 'asc' | 'desc';
  onToggle: (col: SortBy) => void;
  style?: React.CSSProperties;
}

function SortHeader({ label, col, activeSortBy, sortDir, onToggle, style }: SortHeaderProps) {
  const isActive = activeSortBy === col;
  return (
    <th
      onClick={() => onToggle(col)}
      style={{
        padding: '8px',
        textAlign: 'left',
        fontWeight: 500,
        cursor: 'pointer',
        userSelect: 'none',
        color: isActive ? 'var(--fg)' : 'var(--muted)',
        whiteSpace: 'nowrap',
        ...style,
      }}
    >
      {label}
      {isActive && (
        <span style={{ marginLeft: 4, opacity: 0.7 }}>
          {sortDir === 'desc' ? '↓' : '↑'}
        </span>
      )}
    </th>
  );
}

// ── row ───────────────────────────────────────────────────────────────────────

function HistoryRow({ entry, showToken }: { entry: TunnelLogEntry; showToken: boolean }) {
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
      {showToken && (
        <td>{entry.token_label ?? <span style={{ color: 'var(--muted)' }}>admin</span>}</td>
      )}
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

// ── inner table (shared between standalone panel and token drill-down) ────────

interface HistoryTableProps {
  api: ApiClient;
  enabled: boolean;
  tokenId?: string;
  compact?: boolean;
}

export function HistoryTable({ api, enabled, tokenId, compact }: HistoryTableProps) {
  const {
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
    hasPrev,
    hasNext,
  } = useTunnelHistory(api, enabled, { tokenId });

  const showToken = !tokenId;

  const filterBar = (
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
      {/* Protocol tabs */}
      <Tab label="All" active={protocol === 'all'} onClick={() => changeProtocol('all')} />
      <Tab label="HTTP" active={protocol === 'http'} onClick={() => changeProtocol('http')} />
      <Tab label="TCP" active={protocol === 'tcp'} onClick={() => changeProtocol('tcp')} />
      <span style={{ width: 1, height: 14, background: 'var(--border)', display: 'inline-block', margin: '0 4px' }} />
      {/* Status tabs */}
      <Tab label="Active" active={activeFilter === 'active'} onClick={() => changeActiveFilter('active')} />
      <Tab label="Closed" active={activeFilter === 'closed'} onClick={() => changeActiveFilter('closed')} />
      <span style={{ fontSize: 11, color: 'var(--muted)', marginLeft: 6 }}>
        {total} total
      </span>
    </div>
  );

  if (compact) {
    return (
      <div style={{ borderTop: '1px solid var(--border)' }}>
        <div style={{ padding: '8px 16px', borderBottom: '1px solid var(--border)' }}>
          {filterBar}
        </div>
        {renderTable()}
        {renderPagination()}
      </div>
    );
  }

  return (
    <>
      {error && (
        <div style={{ padding: '8px 16px', color: 'var(--red)', fontSize: 12, borderBottom: '1px solid var(--border)' }}>
          {error}
        </div>
      )}
      {filterBar && (
        <div style={{ padding: '6px 16px', borderBottom: '1px solid var(--border)' }}>
          {filterBar}
        </div>
      )}
      {renderTable()}
      {renderPagination()}
    </>
  );

  function renderTable() {
    if (entries.length === 0 && !loading) {
      return (
        <div style={{ padding: '24px 16px', textAlign: 'center', color: 'var(--muted)', fontSize: 12 }}>
          No tunnel history yet. Start a tunnel to see activity here.
        </div>
      );
    }
    return (
      <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 12 }}>
        <thead>
          <tr
            style={{
              borderBottom: '1px solid var(--border)',
              fontSize: 11,
              textTransform: 'uppercase',
              letterSpacing: '0.05em',
            }}
          >
            <SortHeader
              label="Type"
              col="protocol"
              activeSortBy={sortBy}
              sortDir={sortDir}
              onToggle={toggleSort}
              style={{ paddingLeft: 16 }}
            />
            <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500, color: 'var(--muted)' }}>Label</th>
            {showToken && (
              <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500, color: 'var(--muted)' }}>Token</th>
            )}
            <SortHeader
              label="Started"
              col="started"
              activeSortBy={sortBy}
              sortDir={sortDir}
              onToggle={toggleSort}
            />
            <SortHeader
              label="Duration"
              col="duration"
              activeSortBy={sortBy}
              sortDir={sortDir}
              onToggle={toggleSort}
            />
            <th style={{ padding: '8px', textAlign: 'left', fontWeight: 500, color: 'var(--muted)' }}>Session</th>
            <th style={{ padding: '8px 16px 8px 8px', textAlign: 'left', fontWeight: 500, color: 'var(--muted)' }}>
              Tunnel ID
            </th>
          </tr>
        </thead>
        <tbody>
          {entries.map((entry) => (
            <HistoryRow key={entry.id} entry={entry} showToken={showToken} />
          ))}
        </tbody>
      </table>
    );
  }

  function renderPagination() {
    return (
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
        <button onClick={prevPage} disabled={!hasPrev} style={{ padding: '2px 8px', fontSize: 11 }}>
          ← Prev
        </button>
        <span>Page {page} of {totalPages}</span>
        <button onClick={nextPage} disabled={!hasNext} style={{ padding: '2px 8px', fontSize: 11 }}>
          Next →
        </button>
      </div>
    );
  }
}

// ── main panel ────────────────────────────────────────────────────────────────

export function TunnelHistoryPanel({ api, enabled }: Props) {
  return (
    <Panel title="Tunnel History">
      <HistoryTable api={api} enabled={enabled} />
    </Panel>
  );
}
