import { ReactNode, useEffect, useRef } from 'react';
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

export function ErrorBox({ err }: { err: unknown }) {
  const msg = err instanceof Error ? err.message : String(err);
  return <div className="error">{msg}</div>;
}

export function Loading({ children }: { children?: ReactNode }) {
  return <div className="muted">{children ?? 'Loading…'}</div>;
}
