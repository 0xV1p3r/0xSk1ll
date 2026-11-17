import { expect, it } from 'vitest';
import { unifiedDiff } from './textDiff';

it('returns nothing for identical text', () => expect(unifiedDiff('SKILL.md', 'a\nb\n', 'a\nb\n')).toBe(''));

it('emits a hunk with context around a change', () => {
  const before = ['1', '2', '3', '4', '5', '6', '7', '8', '9'].join('\n');
  const after = before.replace('5', 'five');
  expect(unifiedDiff('SKILL.md', before, after)).toBe(['--- a/SKILL.md', '+++ b/SKILL.md', '@@ -2,7 +2,7 @@', ' 2', ' 3', ' 4', '-5', '+five', ' 6', ' 7', ' 8'].join('\n'));
});

it('marks files present on one side only', () => {
  expect(unifiedDiff('extra.md', null, 'x\n')).toBe(['--- /dev/null', '+++ b/extra.md', '@@ -0,0 +1,1 @@', '+x'].join('\n'));
  expect(unifiedDiff('gone.md', 'x\n', null)).toBe(['--- a/gone.md', '+++ /dev/null', '@@ -1,1 +0,0 @@', '-x'].join('\n'));
});
