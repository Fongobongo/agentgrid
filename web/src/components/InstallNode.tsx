import { useEffect, useRef, useState } from 'react';
import {
  ApiError,
  getInstallJob,
  InstallJob,
  InstallNodeRequest,
  joinCommand as requestJoinCommand,
  JoinCommandView,
  NodeView,
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

/**
 * The pull-based join, as a shell line.
 *
 * Kept as a pure function even though the control plane now builds the real
 * command: the two forms have to agree, and this is the string the docs and
 * `deploy/join-node.sh` show. `relayed` reproduces the one difference — a join
 * through a sibling spells out `--server`, because the script's own default is
 * `AGENTGRID_PUBLIC_URL`, which is the address the host cannot reach.
 */
export function joinCommand(server: string, token: string, relayed = false): string {
  const base = (server || 'https://cp.example.com').replace(/\/+$/, '');
  const server_arg = relayed ? ` --server ${base}` : '';
  return `curl -fsSL ${base}/v1/node/join.sh | sudo bash -s -- --token ${quote(token)}${server_arg}`;
}

/**
 * "Or pull it from here instead."
 *
 * Rendered in both modes: the join does not depend on
 * `AGENTGRID_PROVISION_ENABLED`, because nothing on this side reaches into the
 * host — the host reaches here, which it has to do anyway to work.
 *
 * The relay picker is the whole reason this calls the server instead of
 * formatting a string: a host behind a firewall cannot fetch the script from a
 * control plane it cannot reach, so the line it runs points at a sibling node
 * that can carry the traffic. Only the control plane knows which nodes
 * announced one, so the command is built there.
 *
 * The token is minted on demand rather than on mount so a modal left open does
 * not leave a live credential in the DOM after its ten minutes.
 */
function JoinSection({
  server,
  name,
  nodes,
}: {
  server: string;
  name: string;
  nodes: NodeView[];
}) {
  const [via, setVia] = useState('');
  const [view, setView] = useState<JoinCommandView | null>(null);
  const [err, setErr] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  // A node with no announced address cannot carry a join, so it is not offered.
  // A node that listens and announces nothing is *also* not offered, but it is
  // named below instead of being silently absent: the fix is a different
  // variable, and "no relay here" would send the operator to set the one they
  // already set.
  const relays = nodes.filter((n) => n.relay_url);
  const silentRelays = nodes.filter((n) => !n.relay_url && n.relay_listen);

  const mint = async () => {
    setBusy(true);
    setErr(null);
    try {
      setView(
        await requestJoinCommand({
          name: name.trim() || 'remote-node',
          via: via || undefined,
        }),
      );
    } catch (e) {
      setErr(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{ marginTop: 16 }}>
      <strong>Or join from the new host</strong>
      <p className="muted">
        Nothing on this side connects to the host. Mint a one-time token — it
        expires in ten minutes and can be spent once — and run this on the host
        itself. It fetches the daemon from this control plane, verifies every
        byte against the bundle&apos;s SHA-256 and starts it.
      </p>
      <label>
        Reach this control plane through
        <select
          value={via}
          disabled={busy}
          onChange={(e) => {
            setVia(e.target.value);
            // The old command names the old route; keeping it on screen after
            // the picker changed is how an operator pastes the wrong one.
            setView(null);
          }}
        >
          <option value="">directly (the host can reach {server || 'this URL'})</option>
          {relays.map((n) => (
            <option key={n.id} value={n.id}>
              {n.name} — {(n.relay_url ?? '').replace(/^https?:\/\//, '')}
              {n.status === 'online' ? '' : ` (${n.status})`}
            </option>
          ))}
        </select>
      </label>
      {relays.length === 0 && (
        <p className="muted">
          {silentRelays.length > 0 ? (
            <>
              {silentRelays.map((n) => n.name).join(', ')}{' '}
              {silentRelays.length === 1 ? 'listens' : 'listen'} for relay traffic
              but {silentRelays.length === 1 ? 'announces' : 'announce'} no address
              anyone can dial. Set <code>AGENTGRID_RELAY_ADVERTISE</code> on{' '}
              {silentRelays.length === 1 ? 'that node' : 'those nodes'} to the
              address other hosts should dial, and let it beat once.
            </>
          ) : (
            <>
              No node announces a relay address yet. On a node that can reach
              both this control plane and the new host, set{' '}
              <code>AGENTGRID_RELAY_LISTEN=0.0.0.0:7900</code> and{' '}
              <code>AGENTGRID_RELAY_ADVERTISE=http://&lt;that-host&gt;:7900</code>,
              restart it, and it will appear here.
            </>
          )}
        </p>
      )}
      {err ? <ErrorBox err={err} /> : null}
      {view ? (
        <>
          <pre style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>{view.command}</pre>
          <div className="muted">
            {view.via
              ? `Through ${view.via.name} (${view.via.relay_url}) — the host dials that node, not this one. `
              : ''}
            <button onClick={() => navigator.clipboard?.writeText(view.command).catch(() => undefined)}>
              copy join command
            </button>{' '}
            <button onClick={() => setView(null)}>hide</button>
          </div>
          {/* A revoked relay is refused by the control plane, so this is only
              ever "offline"/"degraded": the command is still built, because the
              node may be back by the time it is pasted. Saying nothing would be
              the worse choice — the line is for a host nobody can log into. */}
          {view.via && view.via.status !== 'online' ? (
            <p className="muted">
              <span className="badge warn">{view.via.status}</span> That relay last
              reached this control plane at{' '}
              {view.via.last_heartbeat_at || 'never'}. Add <code>--probe</code> to
              the line above to stop before installing anything if it is not
              relaying.
            </p>
          ) : null}
        </>
      ) : (
        <button disabled={busy} onClick={mint}>
          {busy ? 'minting…' : 'get join command'}
        </button>
      )}
    </div>
  );
}

export default function InstallNode({
  info,
  nodes = [],
  onClose,
  onInstalled,
}: {
  info: ProvisioningInfo | null;
  /** The fleet, for the relay picker. Absent = no relay is offered. */
  nodes?: NodeView[];
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
            <JoinSection server={f.server} name={f.name} nodes={nodes} />
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
      <JoinSection server={f.server} name={f.name} nodes={nodes} />
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
