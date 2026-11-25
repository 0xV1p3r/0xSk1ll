// The Library: browsing this Machine's Skills and Rules, making them editable, creating, renaming and deleting them.
// Editing a file in place is its own flow (editing.test.tsx).
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import { dialog, heading, isDisabled, launch } from './harness';

const rows = () => within(screen.getByRole('table', { name: 'Skills' })).getAllByRole('row').slice(1);

it('filters, searches and sorts the Library, and reads an item\'s files rendered or raw', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/library' });
  await heading('Library');
  await screen.findByRole('table', { name: 'Skills' });
  expect(rows()).toHaveLength(7);
  for (const [type, name] of [['import', 'pdf'], ['unmanaged', 'notes'], ['rule', 'style']]) {
    await user.selectOptions(screen.getByLabelText('Type'), type);
    expect(rows().some(row => row.textContent?.includes(name))).toBe(true);
  }
  expect(rows()).toHaveLength(2);
  await user.selectOptions(screen.getByLabelText('Type'), 'all');
  await user.type(screen.getByLabelText('Search by name'), 'PDF');
  expect(rows()).toHaveLength(1);
  await user.type(screen.getByLabelText('Search by name'), 'x');
  expect(screen.getByText('No matching Skills or Rules')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Add Import' })).toBeNull();
  await user.clear(screen.getByLabelText('Search by name'));
  const first = rows()[0].textContent;
  await user.click(screen.getByRole('button', { name: 'Name' }));
  expect(rows()[0].textContent).not.toBe(first);

  // Opening a row with the keyboard: an Import is read-only until forked.
  screen.getByRole('row', { name: 'View Import pdf' }).focus();
  await user.keyboard('{Enter}');
  const pdf = await dialog('pdf');
  expect(pdf.getByRole('button', { name: 'Fork to edit' })).toBeTruthy();
  expect(pdf.queryByRole('button', { name: 'Rename…' })).toBeNull();
  expect(await pdf.findByRole('heading', { name: 'Instructions' })).toBeTruthy();
  expect(within(pdf.getByRole('region', { name: 'Frontmatter' })).getByText('Work with this Skill.')).toBeTruthy();
  expect(pdf.getByRole('navigation', { name: 'Files' })).toBeTruthy();
  expect(document.querySelector('script')).toBeNull();
  await user.click(pdf.getByRole('button', { name: 'Raw' }));
  expect(pdf.getByText(/Use \*\*careful review\*\* before/).tagName).toBe('PRE');
  await user.click(pdf.getByRole('button', { name: 'Rendered' }));
  await user.click(pdf.getByRole('button', { name: 'example.md' }));
  await waitFor(() => expect(daemon.sent('skill_file').at(-1)).toEqual({ query: { item: 'skill:pdf', path: 'references/example.md' } }));
  await user.keyboard('{Escape}');

  // A Rule is one file, without a file list.
  await user.click(screen.getByRole('row', { name: 'View Rule style' }));
  const style = await dialog('style');
  await waitFor(() => expect(daemon.sent('skill_file').at(-1)).toEqual({ query: { item: 'rule:style', path: 'style.md' } }));
  expect(style.queryByRole('navigation', { name: 'Files' })).toBeNull();
  await user.keyboard('{Escape}');

  // A Harness chip opens the Assignment matrix at that cell.
  await user.click(screen.getByRole('button', { name: 'Assignment for code-review in codex' }));
  await heading('Assignment matrix');
  const target = await screen.findByRole('checkbox', { name: 'skill:code-review in workstation / codex, inherited' });
  await waitFor(() => expect(target.closest('td')?.hasAttribute('data-highlighted')).toBe(true));
});

it('forks an Import and adopts an Unmanaged skill\'s chosen variant before opening either in an editor', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/library' });
  await heading('Library');
  await user.click(await screen.findByRole('row', { name: 'View Import pdf' }));
  let detail = await dialog('pdf');
  await user.click(detail.getByRole('button', { name: 'Fork to edit' }));
  expect(screen.getByText(/no longer receive updates from its source/)).toBeTruthy();
  expect(daemon.sent('fork')).toEqual([]);
  await user.click(screen.getByRole('button', { name: 'Fork and open' }));
  await waitFor(() => expect(daemon.sent('open_in_editor')).toEqual([{ editorId: 'code', item: 'skill:pdf' }]));
  const order = daemon.calls.map(call => call.cmd);
  expect(order.indexOf('fork')).toBeLessThan(order.indexOf('open_in_editor'));
  expect(await detail.findByRole('button', { name: 'Open in VS Code' })).toBeTruthy();
  // A Terminal editor from the split button becomes the preferred one.
  await user.click(detail.getByRole('button', { name: 'Choose editor' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Neovim' }));
  expect(await detail.findByRole('button', { name: 'Open in Neovim' })).toBeTruthy();
  expect(daemon.sent('open_in_editor').at(-1)).toEqual({ editorId: 'nvim', item: 'skill:pdf' });
  await user.keyboard('{Escape}');

  // An Unmanaged chip opens that Harness's variant, never the Assignment.
  await user.click(screen.getByRole('button', { name: 'View notes in codex' }));
  detail = await dialog('notes');
  expect((detail.getByLabelText('Harness location and variant') as HTMLSelectElement).value).toBe('sha256:two:codex');
  await waitFor(() => expect(daemon.sent('skill_file').at(-1)).toEqual({ query: { item: 'unmanaged:notes', hash: 'sha256:two', harness: 'codex', path: 'SKILL.md' } }));
  expect(daemon.sent('skill_tree').at(-1)).toEqual({ query: { item: 'unmanaged:notes', hash: 'sha256:two', harness: 'codex' } });
  const adopt = detail.getByRole('button', { name: 'Adopt into Draft' });
  await user.click(adopt);
  await user.click((await dialog('Adopt notes?')).getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(document.activeElement).toBe(adopt));
  await user.click(detail.getByRole('button', { name: 'Choose editor' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Adopt and open in Neovim' }));
  expect(screen.getByText(/original is kept in recovery/)).toBeTruthy();
  expect(screen.getByText(/selected variant from codex/)).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Adopt and open' }));
  await waitFor(() => expect(daemon.sent('open_in_editor').at(-1)).toEqual({ editorId: 'nvim', item: 'skill:notes' }));
  expect(daemon.sent('adopt_unmanaged')).toEqual([{ target: { name: 'notes', harness: 'codex' } }]);
  expect(await detail.findByRole('button', { name: 'Review in Draft' })).toBeTruthy();
});

it('keeps adoption off while the Server is unreachable, explaining why', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/library', setup: daemon => { daemon.status.connection = 'offline'; } });
  await heading('Library');
  await user.click(await screen.findByRole('row', { name: 'View Unmanaged notes' }));
  const detail = await dialog('notes');
  expect(isDisabled(detail.getByRole('button', { name: 'Adopt into Draft' }))).toBe(true);
  expect(isDisabled(detail.getByRole('button', { name: 'Choose editor' }))).toBe(true);
  expect(detail.getByText('Needs a connection to the Server')).toBeTruthy();
  expect(daemon.sent('adopt_unmanaged')).toEqual([]);
});

it('creates, renames and deletes personal items in the Draft, refusing names already taken', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/library' });
  await heading('Library');
  await user.click(await screen.findByRole('button', { name: 'New' }));
  await user.click(await screen.findByRole('menuitem', { name: 'New Skill' }));
  let form = await dialog('New Skill');
  const name = form.getByLabelText('Name');
  await user.click(name); await user.tab();
  expect(form.getByRole('alert').textContent).toBe('Enter a name');
  expect(form.getByText(/New Skills follow each Harness's default/)).toBeTruthy();
  // Skills and Rules have separate namespaces; Imports and Unmanaged skills take Skill names.
  for (const taken of ['bad name', 'deploy', 'pdf', 'notes']) {
    await user.clear(name); await user.type(name, taken);
    expect(isDisabled(form.getByRole('button', { name: 'Create' }))).toBe(true);
  }
  await user.clear(name); await user.type(name, 'style{Enter}');
  await dialog('style');
  expect(daemon.sent('create_item')).toEqual([{ request: { kind: 'skill', name: 'style' } }]);
  await user.keyboard('{Escape}');

  await user.click(screen.getByRole('row', { name: 'View Personal deploy' }));
  let detail = await dialog('deploy');
  await user.click(detail.getByRole('button', { name: 'Rename…' }));
  form = await dialog('Rename deploy');
  const to = form.getByLabelText('Name');
  for (const bad of ['', 'deploy', 'DEPLOY', 'CODE-REVIEW', 'bad name', 'a..b', '_bad']) {
    await user.clear(to); if (bad) await user.type(to, bad);
    expect(isDisabled(form.getByRole('button', { name: 'Rename' }))).toBe(true);
  }
  daemon.override('rename_item', () => Promise.reject({ message: 'Rename failed' }));
  await user.clear(to); await user.type(to, 'ship{Enter}');
  expect(await form.findByText('Rename failed')).toBeTruthy();
  daemon.restore('rename_item');
  await user.click(form.getByRole('button', { name: 'Rename' }));
  detail = await dialog('ship');
  expect(daemon.sent('rename_item').at(-1)).toEqual({ request: { kind: 'skill', name: 'deploy', to: 'ship' } });

  await user.click(detail.getByRole('button', { name: 'Delete…' }));
  const confirm = await dialog('Delete ship?');
  expect(confirm.getByText(/The deletion reaches other Machines after you commit and push it and they sync./)).toBeTruthy();
  await user.click(confirm.getByRole('button', { name: 'Delete' }));
  await waitFor(() => expect(screen.queryByRole('dialog', { name: 'ship' })).toBeNull());
  expect(daemon.sent('delete_item')).toEqual([{ request: { kind: 'skill', name: 'ship' } }]);
  expect(await screen.findByText('ship deleted in Draft')).toBeTruthy();
});
