// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Conflicts, Imports, Log } from './Activity';
import { api } from './api';
import { toast } from './toast';
vi.mock('./toast', () => ({ toast: vi.fn() }));
import { JoinCodeButton, Settings } from './Settings';
import type { SkillCandidate, StatusView } from './types';

vi.mock('./api', () => ({ api: { editors: vi.fn(), imports: vi.fn(), checkImports: vi.fn(), removeImport: vi.fn(), updateImport: vi.fn(), conflictDiff: vi.fn(), items: vi.fn(), resolveImport: vi.fn(), addImport: vi.fn(), log: vi.fn(), undo: vi.fn(), conflicts: vi.fn(), recovered: vi.fn(), harnesses: vi.fn(), fleet: vi.fn(), join: vi.fn(), joinCode: vi.fn() },
  errorMessage: (e: unknown) => String(e) }));
const changed = vi.fn();
const reviewDraft = vi.fn();
const candidate = (name: string, taken = false): SkillCandidate => ({ path: name, default_name: name, description: `About ${name}`, taken });
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.editors).mockResolvedValue({ editors: [], preferred_editor: 'auto' });
  vi.mocked(api.imports).mockResolvedValue({ checked_at: null, updates: [], errors: [] });
  vi.mocked(api.conflictDiff).mockResolvedValue({ item: 'skill:pdf', diff: '-edit\n+draft' });
  vi.mocked(api.items).mockResolvedValue([]);
  vi.mocked(api.addImport).mockResolvedValue({ name: 'pdf', old_commit: null, new_commit: 'abc', changed: true });
});
afterEach(cleanup);
async function resolve(candidates: SkillCandidate[]) {
  vi.mocked(api.resolveImport).mockResolvedValue({ source: 'github:test/repo', git_ref: 'main', commit: 'abcdef123456', candidates });
  render(<Imports revision={0} changed={changed} reviewDraft={reviewDraft} />);
  fireEvent.click(screen.getByRole('button', { name: 'Add Import' }));
  fireEvent.change(await screen.findByLabelText('Link'), { target: { value: 'github:test/repo' } });
  fireEvent.click(screen.getByRole('button', { name: 'Resolve' }));
  await screen.findByText('github:test/repo');
}
const confirm = () => screen.getByRole('button', { name: 'Add into Draft' }) as HTMLButtonElement;
it('prefills a single candidate and adds it into Draft', async () => {
  await resolve([candidate('pdf')]);
  expect((screen.getByLabelText('Name') as HTMLInputElement).value).toBe('pdf');
  expect(screen.getByText('About pdf')).toBeTruthy();
  fireEvent.click(confirm());
  fireEvent.click(await screen.findByRole('button', { name: 'Review in Draft' }));
  expect(api.addImport).toHaveBeenCalledWith({ name: 'pdf', source: 'github:test/repo', path: 'pdf', ref: 'main', commit: 'abcdef123456' });
  expect(changed).toHaveBeenCalledOnce(); expect(reviewDraft).toHaveBeenCalledOnce();
});
it('leaves all candidates unchecked for a repository', async () => {
  await resolve([candidate('pdf'), candidate('writing')]);
  for (const checkbox of screen.getAllByRole('checkbox')) expect(checkbox.getAttribute('aria-checked')).toBe('false');
  expect(confirm().disabled).toBe(true);
  expect(screen.queryByLabelText('Name for pdf')).toBeNull();
});
it('blocks taken, inventory, duplicate and invalid names', async () => {
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:existing', in_baseline: true, draft: null, unpushed: false, description: null }]);
  await resolve([candidate('pdf', true), candidate('writing')]);
  fireEvent.click(screen.getByRole('checkbox', { name: 'Select pdf' }));
  expect(confirm().disabled).toBe(true);
  const name = screen.getByLabelText('Name for pdf');
  for (const value of ['', '/invalid', 'existing']) {
    fireEvent.change(name, { target: { value } }); expect(confirm().disabled).toBe(true);
  }
  fireEvent.change(name, { target: { value: 'writing' } }); expect(confirm().disabled).toBe(false);
  fireEvent.click(screen.getByRole('checkbox', { name: 'Select writing' })); expect(confirm().disabled).toBe(true);
});
it('adds sequentially and stops at the first error without retrying', async () => {
  let finish!: () => void;
  vi.mocked(api.addImport).mockImplementationOnce(() => new Promise(resolve => { finish = () => resolve({ name: 'one', old_commit: null, new_commit: 'abc', changed: true }); }))
    .mockRejectedValueOnce('Access denied');
  await resolve([candidate('one'), candidate('two'), candidate('three')]);
  for (const name of ['one', 'two', 'three']) fireEvent.click(screen.getByRole('checkbox', { name: `Select ${name}` }));
  fireEvent.click(confirm());
  expect(api.addImport).toHaveBeenCalledTimes(1);
  expect((screen.getByRole('button', { name: 'Change source' }) as HTMLButtonElement).disabled).toBe(true);
  finish();
  await screen.findByText(/Access denied.*Items listed as added stay/);
  await waitFor(() => expect(api.addImport).toHaveBeenCalledTimes(2));
  expect(screen.getByText('one added into Draft.')).toBeTruthy();
  expect(screen.getByText('two may not have been added; check the Draft.')).toBeTruthy();
  expect(changed).toHaveBeenCalledOnce();
});
it('refreshes even when the first add fails, since its outcome is unknown', async () => {
  vi.mocked(api.addImport).mockRejectedValueOnce('connection restore');
  await resolve([candidate('pdf')]);
  fireEvent.click(confirm());
  await screen.findByText('pdf may not have been added; check the Draft.');
  expect(api.addImport).toHaveBeenCalledOnce(); expect(changed).toHaveBeenCalledOnce();
});
it('lets a deleted Draft item\'s name be reused', async () => {
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:pdf', in_baseline: true, draft: 'deleted', unpushed: false, description: null }]);
  vi.mocked(api.imports).mockResolvedValue({ checked_at: null, updates: [{ name: 'pdf', source: 'github:test/repo', ref: 'main', locked: 'a', upstream: 'b' }], errors: [] });
  await resolve([candidate('pdf')]);
  expect(confirm().disabled).toBe(false);
});

it('changes the source while preserving Link and Path and clearing candidates', async () => {
  vi.mocked(api.resolveImport).mockResolvedValue({ source: 'github:test/repo', git_ref: 'main', commit: 'abcdef123456', candidates: [candidate('pdf')] });
  render(<Imports revision={0} changed={changed} reviewDraft={reviewDraft} />);
  fireEvent.click(screen.getByRole('button', { name: 'Add Import' }));
  fireEvent.change(await screen.findByLabelText('Link'), { target: { value: 'github:test/repo' } });
  fireEvent.click(screen.getByRole('button', { name: 'Advanced' }));
  fireEvent.change(await screen.findByLabelText('Path (subdirectory for other git URLs)'), { target: { value: 'skills/pdf' } });
  fireEvent.click(screen.getByRole('button', { name: 'Resolve' }));
  await screen.findByLabelText('Name');
  fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'renamed' } });
  fireEvent.click(screen.getByRole('button', { name: 'Change source' }));
  expect((screen.getByLabelText('Link') as HTMLInputElement).value).toBe('github:test/repo');
  expect(screen.queryByLabelText('Name')).toBeNull(); expect(screen.queryByRole('button', { name: 'Add into Draft' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Advanced' }));
  expect((await screen.findByLabelText('Path (subdirectory for other git URLs)') as HTMLInputElement).value).toBe('skills/pdf');
  vi.mocked(api.resolveImport).mockResolvedValue({ source: 'github:test/other', git_ref: 'main', commit: 'new', candidates: [candidate('writing')] });
  fireEvent.change(screen.getByLabelText('Link'), { target: { value: 'github:test/other' } });
  fireEvent.click(screen.getByRole('button', { name: 'Resolve' }));
  expect((await screen.findByLabelText('Name') as HTMLInputElement).value).toBe('writing');
  expect(api.resolveImport).toHaveBeenLastCalledWith({ link: 'github:test/other', path: 'skills/pdf' });
});
it.each([[true, 'On'], [false, 'Off'], [null, 'Default']] as const)('names the restored %s value and Assignment in success feedback', async (before, label) => {
  vi.mocked(api.log).mockResolvedValue([{ id: 7, machine: 'laptop', harness: 'claude', kind: 'skill', name: 'pdf', before, after: !before, at: 1, author: 'user', cause: { type: 'change' } }]);
  vi.mocked(api.undo).mockResolvedValue({ assignment: { harnesses: {} }, revision: 2, entries: [] });
  render(<Log revision={0} changed={changed} />);
  fireEvent.click(await screen.findByRole('button', { name: `Restore ${label} for log entry 7: skill:pdf on laptop / claude` }));
  await waitFor(() => expect(toast).toHaveBeenCalledWith({ title: `Restored Assignment skill:pdf on laptop / claude to ${label}.` }));
  expect(screen.getByRole('button', { name: `Restore ${label} for log entry 7: skill:pdf on laptop / claude` }).textContent).toContain('Restored');
  expect(api.undo).toHaveBeenCalledWith(7); expect(changed).toHaveBeenCalledOnce();
});
it.each([false, true])('names the item, Harness and path in the %s recovery confirmation', async displaced => {
  const entry = { id: 'recovered', name: 'pdf', harness: 'claude', dest: '/claude/skills/pdf', path: '/recovery/pdf', adoptable: true, reason: null, items: ['skill:pdf'] };
  vi.mocked(api.conflicts).mockResolvedValue(displaced ? [] : [entry]);
  vi.mocked(api.recovered).mockResolvedValue(displaced ? [entry] : []);
  render(<Conflicts revision={0} changed={changed} reviewDraft={reviewDraft} />);
  fireEvent.click(await screen.findByRole('button', { name: `${displaced ? 'Discard' : 'Overwrite'} pdf in claude` }));
  expect(screen.getByRole('dialog').textContent).toContain(displaced ? '/recovery/pdf' : '/claude/skills/pdf');
  expect(screen.getByRole('heading', { name: `${displaced ? 'Discard' : 'Overwrite'} pdf in claude?` })).toBeTruthy();
});

it.each([1, 2, 0])('focuses resolved results and announces %s Skills found', async count => {
  await resolve(Array.from({ length: count }, (_, i) => candidate(`skill${i}`)));
  const target = count === 1 ? screen.getByLabelText('Name') : count ? screen.getByRole('checkbox', { name: 'Select skill0' }) : screen.getByRole('heading', { name: 'Resolved Skills' });
  await waitFor(() => expect(document.activeElement).toBe(target));
  const announcement = within(screen.getByRole('dialog')).getByRole('status');
  expect(announcement.textContent).toBe(count ? `${count} ${count === 1 ? 'Skill' : 'Skills'} found` : '');
  if (!count) {
    expect(screen.getByRole('alert').textContent).toContain('No Skills found at this source.');
    expect(screen.queryByText('0 Skills found')).toBeNull();
  }
  expect(announcement.getAttribute('aria-live')).toBe('polite');
  fireEvent.click(screen.getByRole('button', { name: 'Change source' }));
  await waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText('Link')));
  expect(announcement.textContent).toBe('');
});
it('associates name errors with their input and clears the description after correction', async () => {
  await resolve([candidate('pdf')]);
  const name = screen.getByLabelText('Name');
  fireEvent.change(name, { target: { value: '' } });
  const errorId = name.getAttribute('aria-describedby')!;
  expect(document.getElementById(errorId)?.textContent).toBe('Enter a name.');
  expect(document.getElementById(errorId)?.getAttribute('data-slot')).toBe('field-error');
  expect(name.getAttribute('aria-invalid')).toBe('true');
  fireEvent.change(name, { target: { value: 'renamed' } });
  expect(name.hasAttribute('aria-describedby')).toBe(false);
  expect(name.getAttribute('aria-invalid')).toBe('false');
});

it('describes join errors from the submitted fields without losing existing hints', async () => {
  vi.mocked(api.harnesses).mockResolvedValue([]);
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abc', machines: [] });
  vi.mocked(api.join).mockRejectedValue('Join code expired');
  const status = { server_url: 'https://skills.example.com', machine_id: 'laptop', connection: 'connected', auth_rejected_since: null } as StatusView;
  render(<Settings revision={0} status={status} socket="/daemon.sock" changed={changed} />);
  fireEvent.click(screen.getByRole('button', { name: 'Rejoin this Machine…' }));
  const code = await screen.findByLabelText('Join code');
  fireEvent.change(code, { target: { value: 'expired' } });
  fireEvent.submit(code.closest('form')!);
  const dialog = await screen.findByRole('dialog');
  fireEvent.click(within(dialog).getByRole('button', { name: 'Join / rejoin' }));
  await waitFor(() => expect(code.getAttribute('aria-describedby')).toBe('join-error'));
  expect(document.getElementById('join-error')?.textContent).toBe('Join code expired');
  expect(document.getElementById('join-error')?.getAttribute('data-slot')).toBe('field-error');
  expect(screen.getByLabelText('Machine id').getAttribute('aria-describedby')).toBe('machine-id-hint join-error');
});

it('focuses Review in Draft when adding finishes and announces only the latest success', async () => {
  let finish!: () => void;
  vi.mocked(api.addImport).mockImplementationOnce(() => new Promise(resolve => { finish = () => resolve({ name: 'one', old_commit: null, new_commit: 'abc', changed: true }); }));
  await resolve([candidate('one'), candidate('two')]);
  for (const name of ['one', 'two']) fireEvent.click(screen.getByRole('checkbox', { name: `Select ${name}` }));
  confirm().focus(); fireEvent.click(confirm());
  finish();
  const review = await screen.findByRole('button', { name: 'Review in Draft' });
  await waitFor(() => expect(document.activeElement).toBe(review));
  const announcement = within(screen.getByRole('dialog')).getByRole('status');
  expect(announcement.textContent).toBe('Added two into Draft.');
  expect(screen.getByText('one added into Draft.').closest('[role=status]')).toBeNull();
  expect(screen.getByText('two added into Draft.').closest('[role=status]')).toBeNull();
});
it('focuses the error summary when an import batch stops', async () => {
  vi.mocked(api.addImport).mockRejectedValueOnce('Access denied');
  await resolve([candidate('pdf')]);
  confirm().focus(); fireEvent.click(confirm());
  await screen.findByRole('button', { name: 'Review in Draft' });
  await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('alert').parentElement));
  expect(within(screen.getByRole('dialog')).getByRole('status').textContent).toBe('');
});
it('opens Add Import directly when requested from Skills', async () => {
  render(<Imports revision={0} changed={changed} reviewDraft={reviewDraft} openAdd />);
  expect(await screen.findByRole('dialog', { name: 'Add Import' })).toBeTruthy();
  await waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText('Link')));
});
it('returns focus to Create join code on close', async () => {
  vi.mocked(api.joinCode).mockResolvedValue({ code: 'single-use' });
  render(<JoinCodeButton label="Create join code" />);
  const trigger = screen.getByRole('button', { name: 'Create join code' });
  trigger.focus(); fireEvent.click(trigger);
  const dialog = await screen.findByRole('dialog');
  fireEvent.keyDown(dialog, { key: 'Escape' });
  await waitFor(() => expect(document.activeElement).toBe(trigger));
});
it('keeps Restore focus during the request and guards repeated clicks', async () => {
  let finish!: () => void;
  vi.mocked(api.log).mockResolvedValue([{ id: 7, machine: 'laptop', harness: 'claude', kind: 'skill', name: 'pdf', before: true, after: false, at: 1, author: 'user', cause: { type: 'change' } }]);
  vi.mocked(api.undo).mockImplementation(() => new Promise(resolve => { finish = () => resolve({ assignment: { harnesses: {} }, revision: 2, entries: [] }); }));
  render(<Log revision={0} changed={changed} />);
  const restore = await screen.findByRole('button', { name: /Restore On for log entry 7/ });
  restore.focus(); fireEvent.click(restore);
  expect(document.activeElement).toBe(restore);
  expect(restore.hasAttribute('disabled')).toBe(false);
  expect(restore.getAttribute('aria-disabled')).toBe('true');
  fireEvent.click(restore); expect(api.undo).toHaveBeenCalledOnce();
  finish();
  await waitFor(() => expect(toast).toHaveBeenCalled());
  expect(document.activeElement).toBe(restore);
});
it.each([false, true])('focuses the log region when the restored row is gone and empty is %s', async empty => {
  vi.mocked(api.log).mockResolvedValue([{ id: 7, machine: 'laptop', harness: 'claude', kind: 'skill', name: 'pdf', before: true, after: false, at: 1, author: 'user', cause: { type: 'change' } }]);
  vi.mocked(api.undo).mockResolvedValue({ assignment: { harnesses: {} }, revision: 2, entries: [] });
  const view = render(<Log revision={0} changed={changed} />);
  fireEvent.click(await screen.findByRole('button', { name: /Restore On for log entry 7/ }));
  await waitFor(() => expect(toast).toHaveBeenCalled());
  vi.mocked(api.log).mockResolvedValue(empty ? [] : [{ id: 8, machine: 'laptop', harness: 'claude', kind: 'skill', name: 'other', before: true, after: false, at: 2, author: 'user', cause: { type: 'change' } }]);
  view.rerender(<Log revision={1} changed={changed} />);
  if (empty) await screen.findByText('No log entries');
  else await screen.findByRole('button', { name: /Restore On for log entry 8/ });
  expect(screen.getByRole('region', { name: 'Assignment log' })).toBeTruthy();
});
it('lists all Imports, checks now and removes into Draft', async () => {
  const imports = [{ name: 'pdf', source: 'github:test/repo', ref: 'main', path: 'skills/pdf', locked: 'abcdef123' }, { name: 'writing', source: 'github:test/repo', ref: 'main', path: '', locked: '123456789' }];
  vi.mocked(api.imports).mockResolvedValue({ imports, checked_at: null, updates: [], errors: [] });
  vi.mocked(api.checkImports).mockResolvedValue({ imports, checked_at: 1, updates: [{ ...imports[0], upstream: 'fedcba321' }], errors: [{ name: 'writing', message: 'Access denied' }] });
  vi.mocked(api.removeImport).mockResolvedValue(undefined);
  render(<Imports revision={0} changed={changed} reviewDraft={reviewDraft} />);
  await screen.findByRole('table', { name: 'Imports' });
  expect(screen.getAllByText('Up to date')).toHaveLength(2);
  fireEvent.click(screen.getByRole('button', { name: 'Check now' }));
  await screen.findByText(/Update available/); await screen.findByText(/Check failed/);
  fireEvent.click(screen.getByRole('button', { name: 'Remove pdf' }));
  fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Remove' }));
  await screen.findByText('pdf removal is in the Draft');
  expect(api.removeImport).toHaveBeenCalledWith('pdf'); expect(changed).toHaveBeenCalledOnce();
});
it('loads conflict diffs on expansion and uses recovery ids', async () => {
  const entry = { name: 'pdf', harness: 'claude', dest: '/skills/pdf', adoptable: true, reason: null, items: ['skill:pdf'] };
  vi.mocked(api.conflicts).mockResolvedValue([entry]);
  vi.mocked(api.recovered).mockResolvedValue([{ ...entry, id: 'recovery', path: '/recovery/pdf' }]);
  render(<Conflicts revision={0} changed={changed} reviewDraft={reviewDraft} />);
  const summaries = await screen.findAllByText('Show diff');
  expect(api.conflictDiff).not.toHaveBeenCalled();
  fireEvent.click(summaries[0]);
  await screen.findByLabelText('Conflict diff');
  expect(api.conflictDiff).toHaveBeenCalledWith({ harness: 'claude', name: 'pdf' });
  fireEvent.click(summaries[1]);
  await waitFor(() => expect(api.conflictDiff).toHaveBeenCalledWith({ id: 'recovery' }));
  expect(screen.getAllByText('+draft')[0].getAttribute('data-kind')).toBe('add');
});
it('automatically loads the sole conflict diff', async () => {
  vi.mocked(api.conflicts).mockResolvedValue([{ name: 'pdf', harness: 'claude', dest: '/pdf', adoptable: true, reason: null, items: [] }]);
  vi.mocked(api.recovered).mockResolvedValue([]);
  render(<Conflicts revision={0} changed={changed} reviewDraft={reviewDraft} />);
  await screen.findByLabelText('Conflict diff'); expect(api.conflictDiff).toHaveBeenCalledOnce();
});
it('filters the loaded log page by machine, Harness, author and item', async () => {
  const base = { kind: 'skill' as const, before: true, after: false, at: 1, cause: { type: 'change' as const } };
  vi.mocked(api.log).mockResolvedValue([{ ...base, id: 1, machine: 'laptop', harness: 'claude', author: 'alice', name: 'pdf' }, { ...base, id: 2, machine: 'desktop', harness: 'codex', author: 'bob', name: 'writing' }]);
  render(<Log revision={0} changed={changed} />);
  await screen.findByText('skill:pdf');
  for (const [label, value] of [['Filter by machine', 'laptop'], ['Filter by harness', 'claude'], ['Filter by author', 'alice'], ['Search items', 'PDF']]) {
    fireEvent.change(screen.getByLabelText(label), { target: { value } });
    expect(screen.getByText('skill:pdf')).toBeTruthy(); expect(screen.queryByText('skill:writing')).toBeNull();
    fireEvent.change(screen.getByLabelText(label), { target: { value: '' } });
  }
  expect(screen.queryByText('change')).toBeNull(); expect(api.log).toHaveBeenCalledOnce();
});
