// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { Conflicts } from './Conflicts';
import { api } from './api';
import type { ConflictView } from './types';

vi.mock('./api', () => ({ api: { conflicts: vi.fn(), recovered: vi.fn(), conflictDiff: vi.fn(), adopt: vi.fn(), overwrite: vi.fn(), discardDisplaced: vi.fn() }, errorMessage: (e: unknown) => String(e) }));
vi.mock('./toast', () => ({ toast: vi.fn() }));
const entry = (name = 'pdf'): ConflictView => ({ name, harness: 'claude', dest: `/skills/${name}`, adoptable: true, reason: null, items: [] });
const recovered = { ...entry(), id: 'saved', path: '/recovery/pdf', displaced_at: 1 };
const props = { revision: 0, changed: vi.fn(), reviewDraft: vi.fn() };
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.conflicts).mockResolvedValue([]);
  vi.mocked(api.recovered).mockResolvedValue([]);
  vi.mocked(api.conflictDiff).mockResolvedValue({ item: 'skill:pdf', diff: '-edit\n+draft' });
});
afterEach(cleanup);
it('shows one empty state when both lists are empty', async () => {
  render(<Conflicts {...props} />);
  await screen.findByText('No Conflicts');
  expect(screen.getByText("Every Harness matches this Machine's Draft.")).toBeTruthy();
  expect(screen.queryByRole('heading', { name: 'Harness conflicts' })).toBeNull();
  expect(screen.queryByRole('heading', { name: 'Recovered content' })).toBeNull();
});
it.each([false, true])('hides the empty section when recovered is %s', async saved => {
  vi.mocked(saved ? api.recovered : api.conflicts).mockResolvedValue([saved ? recovered : entry()]);
  render(<Conflicts {...props} />);
  const heading = await screen.findByRole('heading', { name: saved ? 'Recovered content' : 'Harness conflicts' });
  expect(heading.tagName).toBe('H2');
  expect(screen.queryByRole('heading', { name: saved ? 'Harness conflicts' : 'Recovered content' })).toBeNull();
  expect(screen.queryByText('No Conflicts')).toBeNull();
});
it.each([1, 2, 3, 4])('defaults diffs using the combined total of %s entries', async total => {
  vi.mocked(api.conflicts).mockResolvedValue(Array.from({ length: total - 1 }, (_, i) => entry(`skill${i}`)));
  vi.mocked(api.recovered).mockResolvedValue([recovered]);
  render(<Conflicts {...props} />);
  const triggers = await screen.findAllByRole('button', { name: total <= 3 ? 'Hide diff' : 'Show diff' });
  expect(triggers).toHaveLength(total);
  expect(triggers[0].getAttribute('aria-expanded')).toBe(String(total <= 3));
  if (total <= 3) await waitFor(() => expect(api.conflictDiff).toHaveBeenCalledTimes(total));
  else expect(api.conflictDiff).not.toHaveBeenCalled();
  fireEvent.click(triggers[0]);
  expect(triggers[0].textContent).toContain(total <= 3 ? 'Show diff' : 'Hide diff');
});
it('waits for both lists before choosing the initial diff state', async () => {
  let finish!: (entries: ConflictView[]) => void;
  vi.mocked(api.conflicts).mockResolvedValue([entry()]);
  vi.mocked(api.recovered).mockImplementation(() => new Promise(resolve => { finish = resolve; }));
  render(<Conflicts {...props} />);
  await waitFor(() => expect(api.recovered).toHaveBeenCalled());
  expect(api.conflictDiff).not.toHaveBeenCalled();
  finish([recovered, { ...recovered, id: 'two' }, { ...recovered, id: 'three' }]);
  expect(await screen.findAllByRole('button', { name: 'Show diff' })).toHaveLength(4);
  expect(api.conflictDiff).not.toHaveBeenCalled();
});
it('confirms Overwrite before acting', async () => {
  vi.mocked(api.conflicts).mockResolvedValue([entry()]);
  render(<Conflicts {...props} />);
  fireEvent.click(await screen.findByRole('button', { name: 'Overwrite pdf in claude' }));
  expect(screen.getByRole('dialog').textContent).toContain('/skills/pdf');
  expect(api.overwrite).not.toHaveBeenCalled();
});
it('explains recovery and confirms permanent discard', async () => {
  vi.mocked(api.recovered).mockResolvedValue([recovered]);
  render(<Conflicts {...props} />);
  fireEvent.click(await screen.findByRole('button', { name: 'Discard pdf in claude' }));
  expect(screen.getByRole('dialog').textContent).toContain('Permanently remove recovered content');
  expect(api.discardDisplaced).not.toHaveBeenCalled();
  const when = document.querySelector('time');
  expect(when?.getAttribute('datetime')).toBe('1970-01-01T00:00:01.000Z');
  expect(when?.getAttribute('tabindex')).toBe('0'); // keyboard users can reach the exact time
});

it.each([0, 1730000000, null, undefined])('shows edited time only for Harness conflicts with numeric timestamp %s', async detected_at => {
  vi.mocked(api.conflicts).mockResolvedValue([{ ...entry(), detected_at }]);
  vi.mocked(api.recovered).mockResolvedValue([{ ...recovered, detected_at }]);
  render(<Conflicts {...props} />); await screen.findByRole('heading', { name: 'Harness conflicts' });
  const edited = screen.queryByText(/^Edited/);
  if (typeof detected_at === 'number') {
    expect(edited).toBeTruthy(); expect(edited?.querySelector('time')?.getAttribute('datetime')).toBe(new Date(detected_at * 1000).toISOString());
    expect(screen.getAllByText(/^Edited/)).toHaveLength(1);
  } else expect(edited).toBeNull();
});
