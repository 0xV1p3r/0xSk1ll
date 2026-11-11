// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { Log } from './Log';
import { api } from './api';
import type { LogCause, LogEntry } from './types';
vi.mock('./api', () => ({ api: { log: vi.fn(), logFacets: vi.fn(), fleet: vi.fn() }, errorMessage: String }));
afterEach(() => { cleanup(); vi.resetAllMocks(); });
const entry = (id: number, cause: LogCause = { type: 'change' }): LogEntry => ({ id, machine: 'laptop', harness: 'claude', name: 'pdf', kind: 'skill', before: true, after: false, at: 1, author: 'alice', cause });
it('queries filters on the server and resets pagination', async () => {
  vi.mocked(api.log).mockImplementation(async query => query.item ? [] : Array.from({ length: 50 }, (_, i) => entry((query.before ?? 101) - 1 - i)));
  vi.mocked(api.logFacets).mockRejectedValue(new Error('unavailable'));
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
  render(<Log revision={0} changed={() => {}} />);
  await screen.findByText('#100');
  expect(screen.getByRole('option', { name: 'laptop' })).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Older' }));
  await screen.findByText('#50');
  expect(screen.getByText('Page 2')).toBeTruthy();
  expect(api.log).toHaveBeenLastCalledWith({ before: 51, limit: 50, machine: '', harness: '', author: '', item: '' });
  fireEvent.change(screen.getByLabelText('Search items'), { target: { value: 'missing' } });
  await screen.findByText('No entries match these filters');
  expect(api.log).toHaveBeenLastCalledWith({ before: undefined, limit: 50, machine: '', harness: '', author: '', item: 'missing' });
  expect(screen.queryByText('Page 2')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Clear filters' }));
  await screen.findByText('#100');
  expect(screen.getByText('Page 1')).toBeTruthy();
});
it('loads global facets and refreshes them with revision', async () => {
  vi.mocked(api.log).mockResolvedValue([]);
  vi.mocked(api.logFacets).mockResolvedValue({ machines: ['remote'], harnesses: ['codex'], authors: ['bob'] });
  const view = render(<Log revision={0} changed={() => {}} />);
  await screen.findByRole('option', { name: 'remote' });
  view.rerender(<Log revision={1} changed={() => {}} />);
  await waitFor(() => expect(api.logFacets).toHaveBeenCalledTimes(2));
});
it('shows entry IDs and readable causes', async () => {
  const causes: LogCause[] = [{ type: 'undo', entry: 101 }, { type: 'item_removed' }, { type: 'default_materialized' }, { type: 'machine_removed' }, { type: 'harness_added' }];
  vi.mocked(api.log).mockResolvedValue(causes.map((cause, i) => entry(105 - i, cause)));
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
  render(<Log revision={0} changed={() => {}} />);
  await screen.findByText('#101');
  for (const label of ['Restored #101', 'Item removed', 'Default applied', 'Machine removed', 'Harness added']) expect(screen.getAllByText(label).length).toBeGreaterThan(0);
});
it('holds pagination until a typed search is applied, so a stale cursor is never reused', async () => {
  vi.mocked(api.log).mockImplementation(async query => Array.from({ length: 50 }, (_, i) => entry((query.before ?? 101) - 1 - i)));
  vi.mocked(api.logFacets).mockResolvedValue({ machines: [], harnesses: [], authors: [] });
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
  render(<Log revision={0} changed={() => {}} />);
  await screen.findByText('#100');
  fireEvent.change(screen.getByLabelText('Search items'), { target: { value: 'pdf' } });
  expect((screen.getByRole('button', { name: 'Older' }) as HTMLButtonElement).disabled).toBe(true);
  await waitFor(() => expect(api.log).toHaveBeenLastCalledWith(expect.objectContaining({ item: 'pdf', before: undefined })));
  await waitFor(() => expect((screen.getByRole('button', { name: 'Older' }) as HTMLButtonElement).disabled).toBe(false));
});
