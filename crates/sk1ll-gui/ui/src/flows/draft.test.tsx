// Reviewing this Machine's Draft: Commit, Restore, Push, and getting past a Blocked pull with Keep mine.
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import { dialog, heading, isDisabled, launch, notify } from './harness';

it('commits the reviewed selection and refuses to commit once the Draft changed under the dialog', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/draft' });
  await heading('Draft');
  // Friendly names, kinds and line counts without diff headers; no Pull while neither behind nor blocked.
  expect(await screen.findByLabelText('+6 / −0')).toBeTruthy();
  expect(screen.getByLabelText('+2 / −1')).toBeTruthy();
  const row = within(screen.getByRole('checkbox', { name: 'Select skill:release-notes' }).closest('div')!);
  expect(row.getByText('release-notes')).toBeTruthy();
  expect(row.getByText('Skill')).toBeTruthy();
  expect(row.getByText('Added')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Pull Baseline' })).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Expand all diffs' }));
  expect(screen.getByRole('button', { name: 'Diff for rule:style' }).getAttribute('aria-expanded')).toBe('true');
  expect(screen.getByLabelText('Unified diff for rule:style').textContent).toContain('Use two spaces for indentation.');

  const trigger = screen.getByRole('button', { name: 'Commit (2)' });
  await user.click(trigger);
  let commit = await dialog('Commit to Baseline');
  expect((commit.getByRole('textbox', { name: 'Message' }) as HTMLTextAreaElement).value).toBe('commit release-notes, style from workstation');
  await user.keyboard('{Escape}');
  await waitFor(() => expect(document.activeElement).toBe(trigger));

  await user.click(trigger);
  commit = await dialog('Commit to Baseline');
  await user.clear(commit.getByRole('textbox', { name: 'Message' }));
  await user.type(commit.getByRole('textbox', { name: 'Message' }), 'Tighten the style rule');
  // A Draft that can't be read again can't be confirmed.
  daemon.override('draft', () => Promise.reject({ message: 'Draft refresh failed' }));
  await notify('draft');
  expect(await commit.findByText(/Draft refresh failed/)).toBeTruthy();
  expect(isDisabled(commit.getByRole('button', { name: 'Commit' }))).toBe(true);
  daemon.restore('draft');
  // Someone edits the rule and drops release-notes while the dialog is open.
  daemon.diffs['rule:style'] += '\n+Prefer early returns.';
  daemon.draft.splice(daemon.draft.findIndex(entry => entry.item === 'skill:release-notes'), 1);
  await notify('draft');
  await commit.findByText('style: Draft content changed');
  expect(commit.getByText('release-notes: no longer in Draft')).toBeTruthy();
  expect(isDisabled(commit.getByRole('button', { name: 'Commit' }))).toBe(true);
  await user.click(commit.getByRole('button', { name: 'Review updated Draft' }));
  expect((commit.getByRole('textbox', { name: 'Message' }) as HTMLTextAreaElement).value).toBe('Tighten the style rule');
  await user.click(commit.getByRole('button', { name: 'Commit' }));

  const sha = await screen.findByText('7d1e9a0b3c5f8e2d');
  expect(screen.getByText('Pushed to the Server.')).toBeTruthy();
  expect(daemon.sent('commit')).toEqual([{ request: { items: ['rule:style'], message: 'Tighten the style rule' } }]);
  await waitFor(() => expect(sha.closest('[tabindex="-1"]')?.contains(document.activeElement)).toBe(true));
  expect(await screen.findByText('Draft is clean')).toBeTruthy();
});

it('restores only the reviewed items, even when an edit or a new item arrives while Restore is open', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/draft' });
  await heading('Draft');
  const trigger = await screen.findByRole('button', { name: 'Restore all' });
  await user.click(screen.getByRole('button', { name: 'Restore selected' }));
  await dialog('Restore 2 selected items?');
  await user.click(screen.getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Restore selected' })));

  await user.click(trigger);
  const restore = await dialog('Restore all Draft items?');
  expect(restore.getByText('Return release-notes, style to Baseline. Draft changes to these items will be lost.')).toBeTruthy();
  // A binary edit changes only the fingerprint; a new item appears.
  daemon.draft.find(entry => entry.item === 'rule:style')!.fingerprint = 'binary-edit';
  daemon.draft.push({ item: 'skill:new', change: 'added', paths: ['personal/new/SKILL.md'], fingerprint: 'n1' });
  await notify('draft');
  await restore.findByText('new: added to Draft');
  expect(restore.getByText('style: Draft content changed')).toBeTruthy();
  expect(isDisabled(restore.getByRole('button', { name: 'Restore all' }))).toBe(true);
  // Going back to the reviewed bytes doesn't undo the need to review.
  daemon.draft.find(entry => entry.item === 'rule:style')!.fingerprint = 'b2';
  await notify('draft');
  const review = restore.getByRole('button', { name: 'Review updated Draft' });
  await waitFor(() => expect(isDisabled(review)).toBe(false));
  expect(isDisabled(restore.getByRole('button', { name: 'Restore all' }))).toBe(true);
  await user.click(review);
  await user.click(restore.getByRole('button', { name: 'Restore all' }));

  await waitFor(() => expect(daemon.sent('restore')).toEqual([{ request: { items: ['skill:release-notes', 'rule:style'], all: false } }]));
  await screen.findByRole('checkbox', { name: 'Select skill:new' });
  expect(screen.queryByRole('checkbox', { name: 'Select rule:style' })).toBeNull();
  // Arriving items join the selection until the selection is changed by hand.
  expect(screen.getByRole('button', { name: 'Commit (1)' })).toBeTruthy();
  await user.click(screen.getByRole('checkbox', { name: 'Select skill:new' }));
  daemon.draft.push({ item: 'rule:later', change: 'added', paths: ['instructions/later.md'], fingerprint: 'l1' });
  await notify('draft');
  const later = await screen.findByRole('checkbox', { name: 'Select rule:later' });
  expect(later.getAttribute('aria-checked')).toBe('false');
  expect(isDisabled(screen.getByRole('button', { name: /^Commit/ }))).toBe(true);
});

it('gets past a Blocked pull by keeping this Machine\'s version, one item or all at once', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/draft', setup: daemon => {
    Object.assign(daemon.status.report, { blocked: true, clashes: ['rule:style', 'skill:release-notes', 'skill:deploy'] });
    daemon.status.server_baseline = 'b4se11ne0000';
  } });
  await heading('Draft');
  const clashes = within(await screen.findByRole('list', { name: 'Clashing items' }));
  await user.click(screen.getByRole('button', { name: 'Pull Baseline' }));
  expect(await screen.findByText('Pull blocked')).toBeTruthy();
  expect(screen.getByText("The new Baseline also changes style, release-notes, deploy in this Machine's Draft.")).toBeTruthy();
  // Restore takes the Baseline instead, for items that are in this Draft.
  expect(isDisabled(clashes.getByRole('button', { name: 'Restore skill:deploy' }))).toBe(true);
  await user.click(clashes.getByRole('button', { name: 'Restore rule:style' }));
  await dialog('Restore 1 selected item?');
  await user.click(screen.getByRole('button', { name: 'Cancel' }));

  await user.click(clashes.getByRole('button', { name: 'Keep mine: rule:style' }));
  let keep = await dialog('Keep your version of style?');
  expect(keep.getByText(/The whole file wins: the other Machine's change to this item is overwritten/)).toBeTruthy();
  expect(keep.getByText('sk1ll diff')).toBeTruthy();
  expect(daemon.sent('pull_keep')).toEqual([]);
  await user.click(keep.getByRole('button', { name: 'Keep mine' }));
  // The pull is still blocked by the others: nothing is reported as kept on top.
  await waitFor(() => expect(screen.queryByRole('button', { name: 'Keep mine: rule:style' })).toBeNull());
  expect(screen.queryByText(/is back on top/)).toBeNull();

  await user.click(screen.getByRole('button', { name: 'Keep mine for all' }));
  keep = await dialog('Keep your version of every clashing item?');
  expect(keep.getByText(/change to these items is overwritten/)).toBeTruthy();
  await user.click(keep.getByRole('button', { name: 'Keep mine' }));
  const notice = await screen.findByText(/Your version of release-notes, deploy is back on top of Baseline b4se11ne0000/);
  expect(notice.textContent).toMatch(/replacing the other Machine's change/);
  expect(screen.getByText('sk1ll diff skill:release-notes skill:deploy')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Diff for skill:release-notes' }).getAttribute('aria-expanded')).toBe('true');
  expect(daemon.sent('pull_keep')).toEqual([{ request: { items: ['rule:style'], all: false } }, { request: { items: [], all: true } }]);
  await waitFor(() => expect(screen.queryByRole('list', { name: 'Clashing items' })).toBeNull());
});

it('pushes unpushed work and clears the notes an interrupted Keep mine left', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/draft', setup: daemon => {
    daemon.status.keep_report = [
      { item: 'skill:pdf', entry: '1700-pull-SKILL_md', error: 'the keep stopped after its pull; your version is in recovery' },
      { item: 'skill:pdf', entry: null, error: 'personal/pdf/SKILL.md has staged content the keep didn\'t record as its own, so it is left as it is' },
    ];
  } });
  await heading('Draft');
  const kept = within(await screen.findByRole('list', { name: 'Kept versions in recovery' }));
  expect(kept.getByText('sk1ll recover adopt 1700-pull-SKILL_md')).toBeTruthy();
  expect(kept.getByText(/left as it is/)).toBeTruthy();
  expect(screen.getByText(/Conflicts → Recovered content/)).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Dismiss notes' }));
  await waitFor(() => expect(kept.queryByText(/left as it is/)).toBeNull());
  // A report without notes has nothing to dismiss.
  expect(screen.queryByRole('button', { name: 'Dismiss notes' })).toBeNull();

  expect(await screen.findByText('1 unpushed item')).toBeTruthy();
  expect(within(screen.getByRole('checkbox', { name: 'Select skill:release-notes' }).closest('div')!).getByText('Unpushed')).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Push now' }));
  await waitFor(() => expect(screen.queryByText('1 unpushed item')).toBeNull());
  expect(daemon.sent('push')).toHaveLength(1);
});

it('commits to the local repository in standalone, pushing to origin only when asked', async () => {
  const { daemon, user } = launch('standalone-origin', { hash: '#/draft' });
  await heading('Draft');
  await user.click(await screen.findByRole('button', { name: 'Commit (2)' }));
  const commit = await dialog('Commit to local repository');
  await user.click(commit.getByRole('checkbox', { name: 'Push after commit' }));
  await user.click(commit.getByRole('button', { name: 'Commit' }));
  await screen.findByText('Pushed to origin.');
  expect(daemon.calls.map(call => call.cmd).filter(cmd => cmd === 'commit' || cmd === 'push')).toEqual(['commit', 'push']);
});

it('keeps Push and Pull off without an origin, and never commits once every reviewed item left the Draft', async () => {
  const { daemon, user } = launch('standalone', { hash: '#/draft' });
  await heading('Draft');
  expect(isDisabled(await screen.findByRole('button', { name: 'Push' }))).toBe(true);
  expect(isDisabled(screen.getByRole('button', { name: 'Pull' }))).toBe(true);
  expect(screen.getByText('Push and Pull require an origin git remote.')).toBeTruthy();
  await user.click(await screen.findByRole('button', { name: 'Commit (2)' }));
  const commit = await dialog('Commit to local repository');
  expect(commit.queryByRole('checkbox', { name: 'Push after commit' })).toBeNull();
  daemon.draft.splice(0);
  await notify('draft');
  await waitFor(() => expect(isDisabled(commit.getByRole('button', { name: 'Commit' }))).toBe(true));
  await user.click(commit.getByRole('button', { name: 'Review updated Draft' }));
  expect(isDisabled(commit.getByRole('button', { name: 'Commit' }))).toBe(true);
  expect(daemon.sent('commit')).toEqual([]);
});
