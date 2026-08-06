import { useEffect, useState } from 'react';
import { AuditEvent, listAudit } from '../api';
import { ErrorBox, Loading, fmtTime, useLiveRefresh } from './util';

// Plan 3.4: the audit trail — who decided what. Newest first, filterable by
// action, live-refreshed over the change stream. Approval decisions already
// surface on the Approvals page; this shows every audited control-plane
// action (task create/cancel/retry, policy decisions, enrollments, …).
export default function Audit() {
  const [rows, setRows] = useState<AuditEvent[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [draft, setDraft] = useState('');
  const [action, setAction] = useState('');

  const load = () => {
    listAudit(action || undefined, 100)
      .then((r) => {
        setError(null);
        setRows(r);
      })
      .catch((e) => setError(e as Error));
  };

  useEffect(load, [action]); // eslint-disable-line react-hooks/exhaustive-deps
  useLiveRefresh(load);

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
      {!rows && !error && <Loading />}
      {rows && rows.length === 0 && <p className="muted">No audit events{action ? ` for "${action}"` : ''}.</p>}
      {rows && rows.length > 0 && (
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
                <td>{fmtTime(r.created_at)}</td>
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
      )}
    </section>
  );
}
