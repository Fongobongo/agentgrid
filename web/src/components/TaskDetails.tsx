import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import {
  ApiError,
  cancelTask,
  getArtifact,
  getEligibility,
  getTask,
  getTaskEvents,
  retryTask,
  streamTask,
  TaskEligibility,
  TaskEvent,
  TaskView,
} from '../api';
import { ErrorBox, Loading, StatusBadge, fmtTime } from './util';

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
  const [patch, setPatch] = useState<string | null | undefined>(undefined);
  const [validationLog, setValidationLog] = useState<string | null | undefined>(undefined);
  const [busy, setBusy] = useState<string | null>(null);

  const logRef = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);

  useEffect(() => {
    getTask(taskId).then(setTask).catch(setError);
    getEligibility(taskId).then(setElig).catch(() => {});
  }, [taskId]);

  // Initial history, then live stream with automatic reconnect/resume.
  useEffect(() => {
    let last = 0;
    setEvents([]);
    getTaskEvents(taskId, 0)
      .then((hist) => {
        setEvents(hist);
        last = hist.reduce((m, e) => Math.max(m, e.sequence), 0);
      })
      .catch(setError);
    const handle = streamTask(taskId, {
      onEvent: (e) => {
        setEvents((prev) => {
          const next = prev.length > 5000 ? prev.slice(prev.length - 4000) : prev.slice();
          next.push(e);
          return next;
        });
      },
    });
    return () => handle.close();
  }, [taskId]);

  // Fetch artifacts once the task is terminal.
  useEffect(() => {
    if (task && TERMINAL.includes(task.status)) {
      getArtifact(taskId, 'changes.patch').then(setPatch).catch(() => setPatch(null));
      getArtifact(taskId, 'validation.log').then(setValidationLog).catch(() => setValidationLog(null));
    }
  }, [task, taskId]);

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

  if (error) return <ErrorBox err={error} />;
  if (!task) return <Loading />;

  const logEvents = events.filter((e) =>
    ['stdout', 'stderr', 'result', 'error', 'tool'].includes(e.type),
  );
  const statusEvents = events
    .filter((e) => e.type === 'status')
    .sort((a, b) => a.sequence - b.sequence);

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
              {patch && <pre className="patch">{renderPatch(patch)}</pre>}
            </>
          )}

          {validationLog !== undefined && (
            <>
              <h3>Validation log</h3>
              {validationLog === null && <p className="muted">No validation log.</p>}
              {validationLog && <pre className="vlog">{validationLog}</pre>}
            </>
          )}
        </section>
      </div>
    </div>
  );
}

function renderPatch(patch: string) {
  return patch.split('\n').map((line, i) => {
    let cls = 'pl';
    if (line.startsWith('+++') || line.startsWith('---')) cls = 'ph';
    else if (line.startsWith('@@')) cls = 'ph';
    else if (line.startsWith('+')) cls = 'pa';
    else if (line.startsWith('-')) cls = 'pd';
    return (
      <div key={i} className={cls}>
        {line || ' '}
      </div>
    );
  });
}
