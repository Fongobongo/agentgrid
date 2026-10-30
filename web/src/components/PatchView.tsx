// Shared unified-diff renderer.
//
// Extracted from TaskDetails so the approvals queue can show the same diff
// without leaving the page (backlog item 8): a pending patch-review approval is
// decided on exactly this text, and navigating away to read it was the step
// operators skipped.
//
// `onAnnotate` is optional: with it, add/context lines are clickable and the
// caller opens the annotation composer; without it the diff is read-only.

import { parsePatchLines } from '../patch';

export interface PatchLocation {
  file: string;
  line: number;
}

export default function PatchView({
  text,
  onAnnotate,
}: {
  text: string;
  onAnnotate?: (loc: PatchLocation) => void;
}) {
  const lines = parsePatchLines(text);
  return (
    <>
      {lines.map((l, i) => {
        let cls = 'pl';
        if (l.kind === 'file' || l.kind === 'hunk') cls = 'ph';
        else if (l.kind === 'add') cls = 'pa';
        else if (l.kind === 'del') cls = 'pd';
        else if (l.kind === 'meta') cls = 'ph';
        const canComment =
          !!onAnnotate &&
          (l.kind === 'add' || l.kind === 'ctx') &&
          l.newLine !== null &&
          l.file !== '';
        return (
          <div
            key={i}
            className={cls + (canComment ? ' annotatable' : '')}
            title={canComment ? 'Click to comment on this line' : undefined}
            onClick={
              canComment
                ? () => onAnnotate?.({ file: l.file, line: l.newLine as number })
                : undefined
            }
          >
            <span className="ln">{l.newLine ?? ''}</span>
            <span className="lt">
              {l.kind === 'hunk' || l.kind === 'file' || l.kind === 'meta'
                ? l.text
                : (l.kind === 'add' ? '+' : l.kind === 'del' ? '-' : ' ') +
                  (l.text || ' ')}
            </span>
          </div>
        );
      })}
    </>
  );
}
