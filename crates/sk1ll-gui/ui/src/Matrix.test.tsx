// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Matrix } from './Matrix';
import { api } from './api';
import { emptyHarness } from './model';
import { toast } from './toast';
import type { Assignment, AssignmentUpdate, StatusView } from './types';

vi.mock('./api', () => ({ api: {
  fleet: vi.fn(), items: vi.fn(), assignment: vi.fn(), setAssignment: vi.fn(),
}, errorMessage: (error: unknown) => error instanceof Error ? error.message : String(error) }));
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
    id, status: 'in_sync', connected: true, joined_at: 1, last_seen: 1, report: null,
  })) });
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:pdf', in_baseline: true, draft: null, unpushed: false, description: null }]);
  vi.mocked(api.assignment).mockResolvedValue(snapshot());
  vi.mocked(api.setAssignment).mockResolvedValue(snapshot());
});
afterEach(cleanup);
const mount = async () => {
  const view = render(<Matrix revision={0} status={status} />);
  await screen.findByRole('checkbox', { name: 'skill:pdf in a / claude, explicit' });
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
      return Promise.resolve(snapshot(machine === 'b', machine === 'b' ? 3 : 1));
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
    await screen.findByText('refresh unavailable');
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
    expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'new_skills_default', enabled: true }] });
    const next = snapshot(false, 2); next.assignment.harnesses.claude.new_skills_default = true;
    await act(async () => { save.resolve(next); });
    expect(screen.getByRole('switch', { name: 'Default for new skills in a / claude' }).getAttribute('aria-checked')).toBe('true');
    expect(enabled('a')).toBe(false);
  });
  it('labels unpushed Commits and Draft-only additions consistently', async () => {
    vi.mocked(api.items).mockResolvedValue([
      { item: 'skill:queued', in_baseline: false, draft: null, unpushed: true, description: null },
      { item: 'skill:local', in_baseline: false, draft: 'added', unpushed: false, description: null },
    ]);
    render(<Matrix revision={0} status={status} />);
    const queued = (await screen.findByText('queued')).closest('tr')!;
    const local = screen.getByText('local').closest('tr')!;
    expect(within(queued).getByText('Unpushed')).toBeTruthy();
    expect(within(local).getByText('Draft only')).toBeTruthy();
    expect(screen.queryByText(/not committed/i)).toBeNull();
  });
});

it('toggles an Assignment and a Harness default from their state labels', async () => {
  await mount();
  const label = cell('a').closest('label')!;
  expect(label.className).toContain('min-h-8');
  fireEvent.click(label);
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', name: 'pdf', enabled: true }] }));
  await waitFor(() => expect(cell('a').hasAttribute('data-disabled')).toBe(false));
  const defaultLabel = screen.getByRole('switch', { name: 'Default for new skills in a / claude' }).closest('label')!;
  fireEvent.click(defaultLabel);
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'new_skills_default', enabled: true }] }));
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

it('filters by description and kind and shows friendly names', async () => {
  vi.mocked(api.items).mockResolvedValue([
    { item: 'skill:pdf', in_baseline: true, draft: null, unpushed: false, description: 'Read documents' },
    { item: 'rule:safety', in_baseline: true, draft: null, unpushed: false, description: 'Review changes' },
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
    { harness: 'claude', kind: 'skill', name: 'pdf', enabled: true },
    { harness: 'codex', kind: 'skill', name: 'pdf', enabled: true },
  ] });
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
  expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', name: 'pdf', enabled: false }] });
});

it('resets row overrides optimistically and skips unavailable remote Draft items', async () => {
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:pdf', in_baseline: false, draft: 'added', unpushed: false, description: null }]);
  const save = deferred<AssignmentUpdate>();
  vi.mocked(api.setAssignment).mockReturnValue(save.promise);
  await mount();
  fireEvent.click(screen.getByRole('button', { name: 'Actions for skill:pdf' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Reset all to default' }));
  expect(api.setAssignment).toHaveBeenCalledTimes(1);
  expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', name: 'pdf', enabled: null }] });
  expect(screen.getByRole('checkbox', { name: 'skill:pdf in a / claude, inherited' }).className).toContain('border-dashed');
  await act(async () => { save.resolve(snapshot()); });
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
  const inherited = screen.getByRole('checkbox', { name: 'skill:pdf in a / claude, inherited' });
  expect(inherited.hasAttribute('aria-description')).toBe(false);
  expect(inherited.getAttribute('aria-checked')).toBe('false');
  expect(api.setAssignment).toHaveBeenCalledWith('a', { changes: [{ harness: 'claude', kind: 'skill', name: 'pdf', enabled: null }] });
  await act(async () => { save.resolve(snapshot()); });
});

it('describes defaults rows without a visible hint row', async () => {
  await mount();
  const toggle = screen.getByRole('switch', { name: 'Default for new skills in a / claude' });
  expect(document.getElementById(toggle.getAttribute('aria-describedby')!)?.textContent).toMatch(/applies to items added later/);
  expect(document.getElementById('default-hint')?.className).toContain('sr-only');
  expect(screen.queryByText(/defaults for new items/i)).toBeNull();
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
  vi.mocked(api.setAssignment).mockImplementation(async machine => {
    if (machine === 'b') throw new Error('rejected');
    return snapshot(true, 2);
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
    { harness: 'claude', kind: 'skill', name: 'pdf', enabled: false },
    { harness: 'codex', kind: 'skill', name: 'pdf', enabled: null },
  ] });
  expect(toast).toHaveBeenLastCalledWith(expect.objectContaining({ title: 'Restored 2 Assignments', tone: 'default' }));
});
