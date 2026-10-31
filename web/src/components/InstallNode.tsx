import { useEffect, useRef, useState } from 'react';
import {
  ApiError,
  getInstallJob,
  InstallJob,
  InstallNodeRequest,
  ProvisioningInfo,
  startNodeInstall,
} from '../api';
import { Modal } from './Modal';
import { ErrorBox } from './util';

/**
 * Install a node on a remote host, from the panel.
 *
 * Two modes, decided by the server:
 *  - `info.enabled` — the control plane was started with
 *    `AGENTGRID_PROVISION_ENABLED=1` and can run the install itself. The form
 *    posts to `/v1/node-installs` and the modal follows the job.
 *  - otherwise — the panel cannot reach into a host, so it shows the exact
 *    `ag nodes install` command to run instead, filled in from the same form.
 *    `info.reason` says why (feature off, or no daemon binary on the CP).
 *
 * The command is built by `installCommand`, a pure function, so the string the
 * operator copies is testable without a DOM.
 */
export interface InstallForm {
  host: string;
  name: string;
  adapters: string;
  repositories: string;
  max_concurrency: number;
  server: string;
  data_dir: string;
  ssh_key: string;
  /** OpenSSH certificate for `ssh_key` (CA-signed, short-lived). */
  ssh_cert: string;
  password: string;
  accept_new_host_key: boolean;
  host_key_fingerprint: string;
  allow_root: boolean;
}

/** Same defaults the server applies, so the two paths agree. */
export function defaultForm(): InstallForm {
  return {
    host: '',
    name: 'remote-node',
    adapters: 'mock',
    repositories: '*',
    max_concurrency: 2,
    // The node must dial an address *it* can reach; the browser's origin is
    // the best guess and is wrong behind a proxy, so it stays editable.
    server: typeof window === 'undefined' ? '' : window.location.origin,
    data_dir: '/var/lib/agentgrid',
    ssh_key: '',
    ssh_cert: '',
    password: '',
    accept_new_host_key: false,
    host_key_fingerprint: '',
    allow_root: false,
  };
}

function quote(v: string): string {
  // The CLI takes these verbatim; a `*` or a space must survive the shell.
  return /^[A-Za-z0-9._,/@:*-]+$/.test(v) ? v : `'${v.replace(/'/g, "'\\''")}'`;
}

/**
 * The `ag nodes install` line that reproduces what the button would do.
 *
 * `--server` is always passed: without it the CLI opens a reverse SSH tunnel to
 * the machine running the command, which is never what an operator copying a
 * command out of a control-plane panel wants.
 */
export function installCommand(f: InstallForm): string {
  const parts = ['ag nodes install', `--host ${quote(f.host || 'user@host')}`];
  if (f.name) parts.push(`--name ${quote(f.name)}`);
  if (f.adapters) parts.push(`--adapters ${quote(f.adapters)}`);
  if (f.repositories) parts.push(`--repositories ${quote(f.repositories)}`);
  parts.push(`--max-concurrency ${f.max_concurrency}`);
  if (f.server) parts.push(`--server ${quote(f.server)}`);
  if (f.data_dir) parts.push(`--data-dir ${quote(f.data_dir)}`);
  if (f.ssh_key) parts.push(`--ssh-key ${quote(f.ssh_key)}`);
  if (f.ssh_cert) parts.push(`--ssh-cert ${quote(f.ssh_cert)}`);
  if (f.host_key_fingerprint)
    parts.push(`--host-key-fingerprint ${quote(f.host_key_fingerprint)}`);
  if (f.accept_new_host_key) parts.push('--accept-new-host-key');
  if (f.allow_root) parts.push('--allow-root');
  return parts.join(' \\\n  ');
}

export default function InstallNode({
  info,
  onClose,
  onInstalled,
}: {
  info: ProvisioningInfo | null;
  onClose: () => void;
  onInstalled: () => void;
}) {
  const [f, setF] = useState<InstallForm>(defaultForm);
  const [job, setJob] = useState<InstallJob | null>(null);
  const [err, setErr] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  // Follow the job until it reaches a terminal state. Polling, not a stream:
  // an install is a handful of steps and lives for seconds.
  //
  // `onInstalled` goes through a ref: the parent passes an inline closure, and
  // a fresh identity on every render would tear down and re-arm the interval
  // each time the poll updates the job.
  const onInstalledRef = useRef(onInstalled);
  onInstalledRef.current = onInstalled;
  const jobId = job?.id;
  const jobState = job?.state;
  useEffect(() => {
    if (!jobId || jobState !== 'running') return;
    const t = setInterval(() => {
      getInstallJob(jobId)
        .then((j) => {
          setJob(j);
          if (j.state === 'succeeded') onInstalledRef.current();
        })
        .catch(() => {
          // A transient poll failure must not stop the follow-up; the next
          // tick retries and the job keeps running on the server.
        });
    }, 800);
    return () => clearInterval(t);
  }, [jobId, jobState]);

  const set = <K extends keyof InstallForm>(k: K, v: InstallForm[K]) =>
    setF((cur) => ({ ...cur, [k]: v }));

  const submit = async () => {
    setBusy(true);
    setErr(null);
    try {
      const body: InstallNodeRequest = {
        host: f.host.trim(),
        name: f.name.trim() || undefined,
        adapters: f.adapters.trim() || undefined,
        repositories: f.repositories.trim() || undefined,
        max_concurrency: f.max_concurrency,
        server: f.server.trim() || undefined,
        data_dir: f.data_dir.trim() || undefined,
        ssh_key: f.ssh_key.trim() || undefined,
        ssh_cert: f.ssh_cert.trim() || undefined,
        password: f.password || undefined,
        accept_new_host_key: f.accept_new_host_key,
        host_key_fingerprint: f.host_key_fingerprint.trim() || undefined,
        allow_root: f.allow_root,
      };
      const r = await startNodeInstall(body);
      setJob(await getInstallJob(r.job_id));
    } catch (e) {
      setErr(e);
    } finally {
      setBusy(false);
    }
  };

  const copy = (text: string) => {
    navigator.clipboard?.writeText(text).catch(() => undefined);
  };

  if (!info) {
    return (
      <Modal
        title="Install node"
        body={<p className="muted">checking what this control plane allows…</p>}
        actions={<button onClick={onClose}>Close</button>}
        onClose={onClose}
      />
    );
  }

  if (!info.enabled) {
    // No SSH from the control plane. Say why, and give the operator the one
    // command that does the same thing from wherever they can reach the host.
    return (
      <Modal
        title="Install node"
        body={
          <>
            <p>{info.reason}</p>
            <p className="muted">
              Run this on a machine that can reach the host. It copies the
              daemon, enrolls it with a one-shot token and starts it:
            </p>
            <pre style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>
              {installCommand(f)}
            </pre>
            <div className="muted">
              <button onClick={() => copy(installCommand(f))}>copy command</button>
            </div>
          </>
        }
        actions={<button onClick={onClose}>Close</button>}
        onClose={onClose}
      />
    );
  }

  const running = job?.state === 'running';
  const body = (
    <>
      {err ? <ErrorBox err={err} /> : null}
      <p className="muted">
        The control plane will ssh into the host, copy{' '}
        <code>agentgrid-node-daemon</code>, write a 0600 env file with a
        one-shot enrollment token and start the daemon. Credentials are used
        for this install only and are never stored.
      </p>
      <label>
        Host (user@host or user@host:port)
        <input
          value={f.host}
          placeholder="root@10.0.0.5"
          disabled={running || busy}
          onChange={(e) => set('host', e.target.value)}
        />
      </label>
      <label>
        Name
        <input
          value={f.name}
          disabled={running || busy}
          onChange={(e) => set('name', e.target.value)}
        />
      </label>
      <label>
        Server URL the node will dial
        <input
          value={f.server}
          placeholder="http://cp.example.com:7800"
          disabled={running || busy}
          onChange={(e) => set('server', e.target.value)}
        />
      </label>
      <label>
        Adapters
        <input
          value={f.adapters}
          disabled={running || busy}
          onChange={(e) => set('adapters', e.target.value)}
        />
      </label>
      <label>
        Repositories
        <input
          value={f.repositories}
          disabled={running || busy}
          onChange={(e) => set('repositories', e.target.value)}
        />
      </label>
      <label>
        Max concurrency
        <input
          type="number"
          min={1}
          value={f.max_concurrency}
          disabled={running || busy}
          onChange={(e) => set('max_concurrency', Number(e.target.value) || 1)}
        />
      </label>
      <details>
        <summary className="muted">Advanced (auth, paths, host-key policy)</summary>
        <label>
          SSH private key path on the control-plane host (empty = ssh agent)
          <input
            value={f.ssh_key}
            placeholder="/root/.ssh/id_ed25519"
            disabled={running || busy}
            onChange={(e) => set('ssh_key', e.target.value)}
          />
        </label>
        <label>
          SSH certificate for that key (empty = plain key auth)
          <input
            value={f.ssh_cert}
            placeholder="/root/.ssh/id_ed25519-cert.pub"
            disabled={running || busy}
            onChange={(e) => set('ssh_cert', e.target.value)}
          />
        </label>
        <label>
          SSH password (needs sshpass/expect on the control-plane host)
          <input
            type="password"
            value={f.password}
            disabled={running || busy}
            onChange={(e) => set('password', e.target.value)}
          />
        </label>
        <label>
          Data directory on the host
          <input
            value={f.data_dir}
            disabled={running || busy}
            onChange={(e) => set('data_dir', e.target.value)}
          />
        </label>
        <label>
          Pinned host-key fingerprint (SHA256:…)
          <input
            value={f.host_key_fingerprint}
            disabled={running || busy}
            onChange={(e) => set('host_key_fingerprint', e.target.value)}
          />
        </label>
        <label>
          <input
            type="checkbox"
            checked={f.accept_new_host_key}
            disabled={running || busy}
            onChange={(e) => set('accept_new_host_key', e.target.checked)}
          />{' '}
          Accept an unknown SSH host key (first connect to a fresh host)
        </label>
        <label>
          <input
            type="checkbox"
            checked={f.allow_root}
            disabled={running || busy}
            onChange={(e) => set('allow_root', e.target.checked)}
          />{' '}
          Allow the daemon to run as root (not recommended)
        </label>
      </details>
      {job ? (
        <div style={{ marginTop: 12 }}>
          <strong>
            {job.state === 'succeeded'
              ? `installed: ${job.name} → ${job.server_url}`
              : job.state === 'failed'
                ? 'install failed'
                : 'installing…'}
          </strong>
          <ol className="muted" style={{ fontSize: 12 }}>
            {job.steps.map((s, i) => (
              <li key={i}>{s}</li>
            ))}
          </ol>
          {job.error ? <ErrorBox err={new ApiError(0, job.error)} /> : null}
        </div>
      ) : null}
    </>
  );

  return (
    <Modal
      title="Install node"
      body={body}
      actions={
        <>
          <button onClick={onClose}>{job?.state === 'succeeded' ? 'Close' : 'Cancel'}</button>
          <button disabled={busy || running || !f.host.trim()} onClick={submit}>
            {running ? 'Installing…' : 'Install'}
          </button>
        </>
      }
      onClose={onClose}
    />
  );
}
