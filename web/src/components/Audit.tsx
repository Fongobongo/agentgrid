import { useEffect, useState } from 'react';
import { AuditEvent, listAuditPage } from '../api';
import { ErrorBox, TableSkeleton, TimeAgo, useLiveRefresh } from './util';

// Plan 3.4: the audit trail — who decided what. Newest first, filterable by
// action, live-refreshed over the change stream. Approval decisions already
// surface on the Approvals page; this shows every audited control-plane
// action (task create/cancel/retry, policy decisions, enrollments, …).
// History pages back in time via the server's keyset cursor (Load more),
// so the view is no longer capped at the server's 500-row page limit.

const PAGE = 100;

export default function Audit() {
  const [rows, setRows] = useState<AuditEvent[] | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [draft, setDraft] = useState('');
  const [action, setAction] = useState('');

  const load = () => {
    listAuditPage(action || undefined, null, PAGE)
      .then((p) => {
        setError(null);
        setRows(p.items);
        setCursor(p.next_cursor);
      })
      .catch((e) => setError(e as Error));
  };

  useEffect(load, [action]); // eslint-disable-line react-hooks/exhaustive-deps
  useLiveRefresh(load);

  const loadMore = async () => {
    if (!cursor || loadingMore) return;
    setLoadingMore(true);
    try {
      const p = await listAuditPage(action || undefined, cursor, PAGE);
      setRows((prev) => [...(prev ?? []), ...p.items]);
      setCursor(p.next_cursor);
    } catch (e) {
      setError(e as Error);
    } finally {
      setLoadingMore(false);
    }
  };

  const apply = (v: string) => {
    setDraft(v);
    setAction(v.trim());
  };

  return (
    <section>
      <h2>Audit</h2>
      <div className="filters">
        <input
          placeholder="filter by action (e.g. task.create)"
          value={draft}
          onChange={(e) => apply(e.target.value)}
        />
        {action && (
          <button onClick={() => apply('')}>Clear</button>
        )}
      </div>
      {error && <ErrorBox err={error} />}
      {!rows && !error && <TableSkeleton rows={8} cols={5} />}
      {rows && rows.length === 0 && <p className="muted">No audit events{action ? ` for "${action}"` : ''}.</p>}
      {rows && rows.length > 0 && (
        <>
          <table className="grid">
            <thead>
              <tr>
                <th>Time</th>
                <th>Actor</th>
                <th>Action</th>
                <th>Subject</th>
                <th>Payload</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.id}>
                  <td><TimeAgo s={r.created_at} /></td>
                  <td>
                    {r.actor_type}
                    {r.actor_id ? `: ${r.actor_id}` : ''}
                  </td>
                  <td className="mono">{r.action}</td>
                  <td className="mono">{r.subject ?? '—'}</td>
                  <td className="prompt" title={r.payload ?? undefined}>
                    {r.payload
                      ? r.payload.length > 120
                        ? `${r.payload.slice(0, 120)}…`
                        : r.payload
                      : '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {cursor && (
            <div className="pager">
              <span className="muted">{rows.length} loaded</span>
              <button
                className="secondary"
                disabled={loadingMore}
                onClick={loadMore}
              >
                {loadingMore ? 'Loading…' : 'Load more'}
              </button>
            </div>
          )}
        </>
      )}
    </section>
  );
}
