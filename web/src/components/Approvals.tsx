import { useEffect, useState } from 'react';
import { answerApproval, ApprovalView, listApprovals } from '../api';
import { PromptModal } from './Modal';
import { ErrorBox, Loading, StatusBadge, fmtTime } from './util';

// Stage 9.2 operator approval UI: list pending approvals, allow/deny with a
// recorded reason. Auto-polls every 3s so a fresh request_permission surfaces
// without an operator refresh; terminal approvals (allowed/denied/expired)
// are shown briefly for context then hidden on next fetch.

const POLL_MS = 3000;

export default function Approvals({ filter = 'pending' }: { filter?: string }) {
  const [items, setItems] = useState<ApprovalView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [pending, setPending] = useState<{ a: ApprovalView; decision: 'allow' | 'deny' } | null>(null);

  const load = () => {
    listApprovals(filter === 'all' ? undefined : filter)
      .then((items) => {
        setError(null);
        setItems(items);
      })
      .catch(setError);
  };
  useEffect(() => {
    load();
    const t = setInterval(load, POLL_MS);
    return () => clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filter]);

  const submit = async (reason: string) => {
    if (!pending) return;
    const { a, decision } = pending;
    setPending(null);
    setBusy(a.id);
    try {
      const r = await answerApproval(a.id, decision, reason.trim() || undefined);
      if (!r.ok) setError(new Error(`${decision} failed (${r.status})`));
      else load();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  if (!items) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Approvals{filter !== 'all' && ` — ${filter}`}</h2>
      {error ? <ErrorBox err={error} /> : null}
      {items.length === 0 ? (
        <div className="muted">No {filter} approvals.</div>
      ) : (
        <table className="grid approvals-table">
          <thead>
            <tr>
              <th>Status</th>
              <th>Scope</th>
              <th>Permission</th>
              <th>Task</th>
              <th>Attempt</th>
              <th>Created</th>
              <th>Expires</th>
              <th>Reason</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((a) => (
              <tr key={a.id}>
                <td data-h="Status"><StatusBadge status={a.status} /></td>
                <td data-h="Scope">{a.scope}</td>
                <td data-h="Permission" className="mono">{a.permission}</td>
                <td data-h="Task" className="mono"><a href={`#/task/${a.task_id}`}>{a.task_id.slice(0, 8)}</a></td>
                <td data-h="Attempt" className="mono">{a.attempt_id.slice(0, 8)}</td>
                <td data-h="Created">{fmtTime(a.created_at)}</td>
                <td data-h="Expires">{fmtTime(a.expires_at)}</td>
                <td data-h="Reason">{a.reason || '—'}</td>
                <td data-h="Action">
                  {a.status === 'pending' && (
                    <div className="approvals-actions">
                      <button
                        className="ok"
                        disabled={busy === a.id}
                        aria-label={`Allow ${a.permission}`}
                        onClick={() => setPending({ a, decision: 'allow' })}
                      >Allow</button>{' '}
                      <button
                        className="danger"
                        disabled={busy === a.id}
                        aria-label={`Deny ${a.permission}`}
                        onClick={() => setPending({ a, decision: 'deny' })}
                      >Deny</button>
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {pending && (
        <PromptModal
          title={pending.decision === 'allow' ? 'Allow permission' : 'Deny permission'}
          label={
            pending.decision === 'allow'
              ? `Reason for allowing "${pending.a.permission}" (optional)`
              : `Reason for denying "${pending.a.permission}"`
          }
          initialValue={pending.decision === 'deny' ? 'denied by operator' : ''}
          submitLabel={pending.decision === 'allow' ? 'Allow' : 'Deny'}
          required={pending.decision === 'deny'}
          onSubmit={submit}
          onCancel={() => setPending(null)}
        />
      )}
    </section>
  );
}
