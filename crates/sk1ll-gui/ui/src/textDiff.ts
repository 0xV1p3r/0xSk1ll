// Line diff for comparing Unmanaged variants in the browser. Output matches the
// `git diff` shape that DiffLines renders.

type Op = { kind: ' ' | '-' | '+'; line: string; a: number; b: number };

// Skill files are small; past this the LCS table costs too much, so the whole file shows as replaced.
const maxCells = 4_000_000;

function lines(text: string) {
  if (!text) return [];
  const split = text.split('\n');
  if (split[split.length - 1] === '') split.pop();
  return split;
}

function ops(a: string[], b: string[]): Op[] {
  let start = 0, endA = a.length, endB = b.length;
  while (start < endA && start < endB && a[start] === b[start]) start++;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) { endA--; endB--; }
  const midA = a.slice(start, endA), midB = b.slice(start, endB);
  const middle: Op[] = [];
  if (midA.length * midB.length > maxCells) {
    midA.forEach((line, i) => middle.push({ kind: '-', line, a: start + i, b: start }));
    midB.forEach((line, i) => middle.push({ kind: '+', line, a: endA, b: start + i }));
  } else {
    const width = midB.length + 1;
    const table = new Uint32Array((midA.length + 1) * width);
    for (let i = midA.length - 1; i >= 0; i--)
      for (let j = midB.length - 1; j >= 0; j--)
        table[i * width + j] = midA[i] === midB[j] ? table[(i + 1) * width + j + 1] + 1 : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    let i = 0, j = 0;
    while (i < midA.length || j < midB.length) {
      if (i < midA.length && j < midB.length && midA[i] === midB[j]) { middle.push({ kind: ' ', line: midA[i], a: start + i, b: start + j }); i++; j++; }
      else if (i < midA.length && (j === midB.length || table[(i + 1) * width + j] >= table[i * width + j + 1])) { middle.push({ kind: '-', line: midA[i], a: start + i, b: start + j }); i++; }
      else { middle.push({ kind: '+', line: midB[j], a: start + i, b: start + j }); j++; }
    }
  }
  return [
    ...a.slice(0, start).map((line, i) => ({ kind: ' ' as const, line, a: i, b: i })),
    ...middle,
    ...a.slice(endA).map((line, i) => ({ kind: ' ' as const, line, a: endA + i, b: endB + i })),
  ];
}

/** A unified diff with three lines of context, or '' when the texts match. */
export function unifiedDiff(path: string, before: string | null, after: string | null, context = 3): string {
  if (before === after) return '';
  const all = ops(lines(before ?? ''), lines(after ?? ''));
  const changed = all.flatMap((op, i) => op.kind === ' ' ? [] : [i]);
  if (!changed.length) return '';
  const out = [`--- ${before === null ? '/dev/null' : `a/${path}`}`, `+++ ${after === null ? '/dev/null' : `b/${path}`}`];
  let index = 0;
  while (index < changed.length) {
    const from = Math.max(0, changed[index] - context);
    let to = changed[index];
    while (index < changed.length && changed[index] <= to + 2 * context) to = changed[index++];
    to = Math.min(all.length - 1, to + context);
    const hunk = all.slice(from, to + 1);
    const countA = hunk.filter(op => op.kind !== '+').length, countB = hunk.filter(op => op.kind !== '-').length;
    out.push(`@@ -${countA ? hunk[0].a + 1 : hunk[0].a},${countA} +${countB ? hunk[0].b + 1 : hunk[0].b},${countB} @@`);
    out.push(...hunk.map(op => op.kind + op.line));
  }
  return out.join('\n');
}
