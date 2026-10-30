import { Fragment, useEffect, useState } from 'react';
import {
  answerApproval,
  ApprovalView,
  ArtifactDownload,
  getArtifact,
  listApprovals,
  reqOk,
} from '../api';
import { ConfirmModal, PromptModal } from './Modal';
import PatchView from './PatchView';
import { ErrorBox, Loading, StatusBadge, TimeAgo, useLiveRefresh } from './util';
import { toast } from './Toast';

// Stage 9.2 operator approval UI: list pending approvals, allow/deny with a
// recorded reason. Refreshes on the control-plane change stream so a fresh
// request_permission surfaces without an operator refresh and idle pages
// make no requests; terminal approvals (allowed/denied/expired) are shown
// briefly for context then hidden on next fetch.
//
// Backlog item 8: a row can show the task's `changes.patch` inline. A pending
// patch-review approval ("Review required") is decided on exactly that diff, and
// leaving the queue to read it was the step operators skipped — so approving a
// push meant approving something they had not looked at. The diff is fetched on
// demand (one request per task, cached for the page's lifetime) and rendered
// read-only: annotations live on the task page, where the rework flow is.

export default function Approvals({ filter = 'pending' }: { filter?: string }) {
  const [items, setItems] = useState<ApprovalView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [bulkBusy, setBulkBusy] = useState(false);
  const [bulkConfirm, setBulkConfirm] = useState(false);
  const [pending, setPending] = useState<{ a: ApprovalView; decision: 'allow' | 'deny' } | null>(null);
  // Which approval's diff is open, and the fetched patches by task id.
  // `undefined` = not fetched yet, `null` = no artifact (attempt still running).
  const [openDiff, setOpenDiff] = useState<string | null>(null);
  const [diffs, setDiffs] = useState<Record<string, ArtifactDownload | null>>({});

  const load = () => {
    listApprovals(filter === 'all' ? undefined : filter)
      .then((items) => {
        setError(null);
        setItems(items);
      })
      .catch(setError);
  };
  useEffect(load, [filter]);
  useLiveRefresh(load);

  const toggleDiff = async (a: ApprovalView) => {
    if (openDiff === a.id) {
      setOpenDiff(null);
      return;
    }
    setOpenDiff(a.id);
    if (a.task_id in diffs) return;
    try {
      const patch = await getArtifact(a.task_id, 'changes.patch');
      setDiffs((d) => ({ ...d, [a.task_id]: patch }));
    } catch (e) {
      toast.error(e, 'Failed to load the diff');
      setDiffs((d) => ({ ...d, [a.task_id]: null }));
    }
  };

  const submit = async (reason: string) => {
    if (!pending) return;
    const { a, decision } = pending;
    setPending(null);
    setBusy(a.id);
    try {
      await reqOk(await answerApproval(a.id, decision, reason.trim() || undefined));
      toast.ok(decision === 'allow' ? `Allowed ${a.permission}` : `Denied ${a.permission}`);
      load();
    } catch (e) {
      toast.error(e, `Failed to ${decision} approval`);
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  const approveAll = async () => {
    setBulkConfirm(false);
    if (!items) return;
    setBulkBusy(true);
    try {
      // Sequential so a failing answer cannot mask the rest; sync per item.
      for (const a of items) {
        await reqOk(await answerApproval(a.id, 'allow'));
      }
      toast.ok(`Approved ${items.length} approvals`);
      load();
    } catch (e) {
      toast.error(e, 'Bulk approve failed partway');
      setError(e);
      load();
    } finally {
      setBulkBusy(false);
    }
  };

  if (!items) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Approvals{filter !== 'all' && ` — ${filter}`}</h2>
      {filter === 'pending' && items.length > 1 && (
        <p>
          <button disabled={bulkBusy} onClick={() => setBulkConfirm(true)}>
            {bulkBusy ? 'Approving…' : `Allow all ${items.length}`}
          </button>{' '}
          <span className="muted">approves every pending item, one by one</span>
        </p>
      )}
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
              <Fragment key={a.id}>
                <tr>
                  <td data-h="Status"><StatusBadge status={a.status} /></td>
                  <td data-h="Scope">{a.scope}</td>
                  <td data-h="Permission" className="mono">{a.permission}</td>
                  <td data-h="Task" className="mono"><a href={`#/task/${a.task_id}`}>{a.task_id.slice(0, 8)}</a></td>
                  <td data-h="Attempt" className="mono">{a.attempt_id.slice(0, 8)}</td>
                  <td data-h="Created"><TimeAgo s={a.created_at} /></td>
                  <td data-h="Expires"><TimeAgo s={a.expires_at} /></td>
                  <td data-h="Reason">{a.reason || '—'}</td>
                  <td data-h="Action">
                    <div className="approvals-actions">
                      <button
                        disabled={busy === a.id}
                        aria-label={`Show the diff for task ${a.task_id}`}
                        aria-expanded={openDiff === a.id}
                        onClick={() => toggleDiff(a)}
                      >{openDiff === a.id ? 'Hide diff' : 'Diff'}</button>{' '}
                      {a.status === 'pending' && (
                        <>
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
                        </>
                      )}
                    </div>
                  </td>
                </tr>
                {openDiff === a.id && (
                  <tr className="diff-row">
                    <td colSpan={9} data-h="Diff">
                      {(() => {
                        const p = diffs[a.task_id];
                        if (p === undefined) return <Loading />;
                        if (p === null) {
                          return (
                            <p className="muted">
                              No <code>changes.patch</code> for this task yet — the
                              attempt may still be running. Open the{' '}
                              <a href={`#/task/${a.task_id}`}>task</a> for its live
                              event trail.
                            </p>
                          );
                        }
                        return (
                          <>
                            {p.sha256 && (
                              <p className="muted mono">
                                sha256: {p.sha256.slice(0, 16)}…{' '}
                                <a
                                  className="link"
                                  href={`/v1/tasks/${a.task_id}/artifacts/changes.patch`}
                                  download
                                >Download</a>
                              </p>
                            )}
                            <pre className="patch">
                              <PatchView text={p.text} />
                            </pre>
                          </>
                        );
                      })()}
                    </td>
                  </tr>
                )}
              </Fragment>
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
      {bulkConfirm && (
        <ConfirmModal
          title="Approve all pending"
          body={`Approve ${items?.length ?? 0} pending permission requests? This cannot be undone.`}
          confirmLabel="Approve all"
          onConfirm={approveAll}
          onCancel={() => setBulkConfirm(false)}
        />
      )}
    </section>
  );
}
