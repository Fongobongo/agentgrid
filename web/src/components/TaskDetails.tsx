import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  addAnnotation,
  answerApproval,
  ApiError,
  ApprovalView,
  ArtifactDownload,
  AttemptView,
  cancelTask,
  CreateAnnotationRequest,
  getArtifact,
  getEligibility,
  getTask,
  getTaskEvents,
  getTaskReviewApproval,
  listAnnotations,
  PatchAnnotation,
  retryTask,
  reworkAttempt,
  showAttempt,
  streamTask,
  TaskEligibility,
  TaskEvent,
  TaskView,
} from '../api';
import { parsePatchLines } from '../patch';
import { ErrorBox, Loading, StatusBadge, fmtTime } from './util';
import { TextAreaModal } from './Modal';

function eventText(e: TaskEvent): string {
  const p = e.payload ?? {};
  return (
    p.text ?? p.content ?? p.message ?? (typeof p.status === 'string' ? p.status : JSON.stringify(p))
  );
}

const TERMINAL = ['succeeded', 'failed', 'cancelled'];

export default function TaskDetails({ taskId }: { taskId: string }) {
  const [task, setTask] = useState<TaskView | null>(null);
  const [elig, setElig] = useState<TaskEligibility | null>(null);
  const [events, setEvents] = useState<TaskEvent[]>([]);
  const [paused, setPaused] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [patch, setPatch] = useState<ArtifactDownload | null | undefined>(undefined);
  const [validationLog, setValidationLog] = useState<ArtifactDownload | null | undefined>(undefined);
  const [busy, setBusy] = useState<string | null>(null);
  // Competitor plan 1.1 (diff review): pending patch-review approval, if any.
  const [reviewApproval, setReviewApproval] = useState<ApprovalView | null>(null);
  // Competitor-gap feature: inline annotations on the latest attempt's diff
  // + the modal state for composing one on a clicked line.
  const [annotations, setAnnotations] = useState<PatchAnnotation[]>([]);
  const [annotating, setAnnotating] = useState<{ file: string; line: number } | null>(null);
  // Competitor-gap feature (convergence metrics): attempt detail for the
  // validation-rounds counter.
  const [attemptDetail, setAttemptDetail] = useState<AttemptView | null>(null);

  const logRef = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);

  useEffect(() => {
    getTask(taskId).then(setTask).catch(setError);
    getEligibility(taskId).then(setElig).catch(() => {});
  }, [taskId]);

  // Initial history, then live stream with automatic reconnect/resume. The
  // stream opens only after the history resolves, seeded with the history's
  // max ingest_id — starting both from 0 double-delivered every event.
  useEffect(() => {
    setEvents([]);
    let cancelled = false;
    let handle: { close: () => void } | null = null;
    getTaskEvents(taskId, 0)
      .then((hist) => {
        if (cancelled) return;
        setEvents(hist);
        const after = hist.reduce((m, e) => Math.max(m, e.ingest_id || 0), 0);
        handle = streamTask(taskId, {
          after,
          onEvent: (e) => {
            setEvents((prev) => {
              if (e.ingest_id && prev.some((p) => p.ingest_id === e.ingest_id)) {
                return prev;
              }
              const next = prev.length > 5000 ? prev.slice(prev.length - 4000) : prev.slice();
              next.push(e);
              return next;
            });
            // Audit X-W3 (fixed shape): status events carry OBJECT payloads
            // (`{"status":"validating",...}` from the node, permission
            // decisions, etc.) — the string check here could never fire, so a
            // task completing while the page stayed open never refreshed and
            // the diff/review sections never appeared without a reload. Any
            // terminal-status event refetches the task; the guard below
            // keeps an already-terminal view stable.
            const st = e.payload && typeof e.payload === 'object' ? (e.payload as { status?: unknown }).status : undefined;
            if (e.type === 'status' && typeof st === 'string' && TERMINAL.includes(st)) {
              getTask(taskId)
                .then((t) =>
                  setTask((cur) => (cur && TERMINAL.includes(cur.status) ? cur : t)),
                )
                .catch(() => {});
            }
          },
        });
      })
      .catch(setError);
    return () => {
      cancelled = true;
      handle?.close();
    };
  }, [taskId]);

  // Audit follow-up: the eligibility banner was fetched once on mount — a
  // queued task whose nodes come online kept showing "no eligible node"
  // until remount. Refetch while queued on every event batch.
  useEffect(() => {
    if (task?.status !== 'queued') return;
    getEligibility(taskId).then(setElig).catch(() => {});
  }, [taskId, task?.status, events.length]);

  // Fetch artifacts once the task is terminal.
  useEffect(() => {
    if (task && TERMINAL.includes(task.status)) {
      getArtifact(taskId, 'changes.patch').then(setPatch).catch(() => setPatch(null));
      getArtifact(taskId, 'validation.log').then(setValidationLog).catch(() => setValidationLog(null));
      // Competitor plan 1.1: look for a pending patch-review approval so the
      // UI can show approve/reject/rework buttons on the diff.
      getTaskReviewApproval(taskId).then(setReviewApproval).catch(() => setReviewApproval(null));
    }
  }, [task, taskId]);

  // Competitor-gap feature: load inline annotations for the latest attempt
  // once the terminal task id is known (attempt ids arrive via events).
  const latestAttemptId = useMemo(() => {
    let id = task?.assigned_attempt_id ?? null;
    for (const e of events) {
      if (e.attempt_id) {
        id = e.attempt_id;
      }
    }
    return id;
  }, [task?.assigned_attempt_id, events]);
  useEffect(() => {
    if (task && TERMINAL.includes(task.status) && latestAttemptId) {
      listAnnotations(latestAttemptId).then(setAnnotations).catch(() => setAnnotations([]));
      showAttempt(latestAttemptId).then(setAttemptDetail).catch(() => setAttemptDetail(null));
    } else {
      setAnnotations([]);
      setAttemptDetail(null);
    }
  }, [task, taskId, latestAttemptId]);

  const submitAnnotation = async (text: string) => {
    if (!annotating || !latestAttemptId) return;
    setBusy('annotate');
    try {
      const body: CreateAnnotationRequest = {
        file: annotating.file,
        line_start: annotating.line,
        line_end: annotating.line,
        comment: text,
      };
      const a = await addAnnotation(latestAttemptId, body);
      setAnnotations((prev) => [...prev, a]);
      setAnnotating(null);
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(null);
    }
  };

  // Autoscroll the log to the bottom unless paused.
  useLayoutEffect(() => {
    if (!paused && atBottom.current && logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight;
    }
  }, [events, paused]);

  const onScroll = () => {
    const el = logRef.current;
    if (!el) return;
    atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  };

  const act = async (kind: 'cancel' | 'retry') => {
    setBusy(kind);
    try {
      const r = kind === 'cancel' ? await cancelTask(taskId) : await retryTask(taskId);
      if (!r.ok) setError(new ApiError(r.status, `${kind} failed (${r.status})`));
      else getTask(taskId).then(setTask).catch(() => {});
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(null);
    }
  };

  // Competitor plan 1.1: operator decision on the patch-review approval.
  // - accept -> allow the approval, task stays succeeded (patch accepted)
  // - reject -> deny, human has seen the patch and rejected it
  // - rework -> deny + send the annotated attempt for rework (the backend
  //   folds all inline annotations into a fresh task's prompt)
  const reviewDecide = async (decision: 'accept' | 'reject' | 'rework') => {
    if (!reviewApproval) return;
    setBusy(decision);
    try {
      const deny = decision !== 'accept';
      const r = await answerApproval(
        reviewApproval.id,
        deny ? 'deny' : 'allow',
        decision === 'rework' ? 'operator requested rework' : undefined,
      );
      if (!r.ok) throw new ApiError(r.status, `review ${decision} failed (${r.status})`);
      if (decision === 'rework') {
        if (!latestAttemptId) throw new ApiError(404, 'rework failed: no attempt to annotate');
        const rr = await reworkAttempt(latestAttemptId);
        window.location.hash = `#/task/${rr.task_id}`;
        return;
      }
      setReviewApproval(null);
      getTask(taskId).then(setTask).catch(() => {});
    } catch (e) {
      setError(e as Error);
    } finally {
      setBusy(null);
    }
  };

  if (error) return <ErrorBox err={error} />;
  if (!task) return <Loading />;

  const logEvents = events.filter((e) =>
    ['stdout', 'stderr', 'result', 'error', 'tool'].includes(e.type),
  );
  const statusEvents = events
    .filter((e) => e.type === 'status')
    // Hardening P0 item 9: order status transitions by the global ingest
    // cursor so a retry's timeline reads correctly across attempts.
    .sort((a, b) => (a.ingest_id || a.sequence) - (b.ingest_id || b.sequence));

  // Attempts derived from event attempt_ids, in first-seen order.
  const attemptOrder: string[] = [];
  const attemptStatus: Record<string, string> = {};
  for (const e of events) {
    if (!attemptOrder.includes(e.attempt_id)) attemptOrder.push(e.attempt_id);
    if (e.type === 'status') {
      const s = e.payload?.status;
      if (typeof s === 'string') attemptStatus[e.attempt_id] = s;
    }
  }

  const canCancel = ['queued', 'assigned', 'running', 'validating'].includes(task.status);
  const canRetry = ['failed', 'cancelled'].includes(task.status);

  return (
    <div className="task-details">
      {annotating && (
        <TextAreaModal
          title={`Comment on ${annotating.file}:L${annotating.line}`}
          label="Comment (Ctrl/Cmd+Enter to submit):"
          submitLabel="Add comment"
          onSubmit={submitAnnotation}
          onCancel={() => setAnnotating(null)}
        />
      )}
      <div className="task-head">
        <h2>
          <StatusBadge status={task.status} /> {task.repository}
        </h2>
        <div className="actions">
          {canCancel && (
            <button className="danger" disabled={busy === 'cancel'} onClick={() => act('cancel')}>
              Cancel
            </button>
          )}
          {canRetry && (
            <button disabled={busy === 'retry'} onClick={() => act('retry')}>
              Retry
            </button>
          )}
        </div>
      </div>

      <div className="meta">
        <span><b>ID</b> {task.id}</span>
        <span><b>Adapter</b> {task.adapter}</span>
        <span><b>Validation</b> {task.validation_command ?? '—'}</span>
        <span><b>Created</b> {fmtTime(task.created_at)}</span>
        <span><b>Finished</b> {fmtTime(task.finished_at)}</span>
        {/* Hardening P2 item 36: the security profile the agent ran under. */}
        {task.security_profile && (
          <span><b>Security profile</b> {task.security_profile}</span>
        )}
        {/* Competitor-gap feature (convergence metrics): rework depth + how
            many feedback-loop rounds the attempt needed to converge. */}
        {task.rework_of && (
          <span><b>Rework of</b> {task.rework_of}</span>
        )}
        {(attemptDetail?.validation_rounds ?? 0) > 0 && (
          <span><b>Validation rounds</b> {attemptDetail?.validation_rounds}</span>
        )}
      </div>
      <div className="prompt-box"><b>Prompt:</b> {task.prompt}</div>

      {task.status === 'queued' && elig && elig.no_eligible_nodes.length > 0 && (
        <div className="error">
          <b>No eligible node:</b> {elig.no_eligible_nodes.join('; ')}
        </div>
      )}

      <div className="cols">
        <section className="col">
          <h3>Live output</h3>
          <div className="log-bar">
            <button onClick={() => setPaused((p) => !p)}>{paused ? 'Resume' : 'Pause'}</button>
            <span className="muted">{logEvents.length} lines</span>
          </div>
          <div className="log" ref={logRef} onScroll={onScroll}>
            {logEvents.length === 0 && <div className="muted">No output yet.</div>}
            {logEvents.map((e, i) => (
              <div key={i} className={`logline ${e.type}`}>
                {eventText(e)}
              </div>
            ))}
          </div>
        </section>

        <section className="col">
          <h3>Status timeline</h3>
          <ul className="timeline">
            {statusEvents.length === 0 && <li className="muted">No transitions yet.</li>}
            {statusEvents.map((e, i) => (
              <li key={i}>
                <StatusBadge status={e.payload?.status ?? e.type} /> {fmtTime(e.created_at)}
              </li>
            ))}
            <li>
              <StatusBadge status={task.status} /> current
            </li>
          </ul>

          <h3>Attempts</h3>
          {attemptOrder.length === 0 && <p className="muted">No attempts yet.</p>}
          <ul className="attempts">
            {attemptOrder.map((aid, i) => (
              <li key={aid}>
                #{i + 1} <code>{aid.slice(0, 8)}</code>{' '}
                {attemptStatus[aid] && <StatusBadge status={attemptStatus[aid]} />}
              </li>
            ))}
          </ul>

          {patch !== undefined && (
            <>
              <h3>Diff (changes.patch)</h3>
              {patch === null && <p className="muted">No diff artifact.</p>}
              {patch && (
                <>
                  {/* Hardening P2 item 36: integrity hash + attachment download. */}
                  {patch.sha256 && <p className="muted mono">sha256: {patch.sha256.slice(0, 16)}…</p>}
                  <a className="link" href={`/v1/tasks/${taskId}/artifacts/changes.patch`} download>
                    Download
                  </a>
                  {/* Competitor plan 1.1: review-actions on the diff. */}
                  {reviewApproval && (
                    <div className="review-bar">
                      <b>Review required</b>
                      <button disabled={busy === 'accept'} onClick={() => reviewDecide('accept')}>
                        Accept
                      </button>
                      <button
                        className="danger"
                        disabled={busy === 'reject'}
                        onClick={() => reviewDecide('reject')}
                      >
                        Reject
                      </button>
                      <button disabled={busy === 'rework'} onClick={() => reviewDecide('rework')}>
                        Request rework
                      </button>
                    </div>
                  )}
                  {/* Competitor-gap feature: inline annotations list. */}
                  {annotations.length > 0 && (
                    <div className="annotations">
                      <b>Inline comments</b>
                      {annotations.map((a) => (
                        <div key={a.id} className="annotation">
                          <span className="muted mono">
                            {a.file}
                            {a.line_start !== null && `:L${a.line_start}`}
                            {a.line_start !== null && a.line_end !== null && a.line_end !== a.line_start && `-${a.line_end}`}
                          </span>{' '}
                          {a.comment}
                        </div>
                      ))}
                    </div>
                  )}
                  <pre className="patch">{renderPatch(patch.text, setAnnotating)}</pre>
                </>
              )}
            </>
          )}

          {validationLog !== undefined && (
            <>
              <h3>Validation log</h3>
              {validationLog === null && <p className="muted">No validation log.</p>}
              {validationLog && (
                <>
                  {validationLog.sha256 && (
                    <p className="muted mono">sha256: {validationLog.sha256.slice(0, 16)}…</p>
                  )}
                  <a className="link" href={`/v1/tasks/${taskId}/artifacts/validation.log`} download>
                    Download
                  </a>
                  <pre className="vlog">{validationLog.text}</pre>
                </>
              )}
            </>
          )}
        </section>
      </div>
    </div>
  );
}

function renderPatch(
  patch: string,
  onAnnotate: (loc: { file: string; line: number }) => void,
) {
  const lines = parsePatchLines(patch);
  return lines.map((l, i) => {
    let cls = 'pl';
    if (l.kind === 'file' || l.kind === 'hunk') cls = 'ph';
    else if (l.kind === 'add') cls = 'pa';
    else if (l.kind === 'del') cls = 'pd';
    else if (l.kind === 'meta') cls = 'ph';
    const canComment = (l.kind === 'add' || l.kind === 'ctx') && l.newLine !== null && l.file !== '';
    return (
      <div
        key={i}
        className={cls + (canComment ? ' annotatable' : '')}
        title={canComment ? 'Click to comment on this line' : undefined}
        onClick={canComment ? () => onAnnotate({ file: l.file, line: l.newLine as number }) : undefined}
      >
        <span className="ln">{l.newLine ?? ''}</span>
        <span className="lt">
          {l.kind === 'hunk' || l.kind === 'file' || l.kind === 'meta' ? l.text : (l.kind === 'add' ? '+' : l.kind === 'del' ? '-' : ' ') + (l.text || ' ')}
        </span>
      </div>
    );
  });
}
