import { ReactNode } from 'react';

export function StatusBadge({ status }: { status: string }) {
  return <span className={`badge ${statusClass(status)}`}>{status}</span>;
}

export function statusClass(status: string): string {
  switch (status) {
    case 'succeeded':
    case 'online':
      return 'ok';
    case 'failed':
    case 'offline':
    case 'lost':
    case 'revoked':
      return 'bad';
    case 'running':
    case 'validating':
    case 'assigned':
    case 'degraded':
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
