// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { Imports } from './Activity';
import { api } from './api';
import type { SkillCandidate } from './types';

vi.mock('./api', () => ({ api: { imports: vi.fn(), items: vi.fn(), resolveImport: vi.fn(), addImport: vi.fn() },
  errorMessage: (e: unknown) => String(e) }));
const changed = vi.fn();
const reviewDraft = vi.fn();
const candidate = (name: string, taken = false): SkillCandidate => ({ path: name, default_name: name, description: `About ${name}`, taken });
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.imports).mockResolvedValue({ checked_at: null, updates: [], errors: [] });
  vi.mocked(api.items).mockResolvedValue([]);
  vi.mocked(api.addImport).mockResolvedValue({ name: 'pdf', old_commit: null, new_commit: 'abc', changed: true });
});
afterEach(cleanup);
async function resolve(candidates: SkillCandidate[]) {
  vi.mocked(api.resolveImport).mockResolvedValue({ source: 'github:test/repo', git_ref: 'main', commit: 'abcdef123456', candidates });
  render(<Imports revision={0} changed={changed} reviewDraft={reviewDraft} />);
  fireEvent.click(screen.getByRole('button', { name: 'Add' }));
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
  finish();
  await screen.findByText(/Access denied.*Items listed as added stay/);
  await waitFor(() => expect(api.addImport).toHaveBeenCalledTimes(2));
  expect(screen.getByText('one added into Draft.')).toBeTruthy();
  expect(screen.getByText('two may not have been added; check the Draft.')).toBeTruthy();
  expect(changed).toHaveBeenCalledOnce();
});
it('refreshes even when the first add fails, since its outcome is unknown', async () => {
  vi.mocked(api.addImport).mockRejectedValueOnce('connection reset');
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
