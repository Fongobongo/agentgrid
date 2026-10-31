import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import InstallNode, { defaultForm, installCommand } from './components/InstallNode';

// No DOM in this project (vitest runs in the node environment), so the modal is
// rendered to static markup. That is enough for the branch that matters most:
// when the control plane cannot install (the default — the feature is behind
// AGENTGRID_PROVISION_ENABLED), the panel must still hand the operator a
// command that works, and it must be the same configuration the form holds.

function render(info: { enabled: boolean; node_binary: string | null; reason: string | null }) {
  return renderToStaticMarkup(
    <InstallNode info={info} onClose={() => {}} onInstalled={() => {}} />,
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

  it('keeps root and accept-new-host-key off by default', () => {
    const boxes = Array.from(html.matchAll(/<input type="checkbox"([^>]*)>/g)).map(
      (m) => /\bchecked\b/.test(m[1]),
    );
    expect(boxes.length).toBe(2);
    expect(boxes).toEqual([false, false]);
  });
});
