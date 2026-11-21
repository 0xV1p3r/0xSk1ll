// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Matrix } from './Matrix';
import { api } from './api';
import { emptyHarness } from './model';
import { toast } from './toast';
import type { Assignment, AssignmentUpdate, StatusView } from './types';

vi.mock('./api', () => ({ api: {
  fleet: vi.fn(), items: vi.fn(), orphans: vi.fn(), pruneOrphans: vi.fn(), assignment: vi.fn(), setAssignment: vi.fn(),
}, errorMessage: (error: unknown) => error instanceof Error ? error.message : String(error),
errorStatus: (error: unknown) => (error as { status?: number }).status }));
vi.mock('./toast', () => ({ toast: vi.fn() }));
const status = { machine_id: 'a' } as StatusView;
const assignment = (enabled = false): Assignment => ({ harnesses: { claude: {
  ...emptyHarness(), new_skills_default: false, skills: { pdf: enabled },
} } });
const snapshot = (enabled = false, revision = 1): AssignmentUpdate => ({ assignment: assignment(enabled), revision, entries: [] });
const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
};
const cell = (machine: string) => screen.getByRole('checkbox', { name: `skill:pdf in ${machine} / claude, explicit` });
const enabled = (machine: string) => cell(machine).getAttribute('aria-checked') === 'true';
beforeEach(() => {
  vi.resetAllMocks();
  localStorage.clear();
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abc', machines: ['a', 'b'].map(id => ({
    id, status: 'in_sync', connected: true, joined_at: 1, last_seen: 1, report: null, registration: `r-${id}`,
  })) });
  vi.mocked(api.orphans).mockResolvedValue({ orphans: [] });
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:pdf', id: 'pdf', in_baseline: true, draft: null, unpushed: false, description: null }]);
  vi.mocked(api.assignment).mockResolvedValue(snapshot());
  vi.mocked(api.setAssignment).mockResolvedValue(snapshot());
});
afterEach(cleanup);
const mount = async () => {
  const view = render(<Matrix revision={0} status={status} />);
  await screen.findByRole('checkbox', { name: 'skill:pdf in a / claude, explicit' });
  // The write hook records the loaded snapshot in a passive effect.
  await act(async () => {});
  return view;
};

describe('Assignment response cache', () => {
  it('loads and refreshes assignments through GET without writing', async () => {
    const view = await mount();
    expect(api.assignment).toHaveBeenCalledWith('a');
    expect(api.assignment).toHaveBeenCalledWith('b');
    view.rerender(<Matrix revision={1} status={status} />);
    await waitFor(() => expect(api.assignment).toHaveBeenCalledTimes(4));
    expect(api.setAssignment).not.toHaveBeenCalled();
  });
  it('keeps concurrent successful writes when B supersedes A refresh and an older snapshot arrives', async () => {
    const a = deferred<AssignmentUpdate>(), b = deferred<AssignmentUpdate>();
    const oldA = deferred<AssignmentUpdate>(), oldB = deferred<AssignmentUpdate>();
    let reads = 0;
    vi.mocked(api.setAssignment).mockImplementation(machine => machine === 'a' ? a.promise : b.promise);
    vi.mocked(api.assignment).mockImplementation(machine => {
      reads++;
      if (reads <= 2) return Promise.resolve(snapshot());
      if (reads <= 4) return machine === 'a' ? oldA.promise : oldB.promise;
      // Sent after both writes were answered, so the Server has them.
      return Promise.resolve(snapshot(true, machine === 'b' ? 3 : 2));
    });
    await mount();
    fireEvent.click(cell('a')); fireEvent.click(cell('b'));
    expect(enabled('a') && enabled('b')).toBe(true);
    await act(async () => { a.resolve(snapshot(true, 2)); });
    await waitFor(() => expect(reads).toBeGreaterThanOrEqual(4));
    await act(async () => { b.resolve(snapshot(true, 3)); });
    await waitFor(() => expect(reads).toBeGreaterThanOrEqual(6));
    expect(enabled('a') && enabled('b')).toBe(true);
    await act(async () => { oldA.resolve(snapshot(false, 1)); oldB.resolve(snapshot(false, 1)); });
    expect(enabled('a') && enabled('b')).toBe(true);
    expect(vi.mocked(api.setAssignment).mock.calls.filter(([, r]) => r.changes.length)).toHaveLength(2);
  });
  it('ignores a mutation response older than a snapshot already accepted for that machine', async () => {
    const save = deferred<AssignmentUpdate>();
    let reads = 0;
    vi.mocked(api.setAssignment).mockReturnValue(save.promise);
    vi.mocked(api.assignment).mockImplementation(async machine => {
      reads++;
      return snapshot(false, reads > 2 && machine === 'a' ? 4 : 1);
    });
    const view = await mount();
    fireEvent.click(cell('a'));
    view.rerender(<Matrix revision={1} status={status} />);
    await waitFor(() => expect(reads).toBeGreaterThanOrEqual(4));
    await act(async () => { save.resolve(snapshot(true, 2)); });
    expect(enabled('a')).toBe(false);
  });
  it('surfaces refresh failures while retaining the successful mutation response', async () => {
    let reads = 0;
    vi.mocked(api.setAssignment).mockResolvedValue(snapshot(true, 2));
    vi.mocked(api.assignment).mockImplementation(async () => {
      if (++reads > 2) throw new Error('refresh unavailable');
      return snapshot();
    });
    await mount(); fireEvent.click(cell('a'));
    await screen.findByText('a: refresh unavailable');
    expect(screen.getByText('b: refresh unavailable')).toBeTruthy();
    expect(enabled('a')).toBe(true);
    await waitFor(() => expect(cell('a').hasAttribute('data-disabled')).toBe(false));
  });
  it('rolls back a failed item write and allows a retry', async () => {
    vi.mocked(api.setAssignment).mockRejectedValue(new Error('write rejected'));
    await mount(); fireEvent.click(cell('a'));
    await screen.findByText('a: write rejected');
    expect(enabled('a')).toBe(false);
    fireEvent.click(cell('a'));
    await waitFor(() => expect(vi.mocked(api.setAssignment).mock.calls.filter(([, r]) => r.changes.length)).toHaveLength(2));
  });
  it('sends only the default change and renders materialized values from the Server response', async () => {
    const save = deferred<AssignmentUpdate>();
    vi.mocked(api.setAssignment).mockReturnValue(save.promise);
    await mount();
    fireEvent.click(screen.getByRole('switch', { name: 'Default for new skills in a / claude' }));
    expect(enabled('a')).toBe(false);
    expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'new_skills_default', enabled: true }], expected: { revision: 1, registration: 'r-a' } });
    const next = snapshot(false, 2); next.assignment.harnesses.claude.new_skills_default = true;
    vi.mocked(api.assignment).mockResolvedValue(next);
    await act(async () => { save.resolve(next); });
    expect(screen.getByRole('switch', { name: 'Default for new skills in a / claude' }).getAttribute('aria-checked')).toBe('true');
    expect(enabled('a')).toBe(false);
  });
  it('labels unpushed Commits and Draft-only additions consistently', async () => {
    vi.mocked(api.items).mockResolvedValue([
      { item: 'skill:queued', id: 'queued', in_baseline: false, draft: null, unpushed: true, description: null },
      { item: 'skill:local', id: 'local', in_baseline: false, draft: 'added', unpushed: false, description: null },
    ]);
    render(<Matrix revision={0} status={status} />);
    const queued = (await screen.findByText('queued')).closest('tr')!;
    const local = screen.getByText('local').closest('tr')!;
    expect(within(queued).getByText('Unpushed')).toBeTruthy();
    expect(within(local).getByText('Draft only')).toBeTruthy();
    expect(screen.queryByText(/not committed/i)).toBeNull();
  });
});

it('focuses and highlights the requested local cell after loading without changing its Assignment', async () => {
  const scroll = vi.fn();
  const previous = HTMLElement.prototype.scrollIntoView;
  HTMLElement.prototype.scrollIntoView = scroll;
  try {
    const target = { item: 'skill:pdf', harness: 'claude' };
    const view = render(<Matrix revision={0} status={status} target={target} />);
    await waitFor(() => expect(document.activeElement).toBe(cell('a')));
    expect(cell('a').closest('td')?.getAttribute('data-highlighted')).toBe('true');
    expect(scroll).toHaveBeenCalledWith({ block: 'center', inline: 'nearest' });
    expect(api.setAssignment).not.toHaveBeenCalled();
    cell('b').focus();
    view.rerender(<Matrix revision={1} status={status} target={target} />);
    await waitFor(() => expect(api.assignment).toHaveBeenCalledTimes(4));
    expect(document.activeElement).toBe(cell('b'));
  } finally { HTMLElement.prototype.scrollIntoView = previous; }
});

describe('a Machine whose Assignment fails to load', () => {
  beforeEach(() => {
    vi.mocked(api.assignment).mockImplementation(async machine => {
      if (machine === 'b') throw new Error('Machine not found');
      return snapshot();
    });
  });
  it('keeps the other Machines usable and retries only its own column', async () => {
    await mount();
    expect(cell('a').hasAttribute('data-disabled')).toBe(false);
    expect(screen.queryByRole('checkbox', { name: /in b \// })).toBeNull();
    expect(screen.queryByRole('switch', { name: /in b \// })).toBeNull();
    expect(screen.getByText('Assignment not loaded: Machine not found').closest('[role=alert]')).toBeTruthy();
    vi.mocked(api.assignment).mockResolvedValue(snapshot(true, 2));
    fireEvent.click(screen.getByRole('button', { name: 'Retry loading the Assignment of b' }));
    await waitFor(() => expect(enabled('b')).toBe(true));
    expect(api.assignment).toHaveBeenLastCalledWith('b');
    expect(enabled('a')).toBe(false);
    expect(screen.queryByText(/Assignment not loaded/)).toBeNull();
  });
  it('keeps the error when a retry fails again', async () => {
    await mount();
    vi.mocked(api.assignment).mockRejectedValueOnce(new Error('Server unreachable'));
    fireEvent.click(screen.getByRole('button', { name: 'Retry loading the Assignment of b' }));
    expect(await screen.findByText('Assignment not loaded: Server unreachable')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Retry loading the Assignment of b' }).hasAttribute('disabled')).toBe(false);
  });
  it('sends bulk changes and their undo only for loaded Machines', async () => {
    const save = deferred<AssignmentUpdate>();
    vi.mocked(api.setAssignment).mockReturnValueOnce(save.promise).mockResolvedValueOnce(snapshot(false, 3));
    await mount();
    expect(screen.queryByText('Mixed')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
    vi.mocked(api.assignment).mockImplementation(async machine => {
      if (machine === 'b') throw new Error('Machine not found');
      return snapshot(true, 2);
    });
    await act(async () => { save.resolve(snapshot(true, 2)); });
    expect(toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Enabled pdf in 1 Harness' }));
    expect(api.setAssignment).toHaveBeenCalledTimes(1);
    expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: true }], expected: { revision: 1, registration: 'r-a' } });
    await act(async () => { vi.mocked(toast).mock.calls[0][0].action!.run(); });
    expect(api.setAssignment).toHaveBeenCalledTimes(2);
    expect(vi.mocked(api.setAssignment).mock.calls.every(([machine]) => machine === 'a')).toBe(true);
  });
  it('still shows a Machine without reported Harnesses so it can be retried', async () => {
    await mount();
    expect(screen.getByRole('columnheader', { name: 'b' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'b Harnesses' })).toBeNull();
  });
});

it('filters by description and kind and shows friendly names', async () => {
  vi.mocked(api.items).mockResolvedValue([
    { item: 'skill:pdf', id: 'pdf', in_baseline: true, draft: null, unpushed: false, description: 'Read documents' },
    { item: 'rule:safety', id: 'safety', in_baseline: true, draft: null, unpushed: false, description: 'Review changes' },
  ]);
  await mount();
  expect(screen.getByText('pdf')).toBeTruthy();
  fireEvent.change(screen.getByRole('textbox', { name: 'Search items' }), { target: { value: 'documents' } });
  expect(screen.queryByText('safety')).toBeNull();
  fireEvent.change(screen.getByRole('textbox', { name: 'Search items' }), { target: { value: '' } });
  fireEvent.change(screen.getByRole('combobox', { name: 'Item kind' }), { target: { value: 'rule' } });
  expect(screen.queryByText('pdf')).toBeNull();
  expect(screen.getByText('safety')).toBeTruthy();
});

it('marks mixed states and batches row changes once per machine', async () => {
  vi.mocked(api.assignment).mockImplementation(async machine => {
    const next = snapshot(machine === 'a');
    next.assignment.harnesses.codex = { ...emptyHarness(), skills: { pdf: false } };
    return next;
  });
  await mount();
  expect(screen.getByText('Mixed')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledTimes(2));
  for (const machine of ['a', 'b']) expect(api.setAssignment).toHaveBeenCalledWith(machine, { changes: [
    { harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: true },
    { harness: 'codex', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: true },
  ], expected: { revision: 1, registration: `r-${machine}` } });
});

it('persists hidden machines and limits column bulk changes to filtered items', async () => {
  await mount();
  fireEvent.click(screen.getByRole('button', { name: 'Machines (2)' }));
  fireEvent.click(await screen.findByRole('menuitemcheckbox', { name: 'b' }));
  expect(localStorage.getItem('assignment-matrix-machines')).toBe('["a"]');
  expect(screen.queryByRole('checkbox', { name: 'skill:pdf in b / claude, explicit' })).toBeNull();
  fireEvent.keyDown(screen.getByRole('menuitemcheckbox', { name: 'a' }), { key: 'Escape' });
  fireEvent.click(screen.getByRole('button', { name: 'Actions for a / claude' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Disable all visible items' }));
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledTimes(1));
  expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: false }], expected: { revision: 1, registration: 'r-a' } });
});

it('resets row overrides optimistically and skips unavailable remote Draft items', async () => {
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:pdf', id: 'pdf', in_baseline: false, draft: 'added', unpushed: false, description: null }]);
  const save = deferred<AssignmentUpdate>();
  vi.mocked(api.setAssignment).mockReturnValue(save.promise);
  await mount();
  fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Reset all to default' }));
  expect(api.setAssignment).toHaveBeenCalledTimes(1);
  expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: null }], expected: { revision: 1, registration: 'r-a' } });
  expect((await screen.findByRole('checkbox', { name: 'skill:pdf in a / claude, inherited' })).className).toContain('border-dashed');
  const reset = snapshot(false, 2);
  reset.assignment.harnesses.claude.skills = {};
  vi.mocked(api.assignment).mockResolvedValue(reset);
  await act(async () => { save.resolve(reset); });
});

it('describes explicit values that override the Harness default and clears the note on reset', async () => {
  vi.mocked(api.assignment).mockImplementation(async machine => snapshot(machine === 'a'));
  const save = deferred<AssignmentUpdate>();
  vi.mocked(api.setAssignment).mockReturnValue(save.promise);
  await mount();
  expect(cell('a').getAttribute('aria-description')).toBe('Overrides the Harness default.');
  expect(cell('b').hasAttribute('aria-description')).toBe(false);
  expect(cell('a').closest('td')?.className).not.toContain('bg-warning');
  fireEvent.click(screen.getByRole('button', { name: 'Use default for skill:pdf in a / claude' }));
  const inherited = await screen.findByRole('checkbox', { name: 'skill:pdf in a / claude, inherited' });
  expect(inherited.hasAttribute('aria-description')).toBe(false);
  expect(inherited.getAttribute('aria-checked')).toBe('false');
  expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: null }], expected: { revision: 1, registration: 'r-a' } });
  const reset = snapshot(false, 2);
  reset.assignment.harnesses.claude.skills = {};
  vi.mocked(api.assignment).mockResolvedValue(reset);
  await act(async () => { save.resolve(reset); });
});

it('marks offline Machines in the header and on their cells while keeping them editable', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abc', machines: [
    { id: 'a', status: 'in_sync', connected: true, joined_at: 1, last_seen: 1, report: null },
    { id: 'b', status: 'offline', connected: false, joined_at: 1, last_seen: 1, report: null },
  ] });
  await mount();
  expect(within(screen.getByRole('columnheader', { name: 'b, Offline' })).getByText('Offline')).toBeTruthy();
  expect(within(screen.getByRole('columnheader', { name: 'a, This Machine' })).queryByText('Offline')).toBeNull();
  expect(cell('b').getAttribute('aria-description')).toBe('Applies when b reconnects.');
  expect(cell('b').hasAttribute('data-disabled')).toBe(false);
});

it('collapses a Machine to a summary column and remembers it', async () => {
  const view = await mount();
  fireEvent.click(screen.getByRole('button', { name: 'b Harnesses' }));
  expect(screen.getByRole('button', { name: 'b Harnesses' }).getAttribute('aria-expanded')).toBe('false');
  expect(screen.queryByRole('checkbox', { name: 'skill:pdf in b / claude, explicit' })).toBeNull();
  expect(screen.queryByRole('switch', { name: 'Default for new skills in b / claude' })).toBeNull();
  expect(screen.getByText('Enabled in 0 of 1 Harness')).toBeTruthy();
  expect(localStorage.getItem('assignment-matrix-collapsed')).toBe('["b"]');
  view.unmount();
  await mount();
  expect(screen.queryByRole('checkbox', { name: 'skill:pdf in b / claude, explicit' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'b Harnesses' }));
  expect(cell('b')).toBeTruthy();
  expect(localStorage.getItem('assignment-matrix-collapsed')).toBe('[]');
});

it('shows only this Machine or all Machines from the filter shortcuts', async () => {
  await mount();
  fireEvent.click(screen.getByRole('button', { name: 'Machines (2)' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Only this Machine' }));
  await waitFor(() => expect(screen.queryByRole('checkbox', { name: 'skill:pdf in b / claude, explicit' })).toBeNull());
  expect(localStorage.getItem('assignment-matrix-machines')).toBe('["a"]');
  fireEvent.click(screen.getByRole('button', { name: 'Machines (1)' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'All Machines' }));
  await waitFor(() => expect(cell('b')).toBeTruthy());
  expect(localStorage.getItem('assignment-matrix-machines')).toBe('null');
});

it('announces accepted bulk changes but not failed ones', async () => {
  vi.mocked(api.setAssignment).mockImplementation(async machine => {
    if (machine === 'b') throw new Error('b rejected');
    return snapshot(true, 2);
  });
  await mount();
  fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
  await waitFor(() => expect(toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Enabled pdf in 1 Harness', action: expect.objectContaining({ label: 'Undo' }) })));
  await screen.findByText('b: b rejected');
  fireEvent.click(screen.getByRole('button', { name: 'Actions for a / claude' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Disable all visible items' }));
  await waitFor(() => expect(toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Disabled 1 item in a / claude', action: expect.objectContaining({ label: 'Undo' }) })));
});

it('undo restores explicit and inherited values only for accepted Machines', async () => {
  vi.mocked(api.assignment).mockImplementation(async () => {
    const next = snapshot(); next.assignment.harnesses.codex = emptyHarness(); return next;
  });
  const written = () => { const next = snapshot(true, 2); next.assignment.harnesses.codex = { ...emptyHarness(), skills: { pdf: true } }; return next; };
  vi.mocked(api.setAssignment).mockImplementation(async machine => {
    if (machine === 'b') throw new Error('rejected');
    vi.mocked(api.assignment).mockImplementation(async id => id === 'a' ? written() : snapshot());
    return written();
  });
  await mount();
  fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
  await waitFor(() => expect(toast).toHaveBeenCalled());
  const action = vi.mocked(toast).mock.calls[0][0].action!;
  expect(action.label).toBe('Undo');
  await act(async () => { action.run(); });
  expect(api.setAssignment).toHaveBeenCalledTimes(3);
  expect(api.setAssignment).toHaveBeenLastCalledWith('a', { changes: [
    { harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: false },
    { harness: 'codex', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: null },
  ], expected: { revision: 2, registration: 'r-a' } });
  expect(toast).toHaveBeenLastCalledWith(expect.objectContaining({ title: 'Restored 2 Assignments', tone: 'default' }));
});
it('undo is refused by the authority when the Assignment changed meanwhile, then refreshes', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abc', machines: [{ id: 'a', status: 'in_sync', connected: true, joined_at: 1, last_seen: 1, report: null, registration: 'r1' }] });
  vi.mocked(api.setAssignment).mockResolvedValueOnce(snapshot(true, 2))
    .mockRejectedValueOnce({ status: 409, message: 'the assignment of `a` changed since revision 2 (now 3); reload it' });
  vi.mocked(api.assignment).mockResolvedValueOnce(snapshot()).mockResolvedValueOnce(snapshot(true, 2)).mockResolvedValue(snapshot(false, 3));
  await mount();
  fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
  await waitFor(() => expect(toast).toHaveBeenCalled());
  await waitFor(() => expect(enabled('a')).toBe(true));
  const reads = vi.mocked(api.assignment).mock.calls.length;
  await act(async () => { vi.mocked(toast).mock.calls[0][0].action!.run(); });
  await waitFor(() => expect(toast).toHaveBeenLastCalledWith(expect.objectContaining({ title: 'Undo is no longer available' })));
  expect(api.setAssignment).toHaveBeenLastCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: false }], expected: { revision: 2, registration: 'r1' } });
  // No Machine error: the matrix reloads instead.
  expect(screen.queryByText(/changed since revision/)).toBeNull();
  await waitFor(() => expect(vi.mocked(api.assignment).mock.calls.length).toBeGreaterThan(reads));
});
it('drops a write reply for a registration that joined again within the same second', async () => {
  const row = (registration: string) => ({ id: 'a', status: 'in_sync' as const, connected: true, joined_at: 1, last_seen: 1, report: null, registration });
  vi.mocked(api.fleet).mockResolvedValueOnce({ baseline: 'abc', machines: [row('r1')] }).mockResolvedValue({ baseline: 'abc', machines: [row('r2')] });
  const reply = deferred<AssignmentUpdate>();
  vi.mocked(api.setAssignment).mockReturnValue(reply.promise);
  let reads = 0;
  vi.mocked(api.assignment).mockImplementation(() => ++reads === 1 ? Promise.resolve(snapshot()) : reads === 2 ? Promise.resolve(snapshot(false, 5)) : new Promise(() => {}));
  const view = await mount();
  fireEvent.click(cell('a'));
  view.rerender(<Matrix revision={1} status={status} />);
  await waitFor(() => expect(reads).toBe(2));
  await act(async () => { reply.resolve(snapshot(true, 99)); });
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledOnce());
  // The new registration's Assignment stays; the old one's late reply would have enabled pdf.
  expect(enabled('a')).toBe(false);
});
it('refuses an Assignment read for another registration than the Fleet listed', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abc', machines: [{ id: 'a', status: 'in_sync', connected: true, joined_at: 1, last_seen: 1, report: null, registration: 'r2' }] });
  vi.mocked(api.assignment).mockResolvedValue({ ...snapshot(), registration: 'r1' });
  render(<Matrix revision={0} status={status} />);
  expect(await screen.findByText(/a joined the Fleet again meanwhile/)).toBeTruthy();
});

it('assigns Draft-only items locally in standalone without requesting fleet', async () => {
  localStorage.setItem('assignment-matrix-machines', '["other"]');
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:pdf', id: 'pdf', in_baseline: false, draft: 'added', unpushed: false, description: null }]);
  render(<Matrix revision={0} status={{ ...status, mode: 'standalone', report: { seq: 1, items: [], baseline: null, drafts: [], unpushed: [], conflicts: [], blocked: false, clashes: [], behind: false, harnesses: ['claude'] } } as StatusView} />);
  const toggle = await screen.findByRole('checkbox', { name: 'skill:pdf in a / claude, explicit' });
  await act(async () => {});
  fireEvent.click(toggle);
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'pdf', enabled: true }], expected: { revision: 1 } }));
  expect(api.fleet).not.toHaveBeenCalled();
  expect(api.assignment).toHaveBeenCalledWith('a');
  expect(vi.mocked(api.assignment).mock.calls.every(([machine]) => machine === 'a')).toBe(true);
});

it.each([
  { mode: 'joined', server_url: 'https://fleet.example', machine_id: 'a' },
  { mode: 'joined', server_url: 'https://other.example', machine_id: 'a' },
  { mode: 'joined', server_url: 'https://fleet.example', machine_id: 'b' },
] as const)('resets cached revisions and pending writes when authority becomes %j', async identity => {
  const previous = { ...status, mode: identity.mode === 'joined' && identity.server_url === 'https://fleet.example' && identity.machine_id === 'a' ? 'standalone' : 'joined', server_url: 'https://fleet.example', report: { seq: 1, items: [], baseline: null, drafts: [], unpushed: [], conflicts: [], blocked: false, clashes: [], behind: false, harnesses: ['claude'] } } as StatusView;
  const initial = deferred<AssignmentUpdate>();
  vi.mocked(api.assignment).mockReturnValue(initial.promise);
  const save = deferred<AssignmentUpdate>();
  vi.mocked(api.setAssignment).mockReturnValue(save.promise);
  const view = render(<Matrix revision={0} status={previous} />);
  await act(async () => { initial.resolve(snapshot(false, 10)); });
  fireEvent.click(cell('a'));
  expect(api.setAssignment).toHaveBeenCalledTimes(1);
  expect(enabled('a')).toBe(true);
  expect(cell('a').hasAttribute('data-disabled')).toBe(true);
  const replacement = deferred<AssignmentUpdate>();
  vi.mocked(api.assignment).mockReturnValue(replacement.promise);
  view.rerender(<Matrix revision={1} status={{ ...previous, ...identity }} />);
  await act(async () => { replacement.resolve(snapshot(false, 1)); });
  expect(cell('a').hasAttribute('data-disabled')).toBe(false);
  expect(enabled('a')).toBe(false);
  // A response from the previous authority must not restore its optimistic edit.
  await act(async () => { save.resolve(snapshot(true, 11)); });
  expect(enabled('a')).toBe(false);
  expect(cell('a').hasAttribute('data-disabled')).toBe(false);
});

it.each([
  { field: 'mode', next: { mode: 'standalone' } },
  { field: 'server', next: { server_url: 'https://new.example' } },
  { field: 'machine', next: { machine_id: 'b' } },
].flatMap(identity => ['completion', 'undo'].map(phase => ({ ...identity, phase }))))('scopes bulk $phase across a $field change', async ({ phase, next }) => {
  const save = deferred<AssignmentUpdate>();
  vi.mocked(api.setAssignment).mockReturnValue(save.promise);
  const identity = { ...status, mode: 'joined', server_url: 'https://fleet.example', report: { seq: 1, items: [], baseline: null, drafts: [], unpushed: [], conflicts: [], blocked: false, clashes: [], behind: false, harnesses: ['claude'] } } as StatusView;
  const view = render(<Matrix revision={0} status={identity} />);
  await screen.findByRole('checkbox', { name: 'skill:pdf in a / claude, explicit' });
  fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
  if (phase === 'undo') await act(async () => { save.resolve(snapshot(true, 2)); });
  const action = vi.mocked(toast).mock.calls[0]?.[0].action;
  view.rerender(<Matrix revision={0} status={{ ...identity, ...next } as StatusView} />);
  await screen.findByRole('checkbox', { name: 'skill:pdf in a / claude, explicit' });
  if (phase === 'completion') {
    await act(async () => { save.resolve(snapshot(true, 2)); });
    expect(toast).not.toHaveBeenCalled();
  } else {
    expect(action).toBeDefined();
    await act(async () => { action!.run(); });
    expect(api.setAssignment).toHaveBeenCalledTimes(2);
    expect(toast).toHaveBeenLastCalledWith(expect.objectContaining({
      title: 'Undo is no longer available', description: 'The Machine or Fleet changed since this bulk update.',
    }));
  }
});

it('shows a six-second joined load while five-second status polling continues', async () => {
  vi.useFakeTimers();
  try {
    const fleet = vi.mocked(api.fleet).getMockImplementation()!;
    vi.mocked(api.fleet).mockImplementation(() => new Promise(resolve => {
      setTimeout(async () => resolve(await fleet()), 6000);
    }));
    const identity = { ...status, mode: 'joined', server_url: 'https://fleet.example' } as StatusView;
    const view = render(<Matrix revision={0} status={identity} />);
    const poll = setInterval(() => view.rerender(<Matrix revision={0} status={{ ...identity }} />), 5000);
    try {
      await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
      await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
      expect(cell('a')).toBeTruthy();
      await act(async () => { await vi.advanceTimersByTimeAsync(10000); });
      expect(cell('a')).toBeTruthy();
      expect(api.fleet).toHaveBeenCalledTimes(1);
    } finally { clearInterval(poll); }
  } finally { vi.useRealTimers(); }
});

describe('Machine-indexed state', () => {
  const fleet = (rows: [string, number][]) => ({ baseline: 'abc', machines: rows.map(([id, joined_at]) => ({
    id, status: 'in_sync' as const, connected: true, joined_at, last_seen: 1, report: null,
  })) });
  it('treats ids like constructor and toString as plain Machine ids', async () => {
    vi.mocked(api.fleet).mockResolvedValue(fleet([['a', 1], ['constructor', 1], ['toString', 1]]));
    vi.mocked(api.assignment).mockImplementation(async machine => {
      if (machine === 'constructor') throw new Error('Machine not found');
      return snapshot();
    });
    await mount();
    expect(screen.getByText('Assignment not loaded: Machine not found')).toBeTruthy();
    expect(screen.queryByRole('checkbox', { name: /in constructor \// })).toBeNull();
    expect(cell('toString').hasAttribute('data-disabled')).toBe(false);
    vi.mocked(api.setAssignment).mockResolvedValue(snapshot(true, 2));
    fireEvent.click(cell('toString'));
    await waitFor(() => expect(api.setAssignment).toHaveBeenCalledWith('toString', expect.anything()));
    // Bulk changes skip the Machine that never loaded.
    vi.spyOn(window, 'confirm').mockReturnValueOnce(true);
    fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Disable in all visible Harnesses' }));
    await waitFor(() => expect(toast).toHaveBeenCalled());
    expect(vi.mocked(api.setAssignment).mock.calls.some(([machine]) => machine === 'constructor')).toBe(false);
  });
  it('forgets a Machine that left the Fleet, so coming back unloaded shows its error', async () => {
    vi.mocked(api.assignment).mockResolvedValue(snapshot(true, 5));
    const view = await mount();
    expect(enabled('b')).toBe(true);
    vi.mocked(api.fleet).mockResolvedValue(fleet([['a', 1]]));
    view.rerender(<Matrix revision={1} status={status} />);
    await waitFor(() => expect(screen.queryByRole('columnheader', { name: 'b' })).toBeNull());
    vi.mocked(api.fleet).mockResolvedValue(fleet([['a', 1], ['b', 1]]));
    vi.mocked(api.assignment).mockImplementation(async machine => {
      if (machine === 'b') throw new Error('Machine not found');
      return snapshot(true, 5);
    });
    view.rerender(<Matrix revision={2} status={status} />);
    expect(await screen.findByText('Assignment not loaded: Machine not found')).toBeTruthy();
    expect(screen.queryByRole('checkbox', { name: /in b \// })).toBeNull();
  });
  it('drops the snapshot of a Machine that joined again', async () => {
    vi.mocked(api.assignment).mockResolvedValue(snapshot(true, 5));
    const view = await mount();
    vi.mocked(api.fleet).mockResolvedValue(fleet([['a', 1], ['b', 2]]));
    vi.mocked(api.assignment).mockImplementation(async machine => {
      if (machine === 'b') throw new Error('not reachable');
      return snapshot(true, 5);
    });
    view.rerender(<Matrix revision={1} status={status} />);
    expect(await screen.findByText('Assignment not loaded: not reachable')).toBeTruthy();
  });
  it('lets a load sent after the cached snapshot win even with a lower revision', async () => {
    vi.mocked(api.assignment).mockResolvedValue(snapshot(true, 5));
    const view = await mount();
    expect(enabled('a')).toBe(true);
    vi.mocked(api.assignment).mockResolvedValue(snapshot(false, 1));
    view.rerender(<Matrix revision={1} status={status} />);
    await waitFor(() => expect(enabled('a')).toBe(false));
    expect(enabled('b')).toBe(false);
  });
  const bulkEnable = async () => {
    vi.mocked(api.setAssignment).mockImplementation(async () => {
      vi.mocked(api.assignment).mockResolvedValue(snapshot(true, 2));
      return snapshot(true, 2);
    });
    const view = await mount();
    fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
    await waitFor(() => expect(toast).toHaveBeenCalled());
    await waitFor(() => expect(cell('a').hasAttribute('data-disabled')).toBe(false));
    return { view, undo: vi.mocked(toast).mock.calls[0][0].action! };
  };
  it('undoes only Machines whose Assignment is still what the update left', async () => {
    const { view, undo } = await bulkEnable();
    // Someone turned `pdf` off again on a.
    vi.mocked(api.assignment).mockImplementation(async machine => machine === 'a' ? snapshot(false, 3) : snapshot(true, 2));
    view.rerender(<Matrix revision={1} status={status} />);
    await waitFor(() => expect(enabled('a')).toBe(false));
    await act(async () => { undo.run(); });
    await waitFor(() => expect(api.setAssignment).toHaveBeenCalledTimes(3));
    expect(api.setAssignment).toHaveBeenLastCalledWith('b', expect.anything());
    expect(toast).toHaveBeenLastCalledWith(expect.objectContaining({ title: 'Restored 1 Assignment', tone: 'destructive',
      description: '1 Machine changed since this bulk update and kept the current Assignment.' }));
  });
  it('does not undo for a Machine that left the Fleet', async () => {
    const { view, undo } = await bulkEnable();
    vi.mocked(api.fleet).mockResolvedValue({ ...fleet([]), machines: fleet([['b', 1]]).machines.map(m => ({ ...m, registration: 'r-b' })) });
    view.rerender(<Matrix revision={1} status={status} />);
    await waitFor(() => expect(screen.queryByRole('columnheader', { name: /^a/ })).toBeNull());
    await act(async () => { undo.run(); });
    await waitFor(() => expect(api.setAssignment).toHaveBeenCalledTimes(3));
    expect(vi.mocked(api.setAssignment).mock.calls.slice(2).map(([machine]) => machine)).toEqual(['b']);
  });
  it('refuses a bulk change when the Assignment changed since it was shown', async () => {
    vi.mocked(api.setAssignment).mockImplementation(async machine => {
      if (machine === 'b') throw Object.assign(new Error('the assignment of `b` changed since revision 1 (now 2); reload it'), { status: 409 });
      return snapshot(true, 2);
    });
    await mount();
    fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
    await screen.findByText('b: the assignment of `b` changed since revision 1 (now 2); reload it');
    expect(api.setAssignment).toHaveBeenCalledWith('b', expect.objectContaining({ expected: { revision: 1, registration: 'r-b' } }));
    await waitFor(() => expect(toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Enabled pdf in 1 Harness' })));
  });
});

it('refreshes after a single guarded edit conflicts', async () => {
  const refreshed = snapshot(true, 2);
  vi.mocked(api.setAssignment).mockImplementationOnce(async () => {
    vi.mocked(api.assignment).mockResolvedValue(refreshed);
    throw Object.assign(new Error('Assignment changed'), { status: 409 });
  });
  await mount();
  fireEvent.click(cell('a'));
  await waitFor(() => expect(toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Assignment changed' })));
  expect(api.setAssignment).toHaveBeenCalledWith('a', expect.objectContaining({ expected: { revision: 1, registration: 'r-a' } }));
  await waitFor(() => expect(enabled('a')).toBe(true));
  expect(vi.mocked(api.assignment).mock.calls.filter(([id]) => id === 'a')).toHaveLength(2);
});

it('uses the same toggle when the item appears under a new name', async () => {
  vi.mocked(api.items).mockResolvedValue([
    { item: 'skill:pdf', id: 'pdf', in_baseline: true, draft: 'deleted', unpushed: false, description: null },
    { item: 'skill:documents', id: 'pdf', in_baseline: false, draft: 'added', unpushed: false, description: null },
  ]);
  render(<Matrix revision={0} status={status} />);
  const renamed = await screen.findByRole('checkbox', { name: 'skill:documents in b / claude, explicit' });
  expect(renamed.getAttribute('aria-checked')).toBe('false');
  expect(screen.queryByRole('row', { name: /^pdf/ })).toBeNull();
  fireEvent.click(renamed);
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledWith('b', {
    changes: [{ harness: 'claude', kind: 'skill', item_id: 'pdf', name: 'documents', enabled: true }],
    expected: { revision: 1, registration: 'r-b' },
  }));
});

it('clears orphan toggles and refreshes the count', async () => {
  vi.mocked(api.orphans).mockResolvedValue({ orphans: [{ machine: 'a', harness: 'claude', kind: 'skill', item_id: 'gone', name: 'old' }] });
  vi.mocked(api.pruneOrphans).mockImplementation(async () => {
    vi.mocked(api.orphans).mockResolvedValue({ orphans: [] });
    return { entries: [] };
  });
  await mount();
  expect(await screen.findByText('1 toggle for items that no longer exist')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Clear' }));
  const confirm = await screen.findByRole('dialog', { name: 'Clear orphan toggles?' });
  fireEvent.click(within(confirm).getByRole('button', { name: 'Clear' }));
  await waitFor(() => expect(screen.queryByText('1 toggle for items that no longer exist')).toBeNull());
  expect(api.pruneOrphans).toHaveBeenCalledOnce();
});
