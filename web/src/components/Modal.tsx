import { ReactNode, useEffect, useRef, useState } from 'react';

// Plan 3.1: in-app modals replacing the native browser dialogs. Esc or
// clicking the overlay closes; Enter in the input submits.

export function Modal({
  title,
  body,
  actions,
  onClose,
}: {
  title: string;
  body?: ReactNode;
  actions: ReactNode;
  onClose: () => void;
}) {
  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', h);
    return () => window.removeEventListener('keydown', h);
  }, [onClose]);

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal" role="dialog" aria-modal="true" onClick={(e) => e.stopPropagation()}>
        <h3>{title}</h3>
        {body && <div className="modal-body">{body}</div>}
        <div className="modal-actions">{actions}</div>
      </div>
    </div>
  );
}

export function ConfirmModal({
  title,
  body,
  confirmLabel = 'Confirm',
  danger = false,
  onConfirm,
  onCancel,
}: {
  title: string;
  body?: ReactNode;
  confirmLabel?: string;
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <Modal
      title={title}
      body={body}
      onClose={onCancel}
      actions={
        <>
          <button className={danger ? 'danger' : undefined} onClick={onConfirm} autoFocus>
            {confirmLabel}
          </button>
          <button className="secondary" onClick={onCancel}>
            Cancel
          </button>
        </>
      }
    />
  );
}

export function PromptModal({
  title,
  label,
  initialValue = '',
  submitLabel = 'Submit',
  required = false,
  onSubmit,
  onCancel,
}: {
  title: string;
  label?: string;
  initialValue?: string;
  submitLabel?: string;
  required?: boolean;
  onSubmit: (value: string) => void;
  onCancel: () => void;
}) {
  const [value, setValue] = useState(initialValue);
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    ref.current?.focus();
  }, []);
  const valid = !required || value.trim() !== '';
  const submit = () => {
    if (valid) onSubmit(value);
  };

  return (
    <Modal
      title={title}
      body={
        <label>
          {label}
          <input
            ref={ref}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') submit();
            }}
          />
        </label>
      }
      onClose={onCancel}
      actions={
        <>
          <button disabled={!valid} onClick={submit}>
            {submitLabel}
          </button>
          <button className="secondary" onClick={onCancel}>
            Cancel
          </button>
        </>
      }
    />
  );
}

/** Multiline variant of PromptModal (Ctrl/Cmd+Enter submits). */
export function TextAreaModal({
  title,
  label,
  submitLabel = 'Submit',
  onSubmit,
  onCancel,
}: {
  title: string;
  label?: string;
  submitLabel?: string;
  onSubmit: (value: string) => void;
  onCancel: () => void;
}) {
  const [value, setValue] = useState('');
  const ref = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    ref.current?.focus();
  }, []);
  const valid = value.trim() !== '';
  const submit = () => {
    if (valid) onSubmit(value);
  };

  return (
    <Modal
      title={title}
      body={
        <label>
          {label}
          <textarea
            ref={ref}
            rows={5}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) submit();
            }}
          />
        </label>
      }
      onClose={onCancel}
      actions={
        <>
          <button disabled={!valid} onClick={submit}>
            {submitLabel}
          </button>
          <button className="secondary" onClick={onCancel}>
            Cancel
          </button>
        </>
      }
    />
  );
}
