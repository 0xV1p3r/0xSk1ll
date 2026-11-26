// Resolving what a Harness edit or a replaced file left: Harness conflicts and recovered content.
import { expect, it } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import { dialog, heading, launch, nav } from './harness';

it('resolves a Harness conflict from the Library banner and discards recovered content only after confirming', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/library' });
  await heading('Library');
  expect(await screen.findByText('This Machine has a Conflict')).toBeTruthy();
  expect(screen.getByText('code-review', { selector: 'code' }).parentElement?.textContent).toContain('was edited in codex after the last sync.');
  // The unchanged recovered copy needs no one, so the badge counts only the Harness conflict.
  expect(await nav().findByLabelText('1 Conflict to resolve')).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Resolve' }));
  const title = await heading('Conflicts');
  await waitFor(() => expect(document.activeElement).toBe(title));
  expect(screen.queryByRole('button', { name: 'Resolve' })).toBeNull();

  // Up to three entries open their diffs; only the Harness conflict says when it was edited.
  expect(screen.getByRole('heading', { name: 'Harness conflicts', level: 2 })).toBeTruthy();
  expect(screen.getByRole('heading', { name: 'Recovered content', level: 2 })).toBeTruthy();
  expect(screen.getAllByRole('button', { name: 'Hide diff' })).toHaveLength(2);
  expect((await screen.findAllByLabelText('Conflict diff'))[0].textContent).toContain('-Harness content');
  expect(screen.getAllByText(/^Edited/)).toHaveLength(1);
  expect(screen.getByText('/home/dev/.local/share/sk1ll/recovered/d-1')).toBeTruthy();
  expect(document.querySelector('time')?.getAttribute('tabindex')).toBe('0');
  expect(daemon.sent('conflict_diff')).toEqual(expect.arrayContaining([{ request: { harness: 'codex', name: 'code-review' } }, { request: { id: 'd-1' } }]));

  await user.click(screen.getByRole('button', { name: 'Overwrite code-review in codex' }));
  let confirm = await dialog('Overwrite code-review in codex?');
  expect(confirm.getByText('/home/dev/.codex/skills/code-review')).toBeTruthy();
  await user.click(confirm.getByRole('button', { name: 'Cancel' }));
  expect(daemon.sent('overwrite')).toEqual([]);

  await user.click(screen.getByRole('button', { name: 'Adopt code-review from codex' }));
  confirm = await dialog('Adopt code-review into the Draft?');
  await user.click(confirm.getByRole('button', { name: 'Adopt into Draft' }));
  expect(await screen.findByText('code-review is in the Draft')).toBeTruthy();
  await waitFor(() => expect(screen.queryByRole('heading', { name: 'Harness conflicts' })).toBeNull());
  expect(daemon.sent('adopt')).toEqual([{ target: { harness: 'codex', name: 'code-review' } }]);

  await user.click(screen.getByRole('button', { name: 'Discard notes in claude' }));
  confirm = await dialog('Discard notes in claude?');
  expect(confirm.getByText(/Permanently remove recovered content for notes/)).toBeTruthy();
  await user.click(confirm.getByRole('button', { name: 'Discard' }));
  expect(await screen.findByText('No Conflicts')).toBeTruthy();
  expect(screen.getByText("Every Harness matches this Machine's Draft.")).toBeTruthy();
  expect(daemon.sent('discard_displaced')).toEqual([{ id: 'd-1' }]);

  await user.click(screen.getByRole('button', { name: 'Review Draft' }));
  await heading('Draft');
  expect(await screen.findByRole('checkbox', { name: 'Select skill:code-review' })).toBeTruthy();
});

it('keeps diffs closed when four or more entries need review, loading each on demand', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/conflicts', setup: daemon => {
    daemon.conflicts.push(...['pdf', 'deploy'].map(name => ({ ...daemon.conflicts[0], name, items: [`skill:${name}`], detected_at: null })));
    daemon.recovered.push({ ...daemon.recovered[0], id: 'd-2', name: 'scratch' });
  } });
  await heading('Conflicts');
  const toggles = await screen.findAllByRole('button', { name: 'Show diff' });
  expect(toggles).toHaveLength(5);
  expect(toggles[0].getAttribute('aria-expanded')).toBe('false');
  expect(daemon.sent('conflict_diff')).toEqual([]);
  await user.click(toggles[4]);
  expect(await screen.findByLabelText('Conflict diff')).toBeTruthy();
  expect(daemon.sent('conflict_diff')).toEqual([{ request: { id: 'd-2' } }]);
  expect(screen.getAllByText(/^Edited/)).toHaveLength(1);
});

it('marks recovered content changed or unchanged and offers Adopt only where it can go', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/conflicts', setup: daemon => {
    daemon.recovered.push({ ...daemon.recovered[0], id: 'd-2', name: 'linked', reason: null, changed: true, adoptable: false });
  } });
  await heading('Conflicts');
  const [unchanged, kept] = await screen.findAllByRole('button', { name: /^Discard / });
  expect(unchanged.getAttribute('aria-label')).toBe('Discard notes in claude');
  expect(kept.getAttribute('aria-label')).toBe('Discard linked in claude');
  expect(screen.getByText('Unchanged')).toBeTruthy();
  expect(screen.getByText('Unchanged since sk1ll set it aside. A routine copy is removed on its own after a while.')).toBeTruthy();
  expect(screen.getByText('Changed')).toBeTruthy();
  expect(screen.getByText(/can't tell which item this came from/)).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Adopt notes from claude' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Adopt linked from claude' })).toBeNull();
  // Its diff still loads: what it holds, with no Draft destination.
  expect((await screen.findAllByLabelText('Conflict diff')).length).toBeGreaterThan(0);
  expect(daemon.sent('conflict_diff')).toEqual(expect.arrayContaining([{ request: { id: 'd-2' } }]));
  await user.click(kept);
  await user.click((await dialog('Discard linked in claude?')).getByRole('button', { name: 'Discard' }));
  await waitFor(() => expect(daemon.sent('discard_displaced')).toEqual([{ id: 'd-2' }]));
});

it('labels an edit the user overwrote apart from content that needs them, and keeps it adoptable', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/conflicts', setup: daemon => {
    daemon.recovered.push({ ...daemon.recovered[0], id: 'd-3', name: 'AGENTS.md', reason: null, changed: true, overwritten: true });
  } });
  await heading('Conflicts');
  expect(await screen.findByText('Overwritten')).toBeTruthy();
  expect(screen.queryByText('Changed')).toBeNull();
  expect(screen.getByText(/^Overwritten: your edit is kept here\./)).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Adopt AGENTS.md from claude' }));
  await user.click((await dialog('Adopt AGENTS.md into the Draft?')).getByRole('button', { name: 'Adopt into Draft' }));
  await waitFor(() => expect(daemon.sent('adopt_displaced')).toEqual([{ id: 'd-3' }]));
});
