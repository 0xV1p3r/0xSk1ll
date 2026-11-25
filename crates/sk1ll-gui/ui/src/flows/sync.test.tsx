// Syncing Harnesses by hand: a dry-run plan the user confirms, and the instruction files it would write.
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import { dialog, heading, isDisabled, launch } from './harness';
import type { PlacementView } from '../types';

it('applies the reviewed sync plan only after confirming, and asks again whenever the plan changed', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings' });
  await heading('Settings');
  await user.click(screen.getByRole('button', { name: 'Sync now' }));
  const sync = await dialog('Sync now');
  expect(await sync.findByText('/home/dev/.claude/CLAUDE.md')).toBeTruthy();
  expect(sync.getByText('Review in Conflicts.')).toBeTruthy();
  for (const kind of ['update', 'conflict']) expect(sync.getByText(kind, { selector: 'dt' }).nextElementSibling?.textContent).toBe('1');
  expect(daemon.sent('sync_now')).toEqual([]);

  // The first apply stops partway: what it placed is gone from the plan, and the failure is listed.
  const placed: PlacementView = { harness: 'claude', kind: 'update', item: 'instructions', name: 'style', dest: '/home/dev/.claude/CLAUDE.md', reason: null };
  const next: PlacementView = { harness: 'claude', kind: 'create', item: 'skill', name: 'deploy', dest: '/home/dev/.claude/skills/deploy', reason: null };
  daemon.override('sync_now', () => ({ fingerprint: 'remaining', conflicts: daemon.conflicts, actions: [next], completed: [placed], changed: next, errors: ['Could not place notes'] }));
  await user.click(sync.getByRole('button', { name: 'Apply' }));
  expect(await sync.findByText('1 change was applied, then deploy changed — review the remaining plan.')).toBeTruthy();
  expect(sync.getByText('Could not place notes')).toBeTruthy();
  expect(sync.queryByText('/home/dev/.claude/CLAUDE.md')).toBeNull();
  daemon.restore('sync_now');

  // The Daemon's plan no longer matches what was shown: it answers with its plan instead of applying.
  await user.click(sync.getByRole('button', { name: 'Apply' }));
  expect(await sync.findByText('The sync plan changed. Review the new plan before applying.')).toBeTruthy();
  expect(screen.getByRole('dialog', { name: 'Sync now' })).toBeTruthy();
  await user.click(sync.getByRole('button', { name: 'Apply' }));
  await waitFor(() => expect(screen.queryByRole('dialog', { name: 'Sync now' })).toBeNull());
  expect(await screen.findByText('1 conflict remaining.')).toBeTruthy();
  expect(daemon.sent('sync_now').map(args => args.fingerprint)).toEqual([
    JSON.stringify([true, daemon.conflicts]), 'remaining', JSON.stringify([true, daemon.conflicts]),
  ]);

  await user.click(screen.getByRole('button', { name: 'Sync now' }));
  const again = await dialog('Sync now');
  expect(await again.findByText('No changes to sync.')).toBeTruthy();
  expect(isDisabled(again.getByRole('button', { name: 'Apply' }))).toBe(true);
});

it('previews the instruction files a Harness would get, file by file', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings' });
  await heading('Settings');
  const claude = within((await screen.findByRole('checkbox', { name: 'Sync into claude' })).closest('tr')!);
  await user.click(claude.getByRole('button', { name: 'Preview instructions' }));
  let preview = await dialog('Preview instructions for claude');
  expect(await preview.findByText('/home/dev/.claude/CLAUDE.md')).toBeTruthy();
  expect(preview.getByLabelText('Instruction diff').textContent).toContain('+Use two spaces for indentation.');
  await user.keyboard('{Escape}');

  daemon.override('render_instructions', () => [
    { path: '/rules/style.md', format: 'rules', content: 'New style', next_content: 'New style', differs: true, diff: '@@ -1 +1 @@\n-Old style\n+New style' },
    { path: '/rules/security.md', format: 'rules', content: 'Keep secrets private', next_content: 'Keep secrets private', differs: false, diff: '' },
  ]);
  const codex = within(screen.getByRole('checkbox', { name: 'Sync into codex' }).closest('tr')!);
  await user.click(codex.getByRole('button', { name: 'Preview instructions' }));
  preview = await dialog('Preview instructions for codex');
  expect(await preview.findByText('+New style')).toBeTruthy();
  await user.click(preview.getByRole('tab', { name: 'security.md' }));
  expect(preview.getByText('Keep secrets private')).toBeTruthy();
  expect(preview.getByText('Matches the file on disk.')).toBeTruthy();
  expect(preview.queryByText('+New style')).toBeNull();
  expect(daemon.sent('render_instructions')).toEqual([{ name: 'claude' }, { name: 'codex' }]);
});
