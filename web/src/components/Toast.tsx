import { useEffect, useState } from 'react';

// Lightweight global toast bus. Views keep their inline ErrorBox for load
// failures, but transient action results (approve/deny, revoke, save…)
// surface as dismissible toasts in the corner instead of a silent pass.

export interface Toast {
  id: number;
  kind: 'ok' | 'err' | 'info';
  text: string;
}

let nextId = 1;
let listeners: ((t: Toast) => void)[] = [];

function emit(kind: Toast['kind'], text: string) {
  const t: Toast = { id: nextId++, kind, text };
  for (const l of listeners) l(t);
}

export const toast = {
  ok: (text: string) => emit('ok', text),
  err: (text: string) => emit('err', text),
  info: (text: string) => emit('info', text),
  /** Report an unknown error object with a sensible fallback message. */
  error: (e: unknown, fallback: string) =>
    emit('err', e instanceof Error && e.message ? e.message : fallback),
};

export function ToastHost() {
  const [items, setItems] = useState<Toast[]>([]);

  useEffect(() => {
    const on = (t: Toast) => {
      setItems((cur) => [...cur, t]);
      // Auto-dismiss successes/info quickly; errors stay until clicked.
      if (t.kind !== 'err') {
        setTimeout(() => {
          setItems((cur) => cur.filter((x) => x.id !== t.id));
        }, 3500);
      }
    };
    listeners.push(on);
    return () => {
      listeners = listeners.filter((l) => l !== on);
    };
  }, []);

  const dismiss = (id: number) =>
    setItems((cur) => cur.filter((x) => x.id !== id));

  if (!items.length) return null;
  return (
    <div className="toasts" role="status" aria-live="polite">
      {items.map((t) => (
        <div key={t.id} className={`toast ${t.kind}`}>
          <span>{t.text}</span>
          <button
            className="toast-close"
            aria-label="Dismiss notification"
            onClick={() => dismiss(t.id)}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  );
}
