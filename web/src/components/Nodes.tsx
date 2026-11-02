import { useEffect, useState, Fragment } from 'react';
import { drainNode, getJson, getOpencodeAudit, getProvisioning, listNodes, NodeView, OpencodeAuditEntry, OpencodeProfile, ProvisioningInfo, listOpencodeProfiles, postJson, revokeNode, listInstallJobs, forgetInstallJob, InstallJob, InstallState, getClusterStatus, ClusterStatus, ClusterPeer } from '../api';
import InstallNode from './InstallNode';
import { ConfirmModal } from './Modal';
import { ErrorBox, Loading, StatusBadge, TimeAgo, fmtTime, useLiveRefresh } from './util';
import { toast } from './Toast';

interface AccountUsage {
  env: string;
  token_index: number;
  attempts: number;
  rate_limited: number;
}

/**
 * ADR 0029: how this node hands adapter credentials to the agent.
 *
 * The `NAME@gateway=<route>` marker lives in the *daemon's* environment, not in
 * the control plane's store (ADR 0028 keeps values off the CP), so the panel
 * cannot read the config — the node reports counts in its heartbeat and this is
 * the only place the split becomes visible.
 *
 * Three states, and the third is not the second: `undefined`/`null` means the
 * node has never reported (older binary, or it has not beaten since upgrading).
 * Rendering that as "clean" would be a claim the panel cannot make.
 */
export function AdapterEnvBadge({
  delivery,
}: {
  delivery?: { plain: number; gateway: number; exchange?: number } | null;
}) {
  if (!delivery) return null;
  // Older nodes do not send `exchange` at all, and "absent" must not read as
  // zero-of-a-third-kind — it just is not there.
  const exchange = delivery.exchange ?? 0;
  const total = delivery.plain + delivery.gateway + exchange;
  if (delivery.plain > 0) {
    return (
      <span
        className="badge warn"
        title={
          `${delivery.plain} of ${total} AGENTGRID_ADAPTER_ENV entries are handed ` +
          'to the agent as plain values, so the key is readable inside the sandbox (threat-model T13a). ' +
          'Write them NAME@gateway=<route> (a capability, the key never enters the sandbox) or ' +
          'NAME@exchange=<route> (a real short-lived token, for consumers that cannot be proxied), ' +
          'and set AGENTGRID_CRED_GATEWAY_URL.'
        }
      >
        {delivery.plain} plain key{delivery.plain === 1 ? '' : 's'}
      </span>
    );
  }
  // Exchanged entries are *not* the same promise as gateway ones: the sandbox
  // holds a real provider token that works from anywhere until it expires. They
  // are counted and shown apart so this badge never reports the safer mode's
  // wording for the less safe one.
  if (exchange > 0) {
    return (
      <span
        className="badge"
        title={
          `${exchange} of ${total} adapter-env entries are exchanged: the sandbox holds a real ` +
          'provider-issued token, scoped and expired by the provider itself (an hour, one ' +
          'repository, for a GitHub App installation). It is revoked when the attempt ends, but ' +
          'unlike a capability it does work outside the gateway until then.'
        }
      >
        {exchange} exchanged
      </span>
    );
  }
  if (delivery.gateway > 0) {
    return (
      <span
        className="badge ok"
        title={`All ${delivery.gateway} adapter-env entries are gateway-delivered: the sandbox holds per-attempt capabilities, not keys`}
      >
        {delivery.gateway} gateway
      </span>
    );
  }
  return null;
}

/**
 * ADR 0026 + ADR 0012: how this node reaches the control plane.
 *
 * Two independent facts, and the difference between them is the point:
 * `relay_hops` is **observed** by the control plane (a relay stamps the header
 * on every request it forwards, and replaces whatever the client sent, so a
 * node cannot claim fewer hops than it used); `egress_proxy` is **claimed** by
 * the node, because nothing on the receiving side can see a forward proxy.
 *
 * Rendering rules, all three of them deliberate:
 *   - nothing at all means "direct, and the node says no proxy" — the common
 *     case, and a badge on every row would be noise. The legend above the table
 *     says so, so "no badge" is not an unexplained blank;
 *   - `null` for both is **not** direct: it means no heartbeat has arrived
 *     since the columns existed (a row older than the migration) and is shown
 *     as "reach ?" rather than silently claimed as direct;
 *   - relay and proxy are separate badges, because a node can be both and one
 *     label would hide one of them.
 */
export function ReachBadge({
  relayHops,
  egressProxy,
}: {
  relayHops?: number | null;
  egressProxy?: boolean | null;
}) {
  const unknown = relayHops == null && egressProxy == null;
  if (unknown) {
    return (
      <span
        className="badge"
        title="No heartbeat since this control plane started recording how nodes reach it, so its path is unknown — not necessarily direct"
      >
        reach ?
      </span>
    );
  }
  return (
    <>
      {relayHops != null && relayHops > 0 && (
        <span
          className="badge warn"
          title={
            `This node's heartbeat crossed ${relayHops} relay(s) (ADR 0026), so it does not ` +
            'reach the control plane directly. Observed from the x-agentgrid-relay-hops header, ' +
            'not reported by the node — a node cannot tell a relay from a control plane. ' +
            'The first hop is whatever AGENTGRID_SERVER names; check AGENTGRID_RELAY_LISTEN ' +
            'and AGENTGRID_RELAY_ALLOW on the nodes that carry it.'
          }
        >
          via relay ×{relayHops}
        </span>
      )}
      {egressProxy === true && (
        <span
          className="badge"
          title={
            'The node reports that its egress proxy pool (ADR 0012) has an entry in rotation, ' +
            'so its traffic goes through a forward proxy. This is a node claim: the control ' +
            'plane cannot see a forward proxy. A pool whose every entry is dead falls back to ' +
            'direct egress and this badge disappears on the next heartbeat.'
          }
        >
          via proxy
        </span>
      )}
    </>
  );
}

/**
 * "This node can carry a join."
 *
 * The address is the node's own announcement, not something the control plane
 * checked — a relay exists to be reachable from where the control plane is not,
 * so the only party who can verify it is a host that dials it
 * (`agentgrid-node-daemon probe --relay`). Shown because it is the answer to
 * "which node do I use for a host behind a firewall": the same list the join
 * modal offers as `--via`.
 *
 * The second state is the one that used to be invisible: a node that *listens*
 * but announces nothing. It is running a relay that will never be used, and it
 * rendered exactly like a node that is not a relay at all — so the operator was
 * told to set a variable they had already set.
 */
export function RelayBadge({ url, listen }: { url?: string | null; listen?: string | null }) {
  if (!url) {
    if (!listen) return null;
    return (
      <span
        className="badge warn"
        title={
          `This node listens for relay traffic on ${listen} but announces no address ` +
          'anyone can dial, so no join can go through it. A 0.0.0.0 bind means "every ' +
          'interface on this machine", which is not an address the joining host can use: ' +
          'set AGENTGRID_RELAY_ADVERTISE to the address other hosts should dial, and let ' +
          'it beat once.'
        }
      >
        relay: no advertise
      </span>
    );
  }
  return (
    <span
      className="badge ok"
      title={
        `This node relays other nodes' control-plane traffic at ${url}, and announced that ` +
        'address itself (AGENTGRID_RELAY_LISTEN / AGENTGRID_RELAY_ADVERTISE). A host that ' +
        'cannot reach this control plane can be joined through it: ag nodes add --via ' +
        '<this node>. The control plane cannot verify the address — it is reachable from ' +
        'places this control plane is not, which is the point.'
      }
    >
      relay
    </span>
  );
}

/**
 * Fleet reachability at a glance: what exists, and how much of it is answering.
 *
 * `revoked` is counted separately from `offline` on purpose — one is "we cut it
 * off", the other is "it stopped talking", and an operator chasing a gap needs
 * to know which.
 */
export function FleetSummary({ nodes }: { nodes: NodeView[] }) {
  const count = (s: string) => nodes.filter((n) => n.status === s).length;
  const online = count('online');
  const degraded = count('degraded');
  const offline = count('offline');
  const revoked = count('revoked');
  const pending = count('pending');
  const total = nodes.length;
  // "Reachable" = the control plane is hearing from it. `degraded` counts as
  // reachable: it is answering, just not fully usable.
  const reachable = online + degraded;
  const relayed = nodes.filter((n) => (n.relay_hops ?? 0) > 0).length;
  const proxied = nodes.filter((n) => n.egress_proxy === true).length;
  const unknown = nodes.filter((n) => n.relay_hops == null && n.egress_proxy == null).length;

  if (total === 0) {
    return (
      <p className="muted">
        No nodes are enrolled. A host is only listed here once it enrolls — a host that was
        configured but never came up leaves no row.
      </p>
    );
  }
  return (
    <p>
      <strong>
        {reachable}/{total} reachable
      </strong>
      {degraded > 0 && <> · {degraded} degraded</>}
      {offline > 0 && <> · <span className="badge err">{offline} offline</span></>}
      {pending > 0 && <> · {pending} pending</>}
      {revoked > 0 && <> · <span className="badge">{revoked} revoked</span></>}
      {relayed > 0 && <> · {relayed} via relay</>}
      {proxied > 0 && <> · {proxied} via proxy</>}
      {unknown > 0 && <> · {unknown} path unknown</>}
      <br />
      <span className="muted">
        No reach badge = direct (0 relay hops, no egress proxy). A node flips to{' '}
        <em>offline</em> after 30 s without a heartbeat; its last contact is in the Heartbeat
        column.
      </span>
    </p>
  );
}

/**
 * Hosts this control plane was asked to turn into nodes, whether or not they
 * ever arrived.
 *
 * This is the only view of a host that was **configured but is not available**:
 * the node table above is keyed by enrollment, so a host whose install failed,
 * or whose daemon never reached the control plane, has no row there at all.
 *
 * Two things make the list complete rather than partial: the rows are stored
 * (`node_installs`), so a restart does not empty it, and an install that runs
 * elsewhere — `ag nodes install` from an operator's laptop, the install script
 * — leaves an `issued` row because the enrollment-token mint names the host.
 * What is still not here: a host enrolled by hand with a bare `ag token
 * create`, which by definition never named one.
 *
 * The `Enrolled?` column is the cross-reference that makes the list useful:
 * `succeeded` only means ssh finished, and the question an operator has is
 * whether the daemon is now talking to us.
 */
export function installBadgeClass(state: InstallState): string {
  switch (state) {
    case 'failed':
      return 'badge err';
    case 'issued':
      return 'badge warn';
    case 'succeeded':
      return 'badge ok';
    default:
      return 'badge';
  }
}

/**
 * `issued` is not a failure and must not be read as one: it is the state of a
 * host whose install never reported back, which is exactly the case this table
 * exists to surface.
 */
export function installBadgeTitle(state: InstallState): string | undefined {
  if (state === 'issued') {
    return 'An enrollment token was minted for this host and nothing has enrolled since — the install has not run yet, or it stopped before the daemon started';
  }
  return undefined;
}

export function peerBadgeClass(state: ClusterPeer['state']): string {
  switch (state) {
    case 'up':
      return 'badge ok';
    case 'stale':
      return 'badge warn';
    case 'down':
      return 'badge err';
    default:
      // `never` is normal on a standby: nothing there probes anyone. Painting
      // it red would make a healthy cluster look broken.
      return 'badge';
  }
}

/** Seconds as a person reads them; `never` when there has been no contact. */
export function peerAgeLabel(secs: number | null): string {
  if (secs === null) return 'never';
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  return `${Math.floor(secs / 3600)}h ago`;
}

/**
 * HA peers, as seen from the replica serving this page.
 *
 * The view is deliberately local: only the leader fans out to every peer, so a
 * standby legitimately knows about one. Rendering a cluster-wide picture from
 * here would be an invention; saying "seen from X" is not.
 */
export function ClusterPeers({ status }: { status: ClusterStatus | null }) {
  if (!status || status.peers.length === 0) return null;
  return (
    <section>
      <h3>Control-plane peers</h3>
      <p className="muted">
        This replica is <strong>{status.role}</strong> in term {status.term}
        {status.leader_id ? (
          <>
            {' '}· leader <code>{status.leader_id}</code>
          </>
        ) : null}
        . Liveness comes from the consensus exchanges the protocol already runs, not from a
        separate probe — so these are the peers <em>this</em> replica has actually talked to. A
        peer that has never answered is still listed: unreachable and nonexistent are different
        problems.
      </p>
      <table className="grid">
        <thead>
          <tr>
            <th>Peer</th>
            <th>Role</th>
            <th>State</th>
            <th>Last seen</th>
            <th>Applied seq</th>
          </tr>
        </thead>
        <tbody>
          {status.peers.map((p) => (
            <tr key={p.id}>
              <td>
                <code>{p.id}</code>
                {p.url ? <span className="muted"> {p.url}</span> : null}
              </td>
              <td>{p.role}</td>
              <td>
                <span className={peerBadgeClass(p.state)}>{p.state}</span>
              </td>
              <td>{peerAgeLabel(p.age_secs)}</td>
              <td>{p.applied_seq ?? '—'}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

export function InstallAttempts({
  jobs,
  nodes,
  onForget,
  forgetting,
}: {
  jobs: InstallJob[];
  nodes: NodeView[];
  /**
   * Delete the record of a host that will never arrive. Omitted by the tests
   * that only render the table, which is why the action column is conditional:
   * a button that cannot do anything should not be drawn.
   */
  onForget?: (id: string) => void;
  /** The id currently being deleted, so its button can say so. */
  forgetting?: string | null;
}) {
  if (jobs.length === 0) return null;
  const enrolled = (name: string) => nodes.find((n) => n.name === name);
  return (
    <section>
      <h3>Install attempts</h3>
      <p className="muted">
        Every host this control plane has seen an install for. Stored, so a restart does not forget
        them. <code>issued</code> means only that an enrollment token went out — the install ran
        somewhere else (an operator&apos;s laptop, a shell script) and nothing has been heard since;
        those rows are how a host that was configured and never came up stays visible. A host with
        no node row below is configured but not available. Finished attempts age out after 30 days;
        an <code>issued</code> row does not — it is the answer to &ldquo;was this host ever
        configured?&rdquo; — so a host that is gone for good has to be forgotten by hand.
      </p>
      <table className="grid">
        <thead>
          <tr>
            <th>Host</th>
            <th>Node name</th>
            <th>Install</th>
            <th>Enrolled?</th>
            <th>Started</th>
            <th>Error</th>
            {onForget && <th />}
          </tr>
        </thead>
        <tbody>
          {jobs.map((j) => {
            const node = enrolled(j.name);
            return (
              <tr key={j.id}>
                <td><code>{j.host}</code></td>
                <td>{j.name}</td>
                <td>
                  <span className={installBadgeClass(j.state)} title={installBadgeTitle(j.state)}>
                    {j.state}
                  </span>
                </td>
                <td>
                  {node ? (
                    <>
                      <StatusBadge status={node.status} />
                      {node.status === 'offline' && (
                        <span className="muted"> (last seen <TimeAgo s={node.last_heartbeat_at} />)</span>
                      )}
                    </>
                  ) : (
                    <span className="badge warn" title="No node with this name has ever enrolled — the install finished, but the daemon is not talking to this control plane">
                      never enrolled
                    </span>
                  )}
                </td>
                <td>{fmtTime(j.started_at)}</td>
                <td>{j.error ?? '—'}</td>
                {onForget && (
                  <td>
                    <button
                      onClick={() => onForget(j.id)}
                      // A running install is refused by the server too (409):
                      // the row is the only place that shows the work, and the
                      // install thread would rewrite it when it finished.
                      disabled={j.state === 'running' || forgetting === j.id}
                      title={
                        j.state === 'running'
                          ? 'This install is running right now — deleting the record would hide it'
                          : 'Delete this record. The host disappears from this table; a node that enrolled is untouched.'
                      }
                    >
                      {forgetting === j.id ? 'forgetting…' : 'forget'}
                    </button>
                  </td>
                )}
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}

export default function Nodes() {
  const [nodes, setNodes] = useState<NodeView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<NodeView | null>(null);
  // Feature "opencode profiles": audit viewer expander (auto-polls with the
  // rest of the page; the WS push sequence on the CP also surfaces as a new
  // audit row immediately after the node applies it).
  const [auditNode, setAuditNode] = useState<string | null>(null);
  // Audit follow-up: audit responses raced — a slow fetch for node A could
  // resolve after node B's and render A's history under B's expanded row.
  // The rows now carry their owning node id and only render when they match.
  const [auditRows, setAuditRows] = useState<{ nodeId: string; rows: OpencodeAuditEntry[] } | null>(null);
  const [profiles, setProfiles] = useState<OpencodeProfile[] | null>(null);
  const [assignBusy, setAssignBusy] = useState<string | null>(null);
  // Enrollment token for joining a new node (POST /v1/nodes/enrollment-token).
  const [enrollToken, setEnrollToken] = useState<{ token: string; expires_at: string } | null>(null);
  // Per-node credential-pool usage (Stage: accounts/usage).
  const [usageNode, setUsageNode] = useState<string | null>(null);
  // Install-from-the-panel: the modal is always offered, but what it can do
  // depends on `AGENTGRID_PROVISION_ENABLED` on the server — `prov` carries
  // that answer (and the reason when the answer is no).
  const [installOpen, setInstallOpen] = useState(false);
  const [prov, setProv] = useState<ProvisioningInfo | null>(null);
  const [usageRows, setUsageRows] = useState<AccountUsage[] | null>(null);
  // Hosts this control plane has seen an install for. Durable since 0.8.2: a
  // restart used to forget them, which erased the only evidence that a host had
  // been configured at all. `issued` rows come from a token mint that named a
  // host — an install that ran somewhere else and never reported back.
  const [installs, setInstalls] = useState<InstallJob[]>([]);
  // Which install record is being deleted right now (its button shows it).
  const [forgetBusy, setForgetBusy] = useState<string | null>(null);
  // HA only: what this replica knows about its peers. `null` in single mode or
  // on an older control plane, in which case the section renders nothing.
  const [cluster, setCluster] = useState<ClusterStatus | null>(null);

  const load = () => {
    listNodes()
      .then((n) => {
        setError(null);
        setNodes(n);
      })
      .catch(setError);
    listOpencodeProfiles().then(setProfiles).catch(() => undefined);
    listInstallJobs().then(setInstalls).catch(() => undefined);
    getClusterStatus().then(setCluster).catch(() => undefined);
    if (auditNode) {
      getOpencodeAudit(auditNode)
        .then((rows) => setAuditRows({ nodeId: auditNode, rows }))
        .catch(() => setAuditRows({ nodeId: auditNode, rows: [] }));
    }
  };

  useEffect(load, []);
  useEffect(() => {
    getProvisioning()
      .then(setProv)
      .catch(() =>
        // An older control plane has no such route, and a transient failure is
        // just as likely. Either way the modal must not sit on "checking…":
        // fall back to the manual command, which always works.
        setProv({
          enabled: false,
          node_binary: null,
          reason:
            'this control plane did not report its provisioning capability — use the command below',
        }),
      );
  }, []);
  useLiveRefresh(load);

  const assignProfile = async (nodeId: string, profileId: string | null) => {
    setAssignBusy(nodeId);
    try {
      await postJson(`/v1/nodes/${nodeId}/opencode-profile`, { profile_id: profileId });
      load();
    } catch (e) {
      setError(e);
    } finally {
      setAssignBusy(null);
    }
  };

  const toggleAudit = async (nodeId: string) => {
    if (auditNode === nodeId) {
      setAuditNode(null);
      setAuditRows(null);
      return;
    }
    setAuditNode(nodeId);
    setAuditRows(null);
    try {
      const rows = await getOpencodeAudit(nodeId);
      // Drop a stale response whose node is no longer the open one.
      setAuditRows((cur) => (cur === null ? { nodeId, rows } : cur));
    } catch (e) {
      setAuditRows({ nodeId, rows: [] });
      setError(e);
    }
  };

  // Delete the record of a host that will never arrive. `issued` rows are
  // exempt from retention on purpose, so this is the only way out of the list
  // for a decommissioned host — and the server refuses it for a running install
  // (409), which is why the button is disabled for that state too.
  const forgetJob = async (id: string) => {
    setForgetBusy(id);
    try {
      await forgetInstallJob(id);
      toast.ok('Install record deleted');
      load();
    } catch (e) {
      setError(e);
    } finally {
      setForgetBusy(null);
    }
  };

  const revoke = async (n: NodeView) => {
    setBusy(n.id);
    try {
      const r = await revokeNode(n.id);
      if (r.ok) {
        toast.ok(`Node ${n.name} revoked`);
        load();
      } else setError(new Error(`Revoke failed (${r.status})`));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  // Hardening P2 item 37: toggle maintenance drain.
  const toggleDrain = async (n: NodeView) => {
    setBusy(n.id);
    try {
      const r = await drainNode(n.id, !n.drained);
      if (r.ok) {
        toast.ok(`Node ${n.name} ${n.drained ? 'undrained' : 'drained'}`);
        load();
      } else setError(new Error(`Drain failed (${r.status})`));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  const mintEnrollToken = async () => {
    try {
      const t = await postJson<{ token: string; expires_at: string }>(
        '/v1/nodes/enrollment-token',
        {},
      );
      setEnrollToken(t);
    } catch (e) {
      setError(e);
    }
  };

  const toggleUsage = async (nodeId: string) => {
    if (usageNode === nodeId) {
      setUsageNode(null);
      setUsageRows(null);
      return;
    }
    setUsageNode(nodeId);
    setUsageRows(null);
    try {
      setUsageRows(await getJson<AccountUsage[]>(`/v1/nodes/${nodeId}/accounts/usage`));
    } catch {
      setUsageRows([]);
    }
  };

  if (!nodes) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Nodes</h2>
      <p>
        <button onClick={() => setInstallOpen(true)}>Install node…</button>{' '}
        <button onClick={mintEnrollToken}>Enrollment token</button>{' '}
        <span className="muted">(one-shot token a new node daemon uses at first boot)</span>
      </p>
      {enrollToken && (
        <div className="muted" style={{ fontFamily: 'monospace', fontSize: 12, wordBreak: 'break-all' }}>
          {enrollToken.token} — expires {fmtTime(enrollToken.expires_at)}{' '}
          <button onClick={() => { navigator.clipboard.writeText(enrollToken.token).catch(() => {}); }}>copy</button>{' '}
          <button onClick={() => setEnrollToken(null)}>dismiss</button>
        </div>
      )}
      {error ? <ErrorBox err={error} /> : null}
      <FleetSummary nodes={nodes} />
      {installOpen && (
        <InstallNode
          info={prov}
          nodes={nodes}
          onClose={() => setInstallOpen(false)}
          onInstalled={load}
        />
      )}
      <table className="grid">
        <thead>
          <tr>
            <th>Status</th>
            <th>Name</th>
            <th>Adapters</th>
            <th>Repositories</th>
            <th>Load</th>
            <th>Active</th>
            <th>Free disk</th>
            <th>Free mem</th>
            <th>Interception</th>
            <th>Spool</th>
            <th>Heartbeat</th>
            <th>Opencode</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {nodes.map((n) => (
            <Fragment key={n.id}>
              <tr>
                <td><StatusBadge status={n.status} /></td>
                <td>
                  {n.name}
                  {/* Hardening P0 item 5: flag fully-unrestricted nodes. */}
                  {n.unsafe_active && (
                    <span className="badge err" title="Unsafe unattended mode: no sandbox, permissions bypassed">
                      ⚠ unsafe
                    </span>
                  )}
                  <AdapterEnvBadge delivery={n.adapter_env_delivery} />
                  <ReachBadge relayHops={n.relay_hops} egressProxy={n.egress_proxy} />
                  <RelayBadge url={n.relay_url} listen={n.relay_listen} />
                </td>
                <td>{n.adapters.join(', ') || '—'}</td>
                <td>{n.repositories.join(', ') || '—'}</td>
                <td>{n.load_avg.toFixed(2)}</td>
                <td>{n.active_attempts}/{n.max_concurrency}</td>
                <td>{n.free_disk_mb >= 1024 ? `${(n.free_disk_mb / 1024).toFixed(1)} GB` : `${n.free_disk_mb} MB`}</td>
                <td>{(() => {
                  const m = n.mem_available_mb ?? 0;
                  if (m <= 0) return '—';
                  return m >= 1024 ? `${(m / 1024).toFixed(1)} GB` : `${m} MB`;
                })()}</td>
                <td>{n.permission_interception ?? 'wrapper'}</td>
                {/* Hardening P2 item 35: outbox + artifact spool pressure. */}
                <td>
                  {(() => {
                    const b = (n.outbox_bytes ?? 0) + (n.artifact_spool_bytes ?? 0);
                    if (b <= 0) return '—';
                    if (b >= 1048576) return `${(b / 1048576).toFixed(1)} MB`;
                    return `${b} B`;
                  })()}
                </td>
                <td><TimeAgo s={n.last_heartbeat_at} /></td>
                <td>
                  {n.status !== 'revoked' && (
                    <div>
                      <select
                        disabled={assignBusy === n.id}
                        value={n.opencode_profile_id ?? ''}
                        onChange={(e) =>
                          assignProfile(n.id, e.target.value === '' ? null : e.target.value)
                        }
                        title="Bind node → opencode profile"
                      >
                        <option value="">— no profile —</option>
                        {(profiles ?? []).map((p) => (
                          <option key={p.id} value={p.id}>
                            {p.name}
                          </option>
                        ))}
                      </select>
                      {' '}
                      <button
                        onClick={() => toggleAudit(n.id)}
                        title="Show opencode-config apply history"
                      >
                        {auditNode === n.id ? 'hide audit' : 'audit'}
                      </button>
                    </div>
                  )}
                </td>
                <td>
                  {n.status !== 'revoked' && (
                    <>
                      <button
                        onClick={() => toggleUsage(n.id)}
                        title="Pool usage per account"
                      >
                        {usageNode === n.id ? 'hide usage' : 'usage'}
                      </button>{' '}
                      <button
                        disabled={busy === n.id}
                        onClick={() => toggleDrain(n)}
                        title={n.drained ? 'Undrain — allow new assignments' : 'Drain — stop new assignments (in-flight continue)'}
                      >
                        {n.drained ? 'Undrain' : 'Drain'}
                      </button>{' '}
                      <button
                        className="danger"
                        disabled={busy === n.id}
                        onClick={() => setConfirming(n)}
                      >
                        Revoke
                      </button>
                    </>
                  )}
                </td>
              </tr>
              {usageNode === n.id && (
                <tr>
                  <td colSpan={12}>
                    {usageRows === null ? (
                      <div className="muted">loading…</div>
                    ) : usageRows.length === 0 ? (
                      <div className="muted">no account-usage rows</div>
                    ) : (
                      <table className="grid">
                        <thead>
                          <tr><th>Env</th><th>Token idx</th><th>Attempts</th><th>Rate-limited</th></tr>
                        </thead>
                        <tbody>
                          {usageRows.map((u) => (
                            <tr key={u.env}>
                              <td>{u.env}</td>
                              <td>{u.token_index}</td>
                              <td>{u.attempts}</td>
                              <td>{u.rate_limited}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    )}
                  </td>
                </tr>
              )}
              {auditNode === n.id && (
                <tr>
                  <td colSpan={12}>
                    {auditRows === null || auditRows.nodeId !== n.id ? (
                      <div className="muted">loading…</div>
                    ) : auditRows.rows.length === 0 ? (
                      <div className="muted">no opencode-config applies yet</div>
                    ) : (
                      <table className="grid">
                        <thead>
                          <tr>
                            <th>At</th>
                            <th>Trigger</th>
                            <th>Profile</th>
                            <th>Hash</th>
                          </tr>
                        </thead>
                        <tbody>
                          {auditRows.rows.map((a) => (
                            <tr key={a.id}>
                              <td>{fmtTime(a.at)}</td>
                              <td>{a.trigger}</td>
                              <td>{a.profile_id ?? '—'}</td>
                              <td><code>{a.hash.slice(0, 16)}</code></td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    )}
                  </td>
                </tr>
              )}
            </Fragment>
          ))}
        </tbody>
      </table>
      <ClusterPeers status={cluster} />
      <InstallAttempts
        jobs={installs}
        nodes={nodes}
        onForget={forgetJob}
        forgetting={forgetBusy}
      />
      {confirming && (
        <ConfirmModal
          title="Revoke node"
          body={`Revoke node "${confirming.name}"? It will be denied auth immediately.`}
          confirmLabel="Revoke"
          danger
          onConfirm={() => {
            const n = confirming;
            setConfirming(null);
            revoke(n);
          }}
          onCancel={() => setConfirming(null)}
        />
      )}
    </section>
  );
}
