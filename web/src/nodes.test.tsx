import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { AdapterEnvBadge } from './components/Nodes';

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
