import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import NewTask from './components/NewTask';

// No DOM test infra in this project (vitest runs in the node environment and
// there is no @testing-library), but `react-dom/server` still renders the
// component, and a controlled checkbox renders its `checked` state as an
// attribute. That is enough to pin the one default that matters here.
function render() {
  return renderToStaticMarkup(
    <NewTask onCreated={() => {}} onError={() => {}} />,
  );
}

/** Every checkbox in the form, with the text that follows it. */
function checkboxes(html: string) {
  return Array.from(html.matchAll(/<input type="checkbox"([^>]*)>/g)).map(
    (m) => ({
      checked: /\bchecked\b/.test(m[1]),
      label: html
        .slice(m.index! + m[0].length, m.index! + m[0].length + 80)
        .replace(/<[^>]*>/g, ' ')
        .trim(),
    }),
  );
}

/**
 * The form renders `buildBody()` into the "Request preview" `<pre>`, so the
 * exact JSON that would be POSTed is in the markup — no need to simulate a
 * submit to see it.
 */
function requestPreview(html: string): Record<string, unknown> {
  const m = html.match(/<pre>([\s\S]*?)<\/pre>/);
  expect(m, 'the request preview is missing').not.toBeNull();
  const text = m![1]
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&amp;/g, '&');
  return JSON.parse(text) as Record<string, unknown>;
}

describe('NewTask — node-loss retry', () => {
  // The control plane's default is `retry_on_node_loss = true`, and the form
  // only sends the field when the user opts out. If this default ever flips to
  // false, every task created from the web UI silently becomes terminal on
  // node loss — silently, because TaskView does not echo the field back, so
  // nothing in the task view would look wrong.
  it('offers node-loss retry switched on by default', () => {
    const boxes = checkboxes(render());
    const retry = boxes.find((b) => b.label.startsWith('Retry on node loss'));
    expect(retry, 'the node-loss retry checkbox is missing').toBeDefined();
    expect(retry!.checked).toBe(true);
  });

  // GitHub write-back is off by default; asserting it keeps the helper honest —
  // if `checkboxes()` stopped reading the attribute it would report every box
  // as unchecked and the test above would still pass.
  it('reads the unchecked state of the write-back box', () => {
    const boxes = checkboxes(render());
    const push = boxes.find((b) => b.label.startsWith('GitHub write-back'));
    expect(push, 'the GitHub write-back checkbox is missing').toBeDefined();
    expect(push!.checked).toBe(false);
  });

  // The other half of the same polarity: while the box is on, the field must be
  // absent from the body so the control plane's own default governs. Sending
  // `true` would work today and silently pin this client to today's default.
  it('leaves the field out of the request while the box is on', () => {
    const body = requestPreview(render());
    expect('retry_on_node_loss' in body).toBe(false);
    // Nothing else leaks a null either: the control plane cannot distinguish
    // an absent field from `null`, so a client that fills unset fields with
    // null cannot express the tri-state at all.
    expect(Object.values(body)).not.toContain(null);
    expect(body.max_attempts).toBe(1);
  });
});
