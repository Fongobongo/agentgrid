import { describe, it, expect } from 'vitest';
import { parsePatchLines } from './patch';

const PATCH = `diff --git a/src/a.ts b/src/a.ts
index 1111111..2222222 100644
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,3 +1,4 @@
 line1
-line2
+line2 changed
+line3 new
 line4
diff --git a/src/b.ts b/src/b.ts
--- a/src/b.ts
+++ b/src/b.ts
@@ -5 +5,2 @@
-old
+new1
+new2
\\ No newline at end of file
`;

describe('parsePatchLines', () => {
  const lines = parsePatchLines(PATCH);

  it('tracks the current file across ---/+++ pairs', () => {
    const adds = lines.filter((l) => l.kind === 'add');
    expect(adds.map((l) => l.file)).toEqual(['src/a.ts', 'src/a.ts', 'src/b.ts', 'src/b.ts']);
  });

  it('numbers new-file lines from hunk headers', () => {
    const adds = lines.filter((l) => l.kind === 'add');
    expect(adds.map((l) => l.newLine)).toEqual([2, 3, 5, 6]);
  });

  it('numbers old-file lines for context and deletions', () => {
    const ctx = lines.filter((l) => l.kind === 'ctx');
    expect(ctx.map((l) => [l.oldLine, l.newLine])).toEqual([[1, 1], [3, 4]]);
    const del = lines.filter((l) => l.kind === 'del');
    expect(del.map((l) => l.oldLine)).toEqual([2, 5]);
    expect(del.every((l) => l.newLine === null)).toBe(true);
  });

  it('treats missing hunk counts as single-line ranges', () => {
    // "@@ -5 +5,2 @@" => first add lands on new line 5
    const b = lines.filter((l) => l.file === 'src/b.ts' && l.kind === 'add');
    expect(b[0].newLine).toBe(5);
  });

  it('marks file headers, hunk headers and no-newline markers', () => {
    expect(lines.filter((l) => l.kind === 'file').length).toBe(7); // per-file git/extra headers
    expect(lines.filter((l) => l.kind === 'hunk').length).toBe(2);
    expect(lines.filter((l) => l.kind === 'meta').length).toBe(2); // no-newline marker + trailing empty line
  });

  it('handles an empty patch', () => {
    expect(parsePatchLines('')).toEqual([{ text: '', kind: 'meta', file: '', oldLine: null, newLine: null }]);
  });

  it('renames keep the b/ path as the current file', () => {
    const l = parsePatchLines('--- a/old.ts\n+++ b/new.ts\n@@ -1 +1 @@\n-x\n+y\n');
    expect(l.filter((x) => x.kind === 'add')[0].file).toBe('new.ts');
  });
});
