import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { AdapterEnvBadge, ClusterPeers, FleetSummary, InstallAttempts, ReachBadge, RelayBadge } from './components/Nodes';
import type { ClusterPeer, ClusterStatus, InstallJob, NodeView } from './api';

// ADR 0029 observability. The badge is the only place an operator can see
// whether a node's adapter credentials are still readable inside the sandbox —
// the marker lives in the daemon's env, which the control plane never reads.
//
// No DOM in this project (vitest runs in the node environment), so the badge is
// rendered to static markup; it is a pure function of its props, so that is the
// whole behaviour.

function render(delivery?: { plain: number; gateway: number } | null) {
  return renderToStaticMarkup(<AdapterEnvBadge delivery={delivery} />);
}

describe('AdapterEnvBadge', () => {
  it('warns with a count when entries are still plain values', () => {
    const html = render({ plain: 2, gateway: 1 });
    expect(html).toContain('2 plain keys');
    // The tooltip has to name the threat and the fix: a bare count is a
    // number, not something an operator can act on.
    expect(html).toContain('T13a');
    expect(html).toContain('NAME@gateway');
    expect(html).toContain('badge warn');
  });

  it('says "1 plain key", not "1 plain keys"', () => {
    expect(render({ plain: 1, gateway: 0 })).toContain('1 plain key<');
  });

  it('shows a neutral badge once every entry is gateway-delivered', () => {
    const html = render({ plain: 0, gateway: 3 });
    expect(html).toContain('3 gateway');
    expect(html).toContain('badge ok');
    expect(html).not.toContain('badge warn');
  });

  /**
   * The distinction the whole feature rests on. A node that never reported must
   * render nothing — showing "clean" for it would be a claim the panel cannot
   * make, and it is exactly the state a node running an older binary is in.
   */
  it('renders nothing when the node never reported', () => {
    expect(render(undefined)).toBe('');
    expect(render(null)).toBe('');
  });

  it('renders nothing for a node with no adapter env at all', () => {
    expect(render({ plain: 0, gateway: 0 })).toBe('');
  });
});

/**
 * ADR 0026 + ADR 0012: "is this node reachable, and is it direct?"
 *
 * The two facts come from different places — `relay_hops` is what the control
 * plane observed on the request, `egress_proxy` is what the node claims — so
 * the badge has to keep them apart rather than merge them into one "not
 * direct" label.
 */
function renderReach(relayHops?: number | null, egressProxy?: boolean | null) {
  return renderToStaticMarkup(<ReachBadge relayHops={relayHops} egressProxy={egressProxy} />);
}

describe('ReachBadge', () => {
  it('renders nothing for a direct, proxy-less node', () => {
    // The common case. A badge on every row would be noise; the legend above
    // the table says that "no badge" means direct.
    expect(renderReach(0, false)).toBe('');
  });

  it('names the hop count when a relay carried the heartbeat', () => {
    const html = renderReach(2, false);
    expect(html).toContain('via relay ×2');
    expect(html).toContain('badge warn');
    // The tooltip must say the number is observed, not reported: that is why
    // an operator can trust it.
    expect(html).toContain('x-agentgrid-relay-hops');
    expect(html).toContain('ADR 0026');
  });

  it('shows the proxy claim separately, because a node can be both', () => {
    const html = renderReach(1, true);
    expect(html).toContain('via relay ×1');
    expect(html).toContain('via proxy');
    // And it must not read as a verified fact.
    expect(html).toContain('node claim');
  });

  it('shows only the proxy badge when the node is proxied but direct', () => {
    const html = renderReach(0, true);
    expect(html).toContain('via proxy');
    expect(html).not.toContain('via relay');
  });

  /**
   * The state that must not be silently rendered as "direct": a row older than
   * the migration has no heartbeat with either column, so the panel knows
   * nothing about its path.
   */
  it('says "reach ?" when neither fact has ever been reported', () => {
    expect(renderReach(null, null)).toContain('reach ?');
    expect(renderReach(undefined, undefined)).toContain('reach ?');
  });

  it('trusts a reported 0 even when the proxy claim is missing', () => {
    // A node on an older binary sends no `egress_proxy`; the observed hop
    // count is still an observation, so this is not "unknown".
    const html = renderReach(0, null);
    expect(html).not.toContain('reach ?');
    expect(html).toBe('');
  });
});

function node(over: Partial<NodeView>): NodeView {
  return {
    id: over.name ?? 'id',
    name: 'n',
    status: 'online',
    adapters: [],
    repositories: [],
    max_concurrency: 1,
    active_attempts: 0,
    last_heartbeat_at: '2026-01-01T00:00:00Z',
    agent_version: '0.8.1',
    load_avg: 0,
    free_disk_mb: 0,
    ...over,
  } as NodeView;
}

describe('FleetSummary', () => {
  it('separates "offline" from "revoked" and counts only answering nodes as reachable', () => {
    const html = renderToStaticMarkup(
      <FleetSummary
        nodes={[
          node({ name: 'a', status: 'online' }),
          node({ name: 'b', status: 'degraded' }),
          node({ name: 'c', status: 'offline' }),
          node({ name: 'd', status: 'revoked' }),
        ]}
      />,
    );
    expect(html).toContain('2/4 reachable');
    expect(html).toContain('1 offline');
    expect(html).toContain('1 revoked');
    // degraded is reachable — it is answering, just not fully usable.
    expect(html).toContain('1 degraded');
  });

  it('counts relayed, proxied and unknown paths', () => {
    const html = renderToStaticMarkup(
      <FleetSummary
        nodes={[
          node({ name: 'a', relay_hops: 2, egress_proxy: false }),
          node({ name: 'b', relay_hops: 0, egress_proxy: true }),
          node({ name: 'c', relay_hops: null, egress_proxy: null }),
        ]}
      />,
    );
    expect(html).toContain('1 via relay');
    expect(html).toContain('1 via proxy');
    expect(html).toContain('1 path unknown');
  });

  it('says plainly that an unenrolled host leaves no row', () => {
    const html = renderToStaticMarkup(<FleetSummary nodes={[]} />);
    expect(html).toContain('No nodes are enrolled');
  });
});

function job(over: Partial<InstallJob>): InstallJob {
  return {
    id: over.name ?? 'job',
    host: 'root@10.0.0.5',
    name: 'n',
    state: 'succeeded',
    steps: [],
    error: null,
    server_url: null,
    started_at: '2026-01-01T00:00:00Z',
    finished_at: null,
    ...over,
  } as InstallJob;
}

describe('InstallAttempts', () => {
  it('renders nothing when no host was ever provisioned from the panel', () => {
    expect(renderToStaticMarkup(<InstallAttempts jobs={[]} nodes={[]} />)).toBe('');
  });

  /**
   * The gap this closes: `state: succeeded` only means ssh finished. The
   * question is whether the daemon ever enrolled, and a host that did not has
   * no row in the node table at all.
   */
  it('marks a successful install whose daemon never enrolled', () => {
    const html = renderToStaticMarkup(
      <InstallAttempts jobs={[job({ name: 'ghost', state: 'succeeded' })]} nodes={[]} />,
    );
    expect(html).toContain('never enrolled');
    expect(html).toContain('root@10.0.0.5');
  });

  it('cross-references an enrolled node and its live status', () => {
    const html = renderToStaticMarkup(
      <InstallAttempts
        jobs={[job({ name: 'n1' })]}
        nodes={[node({ name: 'n1', status: 'offline' })]}
      />,
    );
    expect(html).not.toContain('never enrolled');
    expect(html).toContain('offline');
    expect(html).toContain('last seen');
  });

  it('surfaces the failure text of a failed install', () => {
    const html = renderToStaticMarkup(
      <InstallAttempts
        jobs={[job({ name: 'bad', state: 'failed', error: 'ssh: connect to host 10.0.0.5 port 22' })]}
        nodes={[]}
      />,
    );
    expect(html).toContain('badge err');
    expect(html).toContain('ssh: connect to host');
  });
});

/**
 * `issued` is the state a host is in when an enrollment token went out and
 * nothing came back — the case the table exists for. It must not read as a
 * failure (the install may still be running elsewhere) and must not read as
 * success.
 */
describe('InstallAttempts — issued', () => {
  it('marks a token-only row as a warning and still says never enrolled', () => {
    const html = renderToStaticMarkup(
      <InstallAttempts jobs={[job({ name: 'edge-1', state: 'issued' })]} nodes={[]} />,
    );
    expect(html).toContain('badge warn');
    expect(html).not.toContain('badge err');
    expect(html).toContain('never enrolled');
    expect(html).toContain('An enrollment token was minted');
  });

  it('stops claiming the list is memory-only', () => {
    // The old copy said a restart forgets these; that is no longer true and a
    // stale warning is worse than none.
    const html = renderToStaticMarkup(
      <InstallAttempts jobs={[job({ name: 'edge-1', state: 'issued' })]} nodes={[]} />,
    );
    expect(html).not.toContain('In memory only');
    expect(html).toContain('Stored');
  });
});

/**
 * A host that is gone for good has to be removable, because `issued` rows are
 * exempt from retention: without a delete they sit in the list forever and the
 * only way out is SQL. The server refuses a running install (409), so the
 * button must not offer it either.
 */
describe('InstallAttempts — forgetting a record', () => {
  it('offers the action only when there is something to do', () => {
    const plain = renderToStaticMarkup(<InstallAttempts jobs={[job({})]} nodes={[]} />);
    expect(plain).not.toContain('>forget<');

    const html = renderToStaticMarkup(
      <InstallAttempts jobs={[job({ state: 'issued' })]} nodes={[]} onForget={() => undefined} />,
    );
    expect(html).toContain('>forget<');
    expect(html).toContain('Delete this record');
  });

  it('disables the action for an install that is running right now', () => {
    const html = renderToStaticMarkup(
      <InstallAttempts jobs={[job({ state: 'running' })]} nodes={[]} onForget={() => undefined} />,
    );
    expect(html).toContain('disabled');
    expect(html).toContain('deleting the record would hide it');
  });

  it('says which record is being deleted', () => {
    const j = job({ state: 'issued' });
    const html = renderToStaticMarkup(
      <InstallAttempts jobs={[j]} nodes={[]} onForget={() => undefined} forgetting={j.id} />,
    );
    expect(html).toContain('forgetting…');
  });
});

/**
 * HA peers. The point of the section is that a peer nobody can reach is still
 * on the page, and that the page admits whose view it is — only the leader
 * probes everyone.
 */
function peer(over: Partial<ClusterPeer>): ClusterPeer {
  return {
    id: 'cp-b',
    url: 'http://cp-b:7800',
    role: 'data',
    last_ok_unix: null,
    age_secs: null,
    state: 'never',
    applied_seq: null,
    consecutive_failures: 0,
    ...over,
  };
}

function cluster(over: Partial<ClusterStatus>): ClusterStatus {
  return {
    node_id: 'cp-a',
    cluster_id: 'prod',
    role: 'leader',
    term: 3,
    epoch: 1,
    leader_id: 'cp-a',
    leader_url: 'http://cp-a:7800',
    applied_seq: 10,
    peers: [],
    ...over,
  };
}

describe('ClusterPeers', () => {
  it('renders nothing in single mode, where there are no peers', () => {
    expect(renderToStaticMarkup(<ClusterPeers status={null} />)).toBe('');
    expect(renderToStaticMarkup(<ClusterPeers status={cluster({})} />)).toBe('');
  });

  it('lists a peer that has never answered instead of hiding it', () => {
    const html = renderToStaticMarkup(
      <ClusterPeers status={cluster({ peers: [peer({})] })} />,
    );
    expect(html).toContain('cp-b');
    expect(html).toContain('never');
    expect(html).not.toContain('badge err');
  });

  it('separates stale from down, and says how old the contact is', () => {
    const html = renderToStaticMarkup(
      <ClusterPeers
        status={cluster({
          peers: [
            peer({ id: 'cp-b', state: 'stale', age_secs: 90, last_ok_unix: 1 }),
            peer({ id: 'cp-c', state: 'down', age_secs: 7200, consecutive_failures: 4 }),
            peer({ id: 'cp-d', state: 'up', age_secs: 2, applied_seq: 41 }),
          ],
        })}
      />,
    );
    expect(html).toContain('badge warn');
    expect(html).toContain('badge err');
    expect(html).toContain('1m ago');
    expect(html).toContain('2h ago');
    expect(html).toContain('41');
  });

  it('says whose view this is', () => {
    // A standby showing `never` for a peer it has not heard from is correct;
    // without this line it reads as a broken cluster.
    const html = renderToStaticMarkup(
      <ClusterPeers status={cluster({ role: 'standby', peers: [peer({})] })} />,
    );
    expect(html).toContain('standby');
    expect(html).toContain('this');
    expect(html).toContain('already runs');
  });
});

describe('RelayBadge', () => {
  it('renders nothing for a node that is not a relay', () => {
    // Most nodes. A badge on every row would say nothing.
    expect(renderToStaticMarkup(<RelayBadge url={null} />)).toBe('');
    expect(renderToStaticMarkup(<RelayBadge url="" />)).toBe('');
    expect(renderToStaticMarkup(<RelayBadge url={null} listen={null} />)).toBe('');
  });

  it('names the address and says the control plane did not verify it', () => {
    const html = renderToStaticMarkup(<RelayBadge url="http://node-a.internal:7900" />);
    expect(html).toContain('relay');
    expect(html).toContain('http://node-a.internal:7900');
    // The address is a node claim: a relay is reachable from where the control
    // plane is not, so "we saw it work" would be a lie the panel cannot back.
    expect(html).toContain('cannot verify');
    // And it has to point at the thing it is useful for.
    expect(html).toContain('ag nodes add --via');
  });

  // A relay that listens and announces nothing is running and useless. It used
  // to render exactly like a node that is not a relay, so the operator was sent
  // to set AGENTGRID_RELAY_LISTEN — the one variable they had already set.
  it('distinguishes "listening but announcing nothing" from "not a relay"', () => {
    const html = renderToStaticMarkup(<RelayBadge url={null} listen="0.0.0.0:7900" />);
    expect(html).toContain('no advertise');
    expect(html).toContain('0.0.0.0:7900');
    expect(html).toContain('AGENTGRID_RELAY_ADVERTISE');
    // The address is not offered as dialable: this is a bind address.
    expect(html).toContain('badge warn');
  });
});
