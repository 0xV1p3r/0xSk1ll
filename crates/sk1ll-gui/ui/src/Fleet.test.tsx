// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { TooltipProvider } from '@/components/ui/tooltip';
import { SidebarProvider } from '@/components/ui/sidebar';
import { Fleet } from './Fleet';
import { App, AppSidebar } from './App';
import { api } from './api';
import type { FleetView, MachineReport, MachineRow, StatusView } from './types';
vi.mock('./api', () => ({ api: { items: vi.fn().mockResolvedValue([]), assignment: vi.fn().mockResolvedValue({ assignment: { harnesses: {} }, revision: 0 }), setAssignment: vi.fn(), push: vi.fn(), fleet: vi.fn(), status: vi.fn(), skills: vi.fn(), connectionInfo: vi.fn(), joinCode: vi.fn(), pull: vi.fn(), removeMachine: vi.fn(), conflicts: vi.fn().mockResolvedValue([]), recovered: vi.fn().mockResolvedValue([]) }, errorMessage: String, errorStatus: (error: { status?: number }) => error?.status }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn().mockResolvedValue(() => {}) }));
beforeEach(() => { window.location.hash = ''; vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() }))); });
afterEach(() => { cleanup(); localStorage.clear(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
it('filters Machines by attention and connection, then clears the filter', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [
    { id: 'online', connected: true, status: 'in_sync', report: null, last_seen: null, joined_at: 1 },
    { id: 'offline', connected: false, status: 'offline', report: null, last_seen: null, joined_at: 1 },
    { id: 'behind', connected: true, status: 'behind', report: null, last_seen: null, joined_at: 1 },
    { id: 'blocked', connected: true, status: 'blocked', report: null, last_seen: null, joined_at: 1 },
    { id: 'conflict', connected: true, status: 'conflict', report: null, last_seen: null, joined_at: 1 },
    { id: 'drafting', connected: true, status: 'drafting', report: null, last_seen: null, joined_at: 1 },
  ] });
  render(<TooltipProvider><Fleet revision={0} machineId="online" navigate={vi.fn()} changed={vi.fn()} live /></TooltipProvider>);
  await screen.findByText('online');
  const filters = within(screen.getByRole('group', { name: 'Machine filter' }));
  expect(filters.getByRole('button', { name: 'Filter: All' }).textContent).toBe('All6');
  expect(filters.getByRole('button', { name: 'Filter: All' }).getAttribute('aria-pressed')).toBe('true');
  expect(filters.getByRole('button', { name: 'Filter: Attention' }).textContent).toBe('Attention3');
  expect(filters.getByRole('button', { name: 'Filter: Connected' }).textContent).toBe('Connected5');
  fireEvent.click(filters.getByRole('button', { name: 'Filter: Attention' }));
  expect(screen.queryByText('online')).toBeNull(); expect(screen.getByText('offline')).toBeTruthy();
  expect(screen.queryByText('behind')).toBeNull(); expect(screen.queryByText('drafting')).toBeNull();
  expect(screen.getByText('blocked')).toBeTruthy(); expect(screen.getByText('conflict')).toBeTruthy();
  expect(filters.getByRole('button', { name: 'Filter: Attention' }).getAttribute('aria-pressed')).toBe('true');
  expect(filters.getByRole('button', { name: 'Filter: All' }).getAttribute('aria-pressed')).toBe('false');
  fireEvent.click(filters.getByRole('button', { name: 'Filter: Connected' }));
  expect(screen.queryByText('offline')).toBeNull(); expect(screen.getByText('online')).toBeTruthy();
  expect(screen.getByText('behind')).toBeTruthy(); expect(screen.getByText('drafting')).toBeTruthy();
  fireEvent.click(filters.getByRole('button', { name: 'Filter: All' })); expect(screen.getByText('offline')).toBeTruthy();
  fireEvent.click(filters.getByRole('button', { name: 'Filter: All' }));
  expect(filters.getByRole('button', { name: 'Filter: All' }).getAttribute('aria-pressed')).toBe('true');
  expect(within(screen.getByRole('table', { name: 'Machines' })).getAllByRole('row')).toHaveLength(7);
});
const machineReport = (overrides: Partial<MachineReport> = {}): MachineReport => ({ baseline: 'abcdef123456', drafts: [], unpushed: [], conflicts: [], harnesses: ['codex'], blocked: false, clashes: [], behind: false, ...overrides });
const machineRow = (overrides: Partial<MachineRow> = {}): MachineRow => ({ id: 'local', status: 'in_sync', connected: true, joined_at: 1, last_seen: 2, report: machineReport(), ...overrides });
function renderFleet(props: Partial<Parameters<typeof Fleet>[0]> = {}) {
  return render(<TooltipProvider><Fleet revision={0} machineId="local" navigate={vi.fn()} changed={vi.fn()} live {...props} /></TooltipProvider>);
}
it('describes every real status and the total in the fleet health bar', async () => {
  const statuses = ['in_sync', 'in_sync', 'drafting', 'behind', 'conflict', 'conflict', 'blocked', 'offline'] as const;
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abcdef123456', machines: statuses.map((status, i) => machineRow({ id: `machine-${i}`, status, connected: status !== 'offline' })) });
  renderFleet();
  const health = await screen.findByRole('region', { name: 'Fleet health' });
  expect(within(health).getByRole('img', { name: 'Fleet health: 2 In sync, 1 Drafting, 1 Behind, 2 Conflict, 1 Blocked, 1 Offline. 8 Machines total.' })).toBeTruthy();
  expect(within(health).getByText('abcdef12')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Filter: Attention' }));
  expect(within(health).getByRole('img').getAttribute('aria-label')).toContain('8 Machines total');
});
it('queues only this Machine’s work and navigates without changing state', async () => {
  vi.mocked(api.pull).mockClear(); vi.mocked(api.removeMachine).mockClear();
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abcdef123456', machines: [
    machineRow({ status: 'conflict', report: machineReport({ drafts: ['skill:draft', 'rule:style'], unpushed: ['skill:release'], conflicts: [{ item: 'skill:review', harness: 'codex' }] }) }),
    machineRow({ id: 'remote', status: 'conflict', report: machineReport({ drafts: ['skill:remote'], conflicts: [{ item: 'skill:remote-conflict', harness: 'claude' }] }) }),
  ] });
  const navigate = vi.fn(); renderFleet({ navigate });
  const next = within(await screen.findByRole('region', { name: 'Up next' }));
  expect(next.getAllByRole('listitem')).toHaveLength(3);
  expect(next.getByText('review in codex')).toBeTruthy(); expect(next.getByText('2 Draft items')).toBeTruthy();
  expect(next.queryByText(/remote/)).toBeNull();
  fireEvent.click(next.getByRole('button', { name: 'Resolve conflict' })); expect(navigate).toHaveBeenLastCalledWith('Conflicts');
  fireEvent.click(next.getByRole('button', { name: 'Review Draft' })); expect(navigate).toHaveBeenLastCalledWith('Draft');
  expect(api.pull).not.toHaveBeenCalled(); expect(api.removeMachine).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Filter: Connected' })); expect(next.getAllByRole('listitem')).toHaveLength(3);
});
it.each(['drafts', 'unpushed'] as const)('offers Review Draft when only %s are reported', async kind => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [machineRow({ report: machineReport({ [kind]: ['skill:one'] }) })] });
  const navigate = vi.fn(); renderFleet({ navigate });
  const next = within(await screen.findByRole('region', { name: 'Up next' }));
  expect(next.getAllByRole('listitem')).toHaveLength(1);
  expect(next.getByText(kind === 'drafts' ? '1 Draft item' : 'Push 1 unpushed item')).toBeTruthy();
  if (kind === 'drafts') { fireEvent.click(next.getByRole('button', { name: 'Review Draft' })); expect(navigate).toHaveBeenCalledWith('Draft'); } else expect(next.getByRole('button', { name: 'Push' })).toBeTruthy();
});
it.each(['empty report', 'missing report', 'missing Machine'])('shows All caught up for %s and ignores remote work', async kind => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [
    ...(kind === 'missing Machine' ? [] : [machineRow({ report: kind === 'missing report' ? null : machineReport() })]),
    machineRow({ id: 'remote', status: 'conflict', report: machineReport({ drafts: ['skill:remote'], conflicts: [{ item: 'skill:remote', harness: 'codex' }] }) }),
  ] });
  renderFleet(); const next = within(await screen.findByRole('region', { name: 'Up next' }));
  expect(next.getByText('All caught up')).toBeTruthy(); expect(next.queryByRole('button')).toBeNull();
});
it('preserves Machine details, local actions, timestamps, and removal confirmation', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'abcdef123456', machines: [machineRow({ status: 'behind', report: machineReport({ baseline: 'old-sha', drafts: ['skill:draft'], unpushed: ['skill:release'], clashes: ['skill:clash'], behind: true, conflicts: [{ item: 'skill:review', harness: 'codex' }] }) })] });
  const navigate = vi.fn(), changed = vi.fn(); vi.mocked(api.pull).mockResolvedValue({ state: 'up_to_date' });
  renderFleet({ navigate, changed });
  const button = await screen.findByRole('button', { name: 'Show details for local' });
  expect(screen.getByText('This Machine')).toBeTruthy(); expect(screen.getByText('old-sha')).toBeTruthy();
  fireEvent.click(button); expect(button.getAttribute('aria-expanded')).toBe('true');
  const details = within(document.getElementById(button.getAttribute('aria-controls')!)!);
  expect(details.getByText('skill:draft')).toBeTruthy(); expect(details.getByText('skill:release')).toBeTruthy(); expect(details.getByText('skill:clash')).toBeTruthy();
  expect(details.getByText('skill:review in codex')).toBeTruthy(); expect(details.getByText(`(${new Date(2000).toLocaleString()})`)).toBeTruthy();
  fireEvent.click(details.getByRole('button', { name: 'Review Conflicts' })); expect(navigate).toHaveBeenLastCalledWith('Conflicts');
  fireEvent.click(details.getByRole('button', { name: 'Review Draft' })); expect(navigate).toHaveBeenLastCalledWith('Draft');
  fireEvent.click(details.getByRole('button', { name: 'Pull Baseline' })); await waitFor(() => expect(changed).toHaveBeenCalledOnce());
  fireEvent.click(details.getByRole('button', { name: 'Remove local from the Fleet' }));
  expect(await screen.findByRole('dialog', { name: 'Remove local from the Fleet?' })).toBeTruthy();
  expect(screen.getByText(/This removes THIS Machine/)).toBeTruthy();
});
it('creates an Add Machine join code through the existing flow', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] }); vi.mocked(api.joinCode).mockResolvedValue({ code: 'fleet-join-code', expires_at: null, scope: null });
  renderFleet(); fireEvent.click(await screen.findByRole('button', { name: 'Add Machine' }));
  expect(await screen.findByRole('dialog', { name: 'Join code' })).toBeTruthy(); expect(screen.getByText('fleet-join-code')).toBeTruthy();
});
it('shows each Machine\'s scope and lets a Self-only Machine remove only itself', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [machineRow({ scope: 'self' }), machineRow({ id: 'other' })] });
  renderFleet();
  expect(await screen.findByText('Self-only')).toBeTruthy();
  expect(screen.getByText(/joined with a Self-only join code/)).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Add Machine' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Show details for local' }));
  fireEvent.click(screen.getByRole('button', { name: 'Show details for other' }));
  expect(screen.getByText('Only itself')).toBeTruthy(); expect(screen.getByText('Whole Fleet')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Remove local from the Fleet' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Remove other from the Fleet' })).toBeNull();
});
it('handles an empty Fleet and a filter with no matches', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] }); renderFleet();
  expect(await screen.findByText('No Machines')).toBeTruthy(); expect(screen.getByRole('img', { name: 'Fleet health: No Machines.' })).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Filter: Attention' })); expect(screen.getByText('No matching Machines')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Clear Machine filter' })); expect(screen.getByText('No Machines')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Filter: All' }).getAttribute('aria-pressed')).toBe('true');
});
it('shows loading and initial errors without fabricating Fleet data', async () => {
  let reject!: (reason: Error) => void;
  vi.mocked(api.fleet).mockReturnValue(new Promise((_resolve, fail) => { reject = fail; })); renderFleet();
  expect(screen.getByRole('status', { name: 'Loading' })).toBeTruthy(); expect(screen.queryByRole('region', { name: 'Fleet health' })).toBeNull();
  await act(async () => reject(new Error('Fleet unavailable')));
  expect(await screen.findByRole('alert')).toHaveProperty('textContent', 'Could not complete the requestError: Fleet unavailable');
  expect(screen.queryByRole('status', { name: 'Loading' })).toBeNull();
});
it('refreshes health and actions on revision and retains data when refresh fails', async () => {
  let resolve!: (value: FleetView) => void;
  vi.mocked(api.fleet).mockReturnValue(new Promise(done => { resolve = done; }));
  const props = { machineId: 'local', navigate: vi.fn(), changed: vi.fn(), live: true };
  const view = render(<TooltipProvider><Fleet {...props} revision={0} /></TooltipProvider>);
  await act(async () => resolve({ baseline: 'first-sha', machines: [machineRow({ status: 'drafting', report: machineReport({ drafts: ['skill:one'] }) })] }));
  expect(screen.getByRole('button', { name: 'Review Draft' })).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Show details for local' }));
  vi.mocked(api.fleet).mockResolvedValue({ baseline: 'next-sha', machines: [machineRow()] });
  view.rerender(<TooltipProvider><Fleet {...props} revision={1} /></TooltipProvider>);
  expect(await screen.findByText('next-sha')).toBeTruthy(); expect(screen.getByText('All caught up')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Hide details for local' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Review Draft' })).toBeNull();
  vi.mocked(api.fleet).mockRejectedValue(new Error('Refresh unavailable'));
  view.rerender(<TooltipProvider><Fleet {...props} revision={2} live={false} /></TooltipProvider>);
  expect(await screen.findByText('Error: Refresh unavailable')).toBeTruthy();
  expect(screen.getByText('Showing last available Fleet data')).toBeTruthy(); expect(screen.getByText('next-sha')).toBeTruthy();
});
it('shows action counts with their purpose', () => {
  const status = { machine_id: 'test', report: { drafts: ['a', 'b'], conflicts: [{}] } } as StatusView;
  render(<TooltipProvider><SidebarProvider><AppSidebar screen="Library" setScreen={vi.fn()} status={status} unmanagedCount={1} /></SidebarProvider></TooltipProvider>);
  expect(screen.getByLabelText('1 Unmanaged skill to adopt').textContent).toBe('1');
  expect(screen.getByLabelText('1 Conflict to resolve').textContent).toBe('1');
  expect(screen.getByLabelText('2 Draft items').textContent).toBe('2');
});

it('opens with Ctrl+K and navigates to Library from App', async () => {
  vi.mocked(api.status).mockResolvedValue({ machine_id: 'test', headline: 'in_sync', connection: 'connected', report: machineReport() } as unknown as StatusView);
  vi.mocked(api.skills).mockResolvedValue([]); vi.mocked(api.connectionInfo).mockResolvedValue('test');
  render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  fireEvent.keyDown(window, { key: 'k', ctrlKey: true });
  const input = await screen.findByRole('combobox');
  fireEvent.change(input, { target: { value: 'Library' } }); fireEvent.keyDown(input, { key: 'Enter' });
  expect(await screen.findByRole('heading', { name: 'Library', level: 1 })).toBeTruthy();
  expect(screen.queryByRole('dialog')).toBeNull();
});

function mockShell(conflicts: StatusView['report']['conflicts'] = []) {
  vi.mocked(api.status).mockResolvedValue({ machine_id: 'test', server_url: 'https://skills.example.com', headline: conflicts.length ? 'conflict' : 'in_sync', connection: 'connected', report: machineReport({ conflicts }) } as unknown as StatusView);
  vi.mocked(api.skills).mockResolvedValue([]); vi.mocked(api.connectionInfo).mockResolvedValue('test');
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
}
const sidebarState = () => document.querySelector('[data-slot="sidebar"][data-state]')?.getAttribute('data-state');
it('persists a collapsed sidebar through wide resizes and remounts', async () => {
  vi.stubGlobal('innerWidth', 1400); mockShell();
  const view = render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  fireEvent.click(within(screen.getByRole('banner')).getByRole('button', { name: 'Toggle Sidebar' }));
  expect(localStorage.getItem('sk1ll-sidebar-open')).toBe('false');
  vi.stubGlobal('innerWidth', 1500); fireEvent(window, new Event('resize'));
  expect(sidebarState()).toBe('collapsed');
  view.unmount(); render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  expect(sidebarState()).toBe('collapsed');
});
it('auto-collapses on narrow windows without losing the expanded preference', async () => {
  localStorage.setItem('sk1ll-sidebar-open', 'true'); vi.stubGlobal('innerWidth', 1400); mockShell();
  render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  expect(sidebarState()).toBe('expanded');
  vi.stubGlobal('innerWidth', 900); fireEvent(window, new Event('resize'));
  expect(sidebarState()).toBe('collapsed'); expect(localStorage.getItem('sk1ll-sidebar-open')).toBe('true');
  vi.stubGlobal('innerWidth', 1400); fireEvent(window, new Event('resize'));
  expect(sidebarState()).toBe('expanded');
});
it('keeps a manual expansion on a narrow window until the breakpoint is crossed', async () => {
  localStorage.setItem('sk1ll-sidebar-open', 'true'); vi.stubGlobal('innerWidth', 800); mockShell();
  render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  expect(sidebarState()).toBe('expanded');
  vi.stubGlobal('innerWidth', 801); fireEvent(window, new Event('resize'));
  expect(sidebarState()).toBe('expanded');
});
it('lists this Machine\'s work in Up next from the Daemon status when the Fleet omits it', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
  render(<Fleet revision={0} machineId="local" localReport={machineReport({ drafts: ['skill:draft'], conflicts: [{ item: 'skill:review', harness: 'codex' }] })} navigate={vi.fn()} changed={vi.fn()} live />);
  expect(await screen.findByText('Resolve conflict')).toBeTruthy(); expect(screen.queryByText('All caught up')).toBeNull();
});
it('keeps the sidebar usable when localStorage is unavailable', async () => {
  vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('Storage unavailable'); });
  vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('Storage unavailable'); });
  vi.stubGlobal('innerWidth', 1400); mockShell();
  render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  fireEvent.click(within(screen.getByRole('banner')).getByRole('button', { name: 'Toggle Sidebar' }));
  fireEvent(window, new Event('resize')); expect(sidebarState()).toBe('collapsed');
});
it('names the first conflict and opens Conflicts with heading focus', async () => {
  mockShell([{ item: 'skill:code-review', harness: 'codex' }, { item: 'rule:style', harness: 'claude' }]);
  render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  expect(screen.queryByRole('button', { name: 'Resolve' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Library' }));
  expect(await screen.findByText('This Machine has 2 Conflicts')).toBeTruthy();
  expect(screen.getByText('code-review').parentElement?.textContent).toContain('was edited in codex');
  fireEvent.click(screen.getByRole('button', { name: 'Resolve' }));
  const heading = await screen.findByRole('heading', { name: 'Conflicts', level: 1 });
  await waitFor(() => expect(document.activeElement).toBe(heading));
  expect(screen.queryByRole('button', { name: 'Resolve' })).toBeNull();
});

it.each([true, false])('push handles pushed=%s', async pushed => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
  vi.mocked(api.push).mockResolvedValue({ pushed });
  const changed = vi.fn(); renderFleet({ changed, localReport: machineReport({ unpushed: ['skill:release'] }) });
  fireEvent.click(await screen.findByRole('button', { name: 'Push' }));
  if (pushed) await waitFor(() => expect(changed).toHaveBeenCalledOnce());
  else { expect(await screen.findByText('Error: Push did not reach the Server.')).toBeTruthy(); expect(changed).not.toHaveBeenCalled(); }
});
it('counts queue entries, adopts in Library, and hides Push without unpushed items', async () => {
  const navigate = vi.fn(); renderFleet({ navigate, unmanaged: [{ name: 'notes' }], localReport: machineReport({ drafts: ['skill:a'] }) });
  const next = within(await screen.findByRole('region', { name: 'Up next' }));
  expect(next.getAllByRole('listitem')).toHaveLength(2); expect(next.getByText('2')).toBeTruthy(); expect(next.queryByRole('button', { name: 'Push' })).toBeNull();
  expect(next.getByText('1 unmanaged skill · notes')).toBeTruthy(); fireEvent.click(next.getByRole('button', { name: 'Adopt unmanaged skills' })); expect(navigate).toHaveBeenCalledWith('Library');
});
it('renders local assignments, toggles a cell and opens the matrix', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [machineRow({ registration: 'local-reg' })] });
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:review', in_baseline: true, draft: null, unpushed: false, description: null }]);
  const snapshot = { assignment: { harnesses: { codex: { new_skills_default: true, new_rules_default: true, skills: {}, rules: {} } } }, revision: 1, registration: 'local-reg' };
  const updated = { assignment: { harnesses: { codex: { new_skills_default: true, new_rules_default: true, skills: { review: false }, rules: {} } } }, revision: 2 };
  vi.mocked(api.assignment).mockClear().mockResolvedValueOnce(snapshot).mockResolvedValue(updated); vi.mocked(api.setAssignment).mockResolvedValue({ ...updated, entries: [] });
  const navigate = vi.fn(); renderFleet({ navigate });
  const cell = await screen.findByRole('checkbox', { name: 'skill:review in local / codex, Inherited' });
  expect(cell.getAttribute('aria-checked')).toBe('true'); fireEvent.click(cell);
  expect(screen.getByRole('checkbox', { name: 'skill:review in local / codex, Explicit' }).getAttribute('aria-checked')).toBe('false');
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledWith('local', { changes: [{ harness: 'codex', kind: 'skill', name: 'review', enabled: false }], expected: { revision: 1, registration: 'local-reg' } }));
  await waitFor(() => expect(api.assignment).toHaveBeenCalledTimes(2));
  expect(screen.getByRole('checkbox', { name: 'skill:review in local / codex, Explicit' }).getAttribute('aria-checked')).toBe('false');
  fireEvent.click(screen.getByRole('button', { name: 'Open matrix' })); expect(navigate).toHaveBeenCalledWith('Assignment matrix');
});

const harness = (skills: Record<string, boolean> = {}) => ({ assignment: { harnesses: { codex: { new_skills_default: true, new_rules_default: true, skills, rules: {} } } }, revision: 1 });
const item = (name: string) => ({ item: `skill:${name}`, in_baseline: true, draft: null, unpushed: false, description: null });
it('rolls a failed local assignment toggle back and shows the error', async () => {
  vi.mocked(api.items).mockResolvedValue([item('review')]); vi.mocked(api.assignment).mockResolvedValue(harness());
  vi.mocked(api.setAssignment).mockRejectedValue(new Error('Server refused'));
  renderFleet(); fireEvent.click(await screen.findByRole('checkbox', { name: 'skill:review in local / codex, Inherited' }));
  expect(await screen.findByText('Error: Server refused')).toBeTruthy();
  expect(screen.getByRole('checkbox', { name: 'skill:review in local / codex, Inherited' }).getAttribute('aria-checked')).toBe('true');
});
it('keeps a toggled row in place even when it drops below the six-row ranking', async () => {
  vi.mocked(api.items).mockResolvedValue(['a', 'b', 'c', 'd', 'e', 'f', 'z-task'].map(item));
  const diverging = harness({ a: true, b: true, c: true, d: true, e: true, f: true, 'z-task': false });
  const matching = { ...harness({ a: true, b: true, c: true, d: true, e: true, f: true, 'z-task': true }), revision: 2 };
  vi.mocked(api.assignment).mockClear().mockResolvedValueOnce(diverging).mockResolvedValue(matching); vi.mocked(api.setAssignment).mockResolvedValue({ ...matching, entries: [] });
  renderFleet();
  const panel = within(await screen.findByRole('table', { name: 'This Machine assignments' }));
  expect(panel.getAllByRole('row').slice(1).map(row => row.textContent)).toEqual(['z-taskSkill', 'aSkill', 'bSkill', 'cSkill', 'dSkill', 'eSkill']);
  const cell = panel.getByRole('checkbox', { name: 'skill:z-task in local / codex, Explicit' }); cell.focus(); fireEvent.click(cell);
  await waitFor(() => expect(api.assignment).toHaveBeenCalledTimes(2));
  expect(panel.getByRole('checkbox', { name: 'skill:z-task in local / codex, Explicit' }).getAttribute('aria-checked')).toBe('true');
  expect(document.activeElement).toBe(panel.getByRole('checkbox', { name: 'skill:z-task in local / codex, Explicit' }));
});
it('reloads the assignments panel when the machine identity changes', async () => {
  vi.mocked(api.items).mockResolvedValue([item('review')]); vi.mocked(api.assignment).mockResolvedValue(harness());
  const view = renderFleet(); await screen.findByRole('checkbox', { name: 'skill:review in local / codex, Inherited' });
  view.rerender(<TooltipProvider><Fleet revision={0} machineId="other" navigate={vi.fn()} changed={vi.fn()} live /></TooltipProvider>);
  expect(await screen.findByRole('checkbox', { name: 'skill:review in other / codex, Inherited' })).toBeTruthy();
});
it('does not claim the queue is empty while unmanaged-skill discovery fails or loads', async () => {
  const view = renderFleet({ discovery: { loading: false, error: 'boom' } });
  expect(await screen.findByText('Skill discovery failed')).toBeTruthy(); expect(screen.queryByText('All caught up')).toBeNull();
  view.rerender(<TooltipProvider><Fleet revision={0} machineId="local" navigate={vi.fn()} changed={vi.fn()} live discovery={{ loading: true, error: '' }} /></TooltipProvider>);
  expect(screen.getByText('Checking for unmanaged skills…')).toBeTruthy();
});

it('keeps essential Machine details and hides empty work fields', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [machineRow()] }); renderFleet();
  fireEvent.click(await screen.findByRole('button', { name: 'Show details for local' }));
  const details = within(document.getElementById('machine-local')!);
  for (const label of ['Connection', 'Joined', 'Last seen', 'Harnesses']) expect(details.getByText(label)).toBeTruthy();
  for (const label of ['Draft', 'Unpushed', 'Conflicts', 'Clashes', 'Blocked', 'Behind']) expect(details.queryByText(label)).toBeNull();
});
it('names unpushed items and shows the assignment preview total', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
  vi.mocked(api.items).mockResolvedValue(['a', 'b', 'c', 'd', 'e', 'f', 'g'].map(item)); vi.mocked(api.assignment).mockResolvedValue(harness());
  renderFleet({ localReport: machineReport({ unpushed: ['skill:release', 'rule:style', 'skill:review'] }) });
  expect(await screen.findByText('release, plus 2 more')).toBeTruthy();
  const assignments = within(await screen.findByRole('region', { name: "This Machine's Assignments" }));
  expect(await assignments.findByText('6 of 7')).toBeTruthy(); expect(assignments.getByText('Inherited')).toBeTruthy();
  expect(assignments.getByRole('button', { name: 'Open matrix' })).toBeTruthy();
});

it('shows every Machine again when the selected Machine was removed', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [{ id: 'desktop', status: 'offline', connected: false, joined_at: 0, last_seen: null, report: null }] });
  renderFleet({ initialMachine: 'laptop' });
  expect(await screen.findByRole('button', { name: 'Show details for desktop' })).toBeTruthy();
  expect(screen.queryByText('No Machines')).toBeNull();
});
