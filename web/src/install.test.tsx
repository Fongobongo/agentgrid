import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import InstallNode, {
  defaultForm,
  installCommand,
  joinCommand,
} from './components/InstallNode';
import type { NodeView } from './api';

// No DOM in this project (vitest runs in the node environment), so the modal is
// rendered to static markup. That is enough for the branch that matters most:
// when the control plane cannot install (the default — the feature is behind
// AGENTGRID_PROVISION_ENABLED), the panel must still hand the operator a
// command that works, and it must be the same configuration the form holds.

function render(
  info: { enabled: boolean; node_binary: string | null; reason: string | null },
  nodes: NodeView[] = [],
) {
  return renderToStaticMarkup(
    <InstallNode info={info} nodes={nodes} onClose={() => {}} onInstalled={() => {}} />,
  );
}

describe('installCommand', () => {
  it('always passes --server so no reverse tunnel is opened', () => {
    const cmd = installCommand({
      ...defaultForm(),
      host: 'root@10.0.0.5',
      server: 'http://cp.example.com:7800',
    });
    expect(cmd).toContain('ag nodes install');
    expect(cmd).toContain('--host root@10.0.0.5');
    expect(cmd).toContain('--server http://cp.example.com:7800');
    // Without --server the CLI would tunnel back to the machine running it.
    expect(cmd).not.toContain('--transport ssh-tunnel');
  });

  it('quotes values the shell would otherwise eat', () => {
    const cmd = installCommand({
      ...defaultForm(),
      host: 'root@10.0.0.5',
      name: 'my node',
      repositories: 'a b',
    });
    expect(cmd).toContain("--name 'my node'");
    expect(cmd).toContain("--repositories 'a b'");
    // `*` stays bare: it is the documented default and the CLI quotes it itself.
    expect(installCommand({ ...defaultForm(), host: 'h' })).toContain(
      '--repositories *',
    );
  });

  it('only emits the optional flags that were asked for', () => {
    const plain = installCommand({ ...defaultForm(), host: 'h' });
    expect(plain).not.toContain('--ssh-key');
    expect(plain).not.toContain('--ssh-cert');
    expect(plain).not.toContain('--allow-root');
    expect(plain).not.toContain('--accept-new-host-key');

    const strict = installCommand({
      ...defaultForm(),
      host: 'h',
      ssh_key: '/root/.ssh/id_ed25519',
      ssh_cert: '/root/.ssh/id_ed25519-cert.pub',
      accept_new_host_key: true,
      allow_root: true,
      host_key_fingerprint: 'SHA256:abc',
    });
    expect(strict).toContain('--ssh-key /root/.ssh/id_ed25519');
    // The command has to reproduce the certificate too: an operator who copies
    // it out of the panel must get the same access the button would have used.
    expect(strict).toContain('--ssh-cert /root/.ssh/id_ed25519-cert.pub');
    expect(strict).toContain('--accept-new-host-key');
    expect(strict).toContain('--allow-root');
    expect(strict).toContain('--host-key-fingerprint SHA256:abc');
  });

  it('falls back to a placeholder when the host is empty', () => {
    expect(installCommand({ ...defaultForm(), host: '' })).toContain(
      '--host user@host',
    );
  });
});

describe('joinCommand', () => {
  it('is one line that needs nothing but a token', () => {
    const cmd = joinCommand('http://cp.example.com:7800', 'tok-123');
    expect(cmd).toBe(
      'curl -fsSL http://cp.example.com:7800/v1/node/join.sh | sudo bash -s -- --token tok-123',
    );
  });

  it('passes the token as an argument, never in the URL', () => {
    const cmd = joinCommand('https://cp.example.com', 'tok-123');
    // A query string would land in the control plane's access log, where the
    // token would outlive its ten minutes.
    expect(cmd).not.toContain('?token=');
    expect(cmd).toContain('bash -s -- --token');
  });

  it('strips a trailing slash so the path does not double up', () => {
    expect(joinCommand('https://cp.example.com/', 't')).toContain(
      'https://cp.example.com/v1/node/join.sh',
    );
  });

  // The one difference a relayed join has, and the reason it is not left to
  // the script's default: that default is AGENTGRID_PUBLIC_URL, which is the
  // address the host cannot reach — that is why it needs a relay.
  it('names the relay as the server when the join goes through one', () => {
    const cmd = joinCommand('http://node-a:7900', 't', true);
    expect(cmd).toContain('curl -fsSL http://node-a:7900/v1/node/join.sh');
    expect(cmd).toContain('--server http://node-a:7900');
  });
});

describe('the relay picker', () => {
  const nodes = [
    { id: 'n1', name: 'plain', relay_url: null },
    { id: 'n2', name: 'relay-a', relay_url: 'http://node-a:7900' },
  ] as unknown as NodeView[];

  it('offers only nodes that announce a dialable address', () => {
    const html = render(
      { enabled: false, node_binary: null, reason: 'off' },
      nodes,
    );
    expect(html).toContain('relay-a');
    // A node that is not a relay cannot carry the join; offering it would only
    // produce a 409 after the operator pressed the button.
    expect(html).not.toContain('plain');
    expect(html).toContain('node-a:7900');
  });

  it('says how to make one when the fleet has no relay', () => {
    const html = render({ enabled: false, node_binary: null, reason: 'off' }, []);
    expect(html).toContain('AGENTGRID_RELAY_LISTEN');
    expect(html).toContain('AGENTGRID_RELAY_ADVERTISE');
  });

  // "No relay here" and "a relay that listens and announces nothing" need
  // different answers. The second one is a node with AGENTGRID_RELAY_LISTEN set
  // and no advertise address — telling its operator to set the listen variable
  // is the loop this replaces.
  it('names a relay that listens but announces nothing instead of saying there is none', () => {
    const html = render({ enabled: false, node_binary: null, reason: 'off' }, [
      { id: 'n3', name: 'half', relay_url: null, relay_listen: '0.0.0.0:7900' },
    ] as unknown as NodeView[]);
    expect(html).toContain('half');
    expect(html).toContain('AGENTGRID_RELAY_ADVERTISE');
    expect(html).not.toContain('No node announces a relay address yet');
  });

  // Offline is transient, so it stays selectable — the relay may be back by the
  // time the line is pasted — but the picker must not present it as healthy.
  it('marks a relay that is not online and still offers it', () => {
    const html = render({ enabled: false, node_binary: null, reason: 'off' }, [
      { id: 'n4', name: 'flaky', relay_url: 'http://flaky:7900', status: 'offline' },
    ] as unknown as NodeView[]);
    expect(html).toContain('flaky');
    expect(html).toContain('(offline)');
  });
});

describe('InstallNode when the control plane cannot install', () => {
  const html = render({
    enabled: false,
    node_binary: null,
    reason: 'provisioning is disabled on this control plane; set AGENTGRID_PROVISION_ENABLED=1',
  });

  it('shows the server reason verbatim', () => {
    expect(html).toContain('AGENTGRID_PROVISION_ENABLED=1');
  });

  it('shows the command instead of a form', () => {
    expect(html).toContain('ag nodes install');
    expect(html).toContain('copy command');
    // No Install button: there is nothing to press it against.
    expect(html).not.toContain('>Install<');
  });

  it('offers the pull-based join too', () => {
    // The join does not need AGENTGRID_PROVISION_ENABLED: the host reaches the
    // control plane, not the other way round.
    expect(html).toContain('Or join from the new host');
    expect(html).toContain('get join command');
  });
});

describe('InstallNode when the control plane can install', () => {
  const html = render({
    enabled: true,
    node_binary: '/usr/local/bin/agentgrid-node-daemon',
    reason: null,
  });

  it('offers the form and the Install button, not the command', () => {
    expect(html).toContain('>Install<');
    expect(html).toContain('root@10.0.0.5'); // the host placeholder
    expect(html).not.toContain('copy command');
  });

  it('offers the join even when the control plane can install', () => {
    expect(html).toContain('Or join from the new host');
  });

  it('keeps root and accept-new-host-key off by default', () => {
    const boxes = Array.from(html.matchAll(/<input type="checkbox"([^>]*)>/g)).map(
      (m) => /\bchecked\b/.test(m[1]),
    );
    expect(boxes.length).toBe(2);
    expect(boxes).toEqual([false, false]);
  });
});
