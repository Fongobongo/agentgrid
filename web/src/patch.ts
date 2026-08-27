// Competitor-gap feature (diff review): unified-diff line parser. Turns the
// `changes.patch` artifact into positioned lines so the UI can anchor inline
// comments to `file` + new-file line numbers (matching the backend's
// `patch_annotations.line_start/line_end`).

export interface PatchLine {
  /** Raw line text without the leading +/-/space marker (empty for blank). */
  text: string;
  kind: 'hunk' | 'file' | 'add' | 'del' | 'ctx' | 'meta';
  /** File the line belongs to (last `---`/`+++` pair seen). */
  file: string;
  /** 1-based new-file line number; undefined for deletions/meta lines. */
  newLine: number | null;
  /** 1-based old-file line number; undefined for additions/meta lines. */
  oldLine: number | null;
}

export function parsePatchLines(patch: string): PatchLine[] {
  const lines = patch.split('\n');
  const out: PatchLine[] = [];
  let file = '';
  let oldNo = 0;
  let newNo = 0;

  const push = (kind: PatchLine['kind'], text: string, oldLine: number | null, newLine: number | null) => {
    out.push({ text, kind, file, oldLine, newLine });
  };

  for (const raw of lines) {
    if (raw.startsWith('diff --git ') || raw.startsWith('index ') || raw.startsWith('new file') ||
        raw.startsWith('deleted file') || raw.startsWith('old mode') || raw.startsWith('new mode') ||
        raw.startsWith('similarity index') || raw.startsWith('rename from') || raw.startsWith('rename to') ||
        raw.startsWith('Binary files') || raw.startsWith('GIT binary patch') || raw.startsWith('---') || raw.startsWith('+++')) {
      if (raw.startsWith('--- ')) {
        const f = raw.slice(4).trim().replace(/^a\//, '').replace(/^b\//, '');
        if (f && f !== '/dev/null') file = f;
      } else if (raw.startsWith('+++ ')) {
        const f = raw.slice(4).trim().replace(/^a\//, '').replace(/^b\//, '');
        if (f && f !== '/dev/null') file = f;
      }
      push('file', raw, null, null);
      continue;
    }
    if (raw.startsWith('@@')) {
      const m = raw.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
      if (m) {
        oldNo = parseInt(m[1], 10) - 1; // first context line will increment to the start
        newNo = parseInt(m[3], 10) - 1;
      }
      push('hunk', raw, null, null);
      continue;
    }
    if (raw.startsWith('\\ No newline')) {
      push('meta', raw, null, null);
      continue;
    }
    if (raw.startsWith('+')) {
      newNo += 1;
      push('add', raw.slice(1), null, newNo);
    } else if (raw.startsWith('-')) {
      oldNo += 1;
      push('del', raw.slice(1), oldNo, null);
    } else if (raw.startsWith(' ')) {
      oldNo += 1;
      newNo += 1;
      push('ctx', raw.slice(1), oldNo, newNo);
    } else {
      push('meta', raw, null, null);
    }
  }
  return out;
}
