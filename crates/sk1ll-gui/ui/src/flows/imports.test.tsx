// Bringing Skills in from git sources: adding Imports, and taking their upstream updates into the Draft.
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import { dialog, heading, isDisabled, launch, main, nav } from './harness';
import type { AddImportRequest } from '../types';

const row = (name: string) => within(screen.getAllByRole('row').find(row => row.querySelector('.font-medium')?.textContent === name)!);

it('checks Imports, previews upstream commits, and updates one into the Draft against the upstream it showed', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/imports', setup: daemon => {
    daemon.imports.skipped = [{ name: 'pdf', reason: "host `example` is not in the server's SK1LL_SERVER_IMPORT_HOSTS" }];
  } });
  await heading('Imports');
  expect(await nav().findByLabelText('1 Import update, 1 failed check')).toBeTruthy();
  await screen.findByRole('table', { name: 'Imports' });
  expect(row('internal-skills').getByText('Check failed')).toBeTruthy();
  expect(row('internal-skills').getByText('git fetch failed: repository not found')).toBeTruthy();
  expect(row('pdf').getByText('Not checked by Server')).toBeTruthy();
  expect(row('pdf').getByText(/is not in the server's SK1LL_SERVER_IMPORT_HOSTS/)).toBeTruthy();
  expect(row('anthropic-skills').getByText('4e5f6a7b → c0ffee12')).toBeTruthy();

  expect(daemon.sent('import_changes')).toEqual([]);
  await user.click(row('anthropic-skills').getByRole('button', { name: 'Upstream changes' }));
  expect(await screen.findByText(/Update skill instructions/)).toBeTruthy();
  expect(daemon.sent('import_changes')).toEqual([{ name: 'anthropic-skills', upstream: 'c0ffee123456' }]);

  // Upstream moved after the check: the update is refused, the check runs again, and the next update takes the new commit.
  daemon.imports.updates[0].upstream = 'deadbeef0000';
  await user.click(screen.getByRole('button', { name: 'Update anthropic-skills into Draft' }));
  expect(await row('anthropic-skills').findByText('4e5f6a7b → deadbeef')).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Update anthropic-skills into Draft' }));
  expect(await screen.findByText('anthropic-skills is in the Draft')).toBeTruthy();
  expect(daemon.sent('update_import')).toEqual([{ name: 'anthropic-skills', expected: 'c0ffee123456' }, { name: 'anthropic-skills', expected: 'deadbeef0000' }]);
  expect(daemon.sent('check_imports')).toHaveLength(1);

  await user.click(screen.getByRole('button', { name: 'Remove internal-skills' }));
  const remove = await dialog('Remove internal-skills?');
  await user.click(remove.getByRole('button', { name: 'Remove' }));
  expect(await main().findByText('internal-skills removal is in the Draft')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Remove internal-skills' })).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Review Draft' }));
  await heading('Draft');
  expect(await screen.findByRole('checkbox', { name: 'Select skill:internal-skills' })).toBeTruthy();
});

it('keeps local edits to an Import by forking it, or overwrites them only after confirming', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/imports', setup: daemon => {
    daemon.imports.updates.push({ name: 'pdf', source: 'github:example/skills', ref: 'main', locked: '4e5f6a7b8c9d', upstream: 'abcabcabcabc' });
    daemon.imports.errors = [];
    daemon.override('update_import', args => args.name === 'pdf' ? Promise.reject({ message: 'locally modified imports: pdf' }) : undefined);
  } });
  await heading('Imports');
  // Updating everything goes on past the failure and reports each result.
  await user.click(await screen.findByRole('button', { name: 'Update all into Draft' }));
  const results = within(await screen.findByRole('list', { name: 'Update results' }));
  expect(await results.findByText('pdf: Failed: locally modified imports: pdf')).toBeTruthy();
  expect(results.getByText('anthropic-skills: Updated into Draft.')).toBeTruthy();
  expect(row('pdf').getByRole('alert').textContent).toContain('This Import has local edits.');

  await user.click(row('pdf').getByRole('button', { name: 'Overwrite my edits' }));
  const overwrite = await dialog('Overwrite edits to pdf?');
  expect(overwrite.getByText(/upstream commit abcabcab/)).toBeTruthy();
  await user.click(overwrite.getByRole('button', { name: 'Cancel' }));
  expect(daemon.sent('force_update_import')).toEqual([]);
  await user.click(row('pdf').getByRole('button', { name: 'Fork to keep my edits' }));
  expect(await screen.findByText('pdf is in the Draft')).toBeTruthy();
  expect(daemon.sent('fork')).toEqual([{ name: 'pdf' }]);
  expect(daemon.content.find(entry => entry.name === 'pdf')?.type).toBe('personal');
});

it('adds several Skills from one repository, retrying only the one that failed, then reviews them in the Draft', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/imports', setup: daemon => {
    let refused = false;
    daemon.override('add_import', args => {
      if ((args.request as AddImportRequest).name !== 'pdf-tools' || refused) return undefined;
      refused = true;
      return Promise.reject({ message: 'Access denied' });
    });
  } });
  await heading('Imports');
  await user.click(await screen.findByRole('button', { name: 'Add Import' }));
  const add = await dialog('Add Import');
  await user.type(add.getByLabelText('Link'), 'github:example/skills');
  await user.click(add.getByRole('button', { name: 'Advanced' }));
  await user.type(await add.findByLabelText('Branch, tag or commit (optional)'), ' v2 ');
  await user.click(add.getByRole('button', { name: 'Resolve' }));
  const pdf = await add.findByRole('checkbox', { name: 'Select skills/pdf' });
  await waitFor(() => expect(document.activeElement).toBe(pdf));
  expect(add.getAllByRole('status').some(status => status.textContent === '2 Skills found')).toBe(true);
  expect(daemon.sent('resolve_import')).toEqual([{ request: { link: 'github:example/skills', path: '', ref: 'v2' } }]);
  // A repository leaves every candidate unchecked; warnings don't block adding.
  expect(checked(pdf)).toBe(false);
  expect(isDisabled(add.getByRole('button', { name: 'Add into Draft' }))).toBe(true);
  expect(add.getByRole('list', { name: 'File warnings for skills/pdf' }).textContent).toContain('scripts/check.py · script, executable');
  expect(add.getAllByText('2,048 bytes total')).toHaveLength(2);

  await user.click(pdf);
  const name = add.getByLabelText('Name for skills/pdf');
  await user.clear(name);
  expect(document.getElementById(name.getAttribute('aria-describedby')!)?.textContent).toBe('Enter a name.');
  expect(name.getAttribute('aria-invalid')).toBe('true');
  for (const taken of ['deploy', '/invalid', 'writing']) {
    await user.clear(name); await user.type(name, taken);
    if (taken === 'writing') await user.click(add.getByRole('checkbox', { name: 'Select skills/writing' }));
    expect(isDisabled(add.getByRole('button', { name: 'Add into Draft' }))).toBe(true);
    if (taken === 'writing') await user.click(add.getByRole('checkbox', { name: 'Select skills/writing' }));
  }
  await user.clear(name); await user.type(name, 'pdf-tools');
  expect(name.hasAttribute('aria-describedby')).toBe(false);
  await user.click(add.getByRole('checkbox', { name: 'Select skills/writing' }));
  await user.click(add.getByRole('button', { name: 'Add into Draft' }));

  const results = within(await add.findByRole('list', { name: 'Import results' }));
  expect(await results.findByText(/^pdf-tools not added: Access denied/)).toBeTruthy();
  expect(results.getByText('writing added into Draft.')).toBeTruthy();
  expect(add.getByRole('alert').textContent).toContain('1 Skill not added.');
  expect((add.getByLabelText('Name for skills/writing') as HTMLInputElement).disabled).toBe(true);
  expect(add.queryByRole('button', { name: 'Change source' })).toBeNull();
  await user.click(add.getByRole('button', { name: 'Retry 1 Skill' }));
  expect(await results.findByText('pdf-tools added into Draft.')).toBeTruthy();
  expect(add.queryByRole('alert')).toBeNull();
  expect(daemon.sent('add_import').map(args => (args.request as AddImportRequest).name)).toEqual(['pdf-tools', 'writing', 'pdf-tools']);
  expect(daemon.sent('add_import')[0]).toEqual({ request: { name: 'pdf-tools', source: 'github:example/skills', path: 'skills/pdf', ref: 'v2', commit: 'c0ffee1234567890' } });

  const review = add.getByRole('button', { name: 'Review in Draft' });
  await waitFor(() => expect(document.activeElement).toBe(review));
  await user.click(review);
  await heading('Draft');
  expect(await screen.findByRole('checkbox', { name: 'Select skill:pdf-tools' })).toBeTruthy();
  expect(screen.getByRole('checkbox', { name: 'Select skill:writing' })).toBeTruthy();
});

it('stops a batch on request, and counts a Skill that landed despite an error response instead of adding it twice', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/imports' });
  await heading('Imports');
  await user.click(await screen.findByRole('button', { name: 'Add Import' }));
  const add = await dialog('Add Import');
  await user.type(add.getByLabelText('Link'), 'github:example/skills');
  daemon.override('resolve_import', () => Promise.reject({ message: 'Fetch failed' }));
  await user.click(add.getByRole('button', { name: 'Resolve' }));
  expect((await add.findByRole('alert')).textContent).toContain('credentials');
  daemon.restore('resolve_import');
  await user.click(add.getByRole('button', { name: 'Resolve' }));
  await user.click(await add.findByRole('checkbox', { name: 'Select skills/pdf' }));
  await user.click(add.getByRole('checkbox', { name: 'Select skills/writing' }));

  let finish!: () => void;
  daemon.override('add_import', args => {
    const request = args.request as AddImportRequest;
    if (request.name === 'pdf') return new Promise(resolve => { finish = () => resolve({ name: 'pdf', old_commit: null, new_commit: 'c0ffee1234567890', changed: true }); });
    // The write lands, but the answer is lost.
    daemon.imports.imports.push({ name: request.name, source: request.source, ref: request.ref, path: request.path, locked: request.commit ?? null });
    daemon.items.push({ item: `skill:${request.name}`, id: 'new-writing', in_baseline: false, draft: 'added', unpushed: false, description: null });
    return Promise.reject({ message: 'connection reset' });
  });
  await user.click(add.getByRole('button', { name: 'Add into Draft' }));
  await user.click(await add.findByRole('button', { name: 'Stop after this Skill' }));
  finish();
  const results = within(await add.findByRole('list', { name: 'Import results' }));
  expect(await results.findByText('writing not attempted.')).toBeTruthy();
  expect(results.getByText('pdf added into Draft.')).toBeTruthy();
  await user.click(add.getByRole('button', { name: 'Retry 1 Skill' }));
  expect(await results.findByText('writing added into Draft (confirmed after an error response).')).toBeTruthy();
  expect(add.queryByRole('alert')).toBeNull();
  expect(add.queryByRole('button', { name: /^Retry/ })).toBeNull();
  expect(daemon.sent('add_import')).toHaveLength(2);
});

const checked = (element: HTMLElement) => element.getAttribute('aria-checked') === 'true';

it('adds the one Skill a folder link names, reusing a deleted item\'s name, and never counts another Import as the one that landed', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/imports', setup: daemon => {
    // pdf was deleted in the Draft, so its name is free again.
    daemon.imports.imports = daemon.imports.imports.filter(entry => entry.name !== 'pdf');
    daemon.content.splice(daemon.content.findIndex(entry => entry.name === 'pdf'), 1);
    daemon.items.push({ item: 'skill:pdf', id: 'id-pdf', in_baseline: true, draft: 'deleted', unpushed: false, description: null });
  } });
  await heading('Imports');
  await user.click(await screen.findByRole('button', { name: 'Add Import' }));
  const add = await dialog('Add Import');
  await user.type(add.getByLabelText('Link'), 'https://github.com/example/skills/tree/main/skills/pdf');
  await user.click(add.getByRole('button', { name: 'Resolve' }));
  expect(((await add.findByLabelText('Name')) as HTMLInputElement).value).toBe('pdf');
  const confirm = add.getByRole('button', { name: 'Add into Draft' });
  expect(isDisabled(confirm)).toBe(false);

  // The answer is lost, and the pdf Import that is there now comes from another commit: it isn't this one.
  daemon.override('add_import', () => {
    daemon.imports.imports.push({ name: 'pdf', source: 'github:example/skills', ref: 'main', path: 'skills/pdf', locked: 'another-commit' });
    return Promise.reject({ message: 'connection reset' });
  });
  await user.click(confirm);
  expect(await add.findByText(/^pdf not added: connection reset/)).toBeTruthy();
  expect(add.queryByText(/confirmed after an error response/)).toBeNull();
  daemon.imports.imports.pop();

  // Nothing can be read back: the user is asked to look at the Draft before retrying.
  daemon.override('add_import', () => Promise.reject({ message: 'connection reset' }));
  daemon.override('items', () => Promise.reject({ message: 'cannot reach the Daemon' }));
  await user.click(add.getByRole('button', { name: 'Retry 1 Skill' }));
  expect(await add.findByText(/Could not check what reached the Draft; review it before retrying./)).toBeTruthy();
  daemon.restore('add_import'); daemon.restore('items');
  await user.click(add.getByRole('button', { name: 'Retry 1 Skill' }));
  expect(await add.findByText('pdf added into Draft.')).toBeTruthy();
  expect(daemon.sent('add_import').at(-1)).toEqual({ request: { name: 'pdf', source: 'github:example/skills', path: 'skills/pdf', ref: 'main', commit: 'c0ffee1234567890' } });
});
