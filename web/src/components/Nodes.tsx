import { useEffect, useState, Fragment } from 'react';
import { drainNode, getJson, getOpencodeAudit, getProvisioning, listNodes, NodeView, OpencodeAuditEntry, OpencodeProfile, ProvisioningInfo, listOpencodeProfiles, postJson, revokeNode, listInstallJobs, InstallJob } from '../api';
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
  delivery?: { plain: number; gateway: number } | null;
}) {
  if (!delivery) return null;
  if (delivery.plain > 0) {
    return (
      <span
        className="badge warn"
        title={
          `${delivery.plain} of ${delivery.plain + delivery.gateway} AGENTGRID_ADAPTER_ENV entries are handed ` +
          'to the agent as plain values, so the key is readable inside the sandbox (threat-model T13a). ' +
          'Write them NAME@gateway=<route> and set AGENTGRID_CRED_GATEWAY_URL to keep the keys out.'
        }
      >
        {delivery.plain} plain key{delivery.plain === 1 ? '' : 's'}
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
 * Two limits, both stated in the UI because both change what the list means:
 * the jobs are in-memory (a control plane restart empties it), and a host
 * installed by hand with `ag nodes install` or by the install script never
 * appears here either — there is no inventory of hosts, only of hosts this
 * control plane provisioned itself.
 *
 * The `Enrolled?` column is the cross-reference that makes the list useful:
 * `succeeded` only means ssh finished, and the question an operator has is
 * whether the daemon is now talking to us.
 */
export function InstallAttempts({ jobs, nodes }: { jobs: InstallJob[]; nodes: NodeView[] }) {
  if (jobs.length === 0) return null;
  const enrolled = (name: string) => nodes.find((n) => n.name === name);
  return (
    <section>
      <h3>Install attempts</h3>
      <p className="muted">
        Hosts this control plane provisioned over ssh. In memory only — a control plane restart
        forgets them, and a host installed by hand (<code>ag nodes install</code> or the install
        script) never appears here. A host with no node row below was configured but is not
        available.
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
                  <span className={j.state === 'failed' ? 'badge err' : 'badge'}>{j.state}</span>
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
  // Hosts this control plane was asked to turn into nodes. Unlike the node
  // list this is **not** durable: the jobs live in the control plane's memory,
  // so a restart forgets them. It is still the only evidence a host exists
  // before it enrolls, which is why it is rendered rather than hidden.
  const [installs, setInstalls] = useState<InstallJob[]>([]);

  const load = () => {
    listNodes()
      .then((n) => {
        setError(null);
        setNodes(n);
      })
      .catch(setError);
    listOpencodeProfiles().then(setProfiles).catch(() => undefined);
    listInstallJobs().then(setInstalls).catch(() => undefined);
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
      <InstallAttempts jobs={installs} nodes={nodes} />
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
