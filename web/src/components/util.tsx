import { ReactNode, useEffect, useRef, useState } from 'react';
import { streamChanges } from '../api';

// Plan 3.2: re-run `load` whenever the control plane reports that the
// task/node/workflow-run status fingerprint changed. Idle pages make no
// requests; a status change shows up in well under a second. The loader is
// read through a ref so prop/state changes (e.g. filters) never go stale.
export function useLiveRefresh(load: () => void) {
  const latest = useRef(load);
  latest.current = load;
  useEffect(() => {
    const h = streamChanges(() => latest.current());
    return () => h.close();
  }, []);
}

export function StatusBadge({ status }: { status: string }) {
  return <span className={`badge ${statusClass(status)}`}>{status}</span>;
}

export function statusClass(status: string): string {
  switch (status) {
    case 'succeeded':
    case 'online':
    case 'allowed':
      return 'ok';
    case 'failed':
    case 'offline':
    case 'lost':
    case 'revoked':
    case 'denied':
    case 'expired':
      return 'bad';
    case 'running':
    case 'validating':
    case 'assigned':
    case 'degraded':
    case 'blocked':
    case 'plan_ready':
      return 'warn';
    case 'queued':
    case 'pending':
      return 'idle';
    case 'cancelled':
      return 'cancel';
    default:
      return 'idle';
  }
}

export function fmtTime(s: string | null): string {
  if (!s) return '—';
  const d = new Date(s);
  if (isNaN(d.getTime())) return s;
  return d.toLocaleString();
}

// Relative timestamps ("2m ago") for table columns; the absolute value
// stays available as the title tooltip. Refreshing re-renders via <TimeAgo>.
export function fmtAgo(s: string | null, now = Date.now()): string {
  if (!s) return '—';
  const d = new Date(s);
  if (isNaN(d.getTime())) return s;
  const sec = Math.max(0, Math.round((now - d.getTime()) / 1000));
  if (sec < 10) return 'just now';
  if (sec < 60) return `${sec}s ago`;
  const min = Math.round(sec / 60);
  if (min < 60) return `${min}m ago`;
  const h = Math.round(min / 60);
  if (h < 24) return `${h}h ago`;
  const days = Math.round(h / 24);
  if (days < 30) return `${days}d ago`;
  return d.toLocaleDateString();
}

/** Re-rendering clock: ticks every 30s so <TimeAgo> stays fresh. */
export function useNow(intervalMs = 30_000): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(t);
  }, [intervalMs]);
  return now;
}

export function TimeAgo({ s }: { s: string | null }) {
  const now = useNow();
  return (
    <span title={fmtTime(s)}>{fmtAgo(s, now)}</span>
  );
}

export function ErrorBox({ err }: { err: unknown }) {
  const msg = err instanceof Error ? err.message : String(err);
  return <div className="error">{msg}</div>;
}

export function Loading({ children }: { children?: ReactNode }) {
  return <div className="muted">{children ?? 'Loading…'}</div>;
}

// Skeleton placeholder for table-backed views: same shape/height as a real
// .grid row so the layout does not jump when data lands. `rows` mimics the
// expected page size, `cols` the table's column count.
export function TableSkeleton({ rows = 6, cols = 4 }: { rows?: number; cols?: number }) {
  return (
    <div className="skeleton-table" role="status" aria-label="Loading">
      {Array.from({ length: rows }, (_, r) => (
        <div className="skeleton-row" key={r}>
          {Array.from({ length: cols }, (_, c) => (
            <span
              className={'skeleton-cell' + (c === 0 ? ' narrow' : '')}
              key={c}
            />
          ))}
        </div>
      ))}
    </div>
  );
}

// Client-side pagination for long tables served without a usable cursor
// (e.g. /v1/audit caps at 500 rows per request). Renders a slice of
// `items` via children and a "Show more" footer when the full list is
// longer than the initial page.
export function Pager({
  items,
  initial = 25,
  step = 50,
  children,
}: {
  items: unknown[];
  initial?: number;
  step?: number;
  children: (shown: unknown[]) => ReactNode;
}) {
  const [count, setCount] = useState(initial);
  useEffect(() => {
    setCount(initial);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [items.length]);
  const shown = items.slice(0, count);
  return (
    <>
      {children(shown)}
      {items.length > count && (
        <div className="pager">
          <span className="muted">
            {count} of {items.length}
          </span>
          <button className="secondary" onClick={() => setCount(count + step)}>
            Show more
          </button>
        </div>
      )}
    </>
  );
}
