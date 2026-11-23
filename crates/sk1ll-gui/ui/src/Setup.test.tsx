// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { App } from './App';
import { api } from './api';
import { GettingStarted, StartingAssignment, showGettingStarted } from './Setup';
import type { ContentEntry, HarnessChoice, StatusView } from './types';
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock('./api', () => ({ api: { status: vi.fn(), connectionInfo: vi.fn(), skills: vi.fn(), recovered: vi.fn(), imports: vi.fn(), fleet: vi.fn(), log: vi.fn(), logFacets: vi.fn(), init: vi.fn(),
  setupHarnesses: vi.fn(), setSetupHarness: vi.fn(), daemonService: vi.fn(), restartDaemon: vi.fn(), push: vi.fn(), startDaemon: vi.fn(), join: vi.fn(), harnesses: vi.fn(), setHarnessAssignment: vi.fn(), joinCode: vi.fn() }, errorMessage: String }));
vi.mock('./Skills', () => ({ Skills: ({ handoff }: { handoff: { create?: string } }) => <div data-create={handoff.create}>Local library</div> }));
vi.mock('./Matrix', () => ({ Matrix: () => <div>Matrix</div> }));

const firstRun: StatusView = { mode: 'standalone', has_origin: false, repo_ready: false, machine_id: 'box', server_url: null, repo: '/home/me/skills', headline: 'in_sync', connection: 'standalone', last_error: null, revoked: false, server_baseline: null, assignment_revision: 0, awaiting_assignment: false, auth_rejected_since: null, report: { seq: 1, items: [], baseline: null, drafts: [], unpushed: [], conflicts: [], harnesses: [], blocked: false, clashes: [], behind: false } };
const joined: StatusView = { ...firstRun, mode: 'joined', has_origin: true, repo_ready: true, server_url: 'https://skills.example.com', connection: 'connected', assignment_revision: 1 };
const choice = (name: string, installed = true, enabled = installed): HarnessChoice =>
  ({ name, installed, enabled, harness: { skills: `~/.${name}/skills`, instructions: null, instructions_format: 'block', mode: 'symlink' } });
let current: StatusView;
beforeEach(() => {
  vi.resetAllMocks();
  vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() })));
  current = firstRun;
  vi.mocked(api.status).mockImplementation(async () => current);
  vi.mocked(api.connectionInfo).mockResolvedValue('/daemon.sock');
  vi.mocked(api.skills).mockResolvedValue([]); vi.mocked(api.recovered).mockResolvedValue([]);
  vi.mocked(api.imports).mockResolvedValue({ checked_at: null, updates: [], errors: [] });
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [] });
  vi.mocked(api.setupHarnesses).mockResolvedValue([choice('claude'), choice('codex'), choice('gemini', false)]);
  vi.mocked(api.setSetupHarness).mockImplementation(async (name, enabled) => [choice('claude'), choice('codex', true, name === 'codex' ? enabled : true), choice('gemini', false)]);
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); localStorage.clear(); window.location.hash = ''; });

const continueTo = async (path?: string) => {
  if (path) fireEvent.click(await screen.findByRole('radio', { name: new RegExp(path) }));
  fireEvent.click(await screen.findByRole('button', { name: 'Continue' }));
  await screen.findByRole('list', { name: 'Harnesses' });
};

it('chooses Harnesses before anything is synced', async () => {
  render(<App />);
  await continueTo();
  expect(screen.getByRole('checkbox', { name: 'Sync claude' }).getAttribute('aria-checked')).toBe('true');
  expect(screen.queryByRole('checkbox', { name: 'Sync gemini' })).toBeNull();
  expect(screen.getByText('Not found on this Machine: gemini')).toBeTruthy();
  fireEvent.click(screen.getByRole('checkbox', { name: 'Sync codex' }));
  await waitFor(() => expect(api.setSetupHarness).toHaveBeenCalledWith('codex', false));
  await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Sync codex' }).getAttribute('aria-checked')).toBe('false'));
  fireEvent.click(screen.getByRole('button', { name: 'Back' }));
  expect(await screen.findByRole('radio', { name: /Only this Machine/ })).toBeTruthy();
});

it('joins a Fleet, waits for the restart and turns everything on', async () => {
  vi.mocked(api.join).mockImplementation(async () => { current = joined; return { machine_id: 'box', repo: '/home/me/skills', setup: 'cloned', harnesses: ['claude'], revision: 1 }; });
  vi.mocked(api.harnesses).mockResolvedValue([{ name: 'claude', origin: 'detected', harness: choice('claude').harness }]);
  vi.mocked(api.setHarnessAssignment).mockResolvedValue({ assignment: { harnesses: {} }, revision: 2, entries: [] });
  render(<App />);
  await continueTo('Join a Fleet');
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  fireEvent.change(await screen.findByLabelText('Server URL'), { target: { value: 'http://10.0.0.5:7878' } });
  expect(screen.getByText(/Plain HTTP sends this Machine's token unencrypted/)).toBeTruthy();
  fireEvent.change(screen.getByLabelText('Server URL'), { target: { value: ' https://skills.example.com ' } });
  fireEvent.change(screen.getByLabelText('Join code'), { target: { value: 'ONCE' } });
  fireEvent.click(screen.getByRole('button', { name: 'Join Fleet' }));
  await waitFor(() => expect(api.join).toHaveBeenCalledWith({ server_url: 'https://skills.example.com', code: 'ONCE', admin_token: undefined, machine_id: undefined, copy_from: undefined }));
  fireEvent.click(await screen.findByRole('button', { name: 'Turn everything on' }));
  await waitFor(() => expect(api.setHarnessAssignment).toHaveBeenCalledWith('claude', true));
  expect(await screen.findByRole('heading', { name: 'Fleet' })).toBeTruthy();
});

it('reports failures from turning everything on before leaving', async () => {
  current = joined;
  vi.mocked(api.harnesses).mockResolvedValue(['claude', 'codex', 'gemini'].map(name => ({ name, origin: 'detected' as const, harness: choice(name).harness })));
  vi.mocked(api.setHarnessAssignment).mockImplementation(async harness => {
    if (harness === 'gemini') throw 'Server unreachable';
    return { assignment: { harnesses: {} }, revision: 2, entries: [], errors: harness === 'codex' ? ['placing skill:pdf: permission denied'] : [] };
  });
  const onDone = vi.fn();
  render(<StartingAssignment status={joined} onDone={onDone} openMatrix={vi.fn()} />);
  fireEvent.click(screen.getByRole('button', { name: 'Turn everything on' }));
  const notice = await screen.findByRole('alert');
  expect(within(notice).getByText('Not everything was turned on')).toBeTruthy();
  expect(within(notice).getByText('codex: placing skill:pdf: permission denied')).toBeTruthy();
  expect(within(notice).getByText('gemini: Server unreachable')).toBeTruthy();
  expect(api.setHarnessAssignment).toHaveBeenCalledTimes(3);
  expect(onDone).not.toHaveBeenCalled();
  expect(screen.queryByRole('button', { name: 'Leave everything off' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  expect(onDone).toHaveBeenCalledOnce();
});

it('keeps the earlier failures when a retry fails before writing anything', async () => {
  current = joined;
  vi.mocked(api.harnesses).mockResolvedValueOnce(['claude', 'codex'].map(name => ({ name, origin: 'detected' as const, harness: choice(name).harness })));
  vi.mocked(api.setHarnessAssignment).mockImplementation(async harness => { if (harness === 'codex') throw 'Server unreachable'; return { assignment: { harnesses: {} }, revision: 2, entries: [] }; });
  const onDone = vi.fn();
  render(<StartingAssignment status={joined} onDone={onDone} openMatrix={vi.fn()} />);
  fireEvent.click(screen.getByRole('button', { name: 'Turn everything on' }));
  expect(await screen.findByText('codex: Server unreachable')).toBeTruthy();
  vi.mocked(api.harnesses).mockRejectedValueOnce('Daemon unreachable');
  fireEvent.click(screen.getByRole('button', { name: 'Turn everything on' }));
  expect(await screen.findByText('Daemon unreachable')).toBeTruthy();
  expect(screen.getByText('codex: Server unreachable')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Leave everything off' })).toBeNull();
  expect(onDone).not.toHaveBeenCalled();
});

it('says when no Harness is on instead of leaving as if everything was turned on', async () => {
  current = joined;
  vi.mocked(api.harnesses).mockResolvedValue([]);
  const onDone = vi.fn();
  render(<StartingAssignment status={joined} onDone={onDone} openMatrix={vi.fn()} />);
  fireEvent.click(screen.getByRole('button', { name: 'Turn everything on' }));
  expect(await screen.findByText('No Harness is on')).toBeTruthy();
  expect(onDone).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  expect(onDone).toHaveBeenCalledOnce();
});

it('keeps the wizard on Harnesses while a choice saves, then lets it continue past a failed save', async () => {
  let fail!: (error: unknown) => void;
  vi.mocked(api.setSetupHarness).mockImplementation(() => new Promise((_, reject) => { fail = reject; }));
  render(<App />);
  await continueTo();
  fireEvent.click(screen.getByRole('checkbox', { name: 'Sync codex' }));
  await waitFor(() => expect(api.setSetupHarness).toHaveBeenCalledWith('codex', false));
  expect((screen.getByRole('button', { name: 'Continue' }) as HTMLButtonElement).disabled).toBe(true);
  expect((screen.getByRole('button', { name: 'Back' }) as HTMLButtonElement).disabled).toBe(true);
  fail('machine.toml is read-only');
  expect(await screen.findByText('machine.toml is read-only')).toBeTruthy();
  expect((screen.getByRole('button', { name: 'Continue' }) as HTMLButtonElement).disabled).toBe(false);
  expect((screen.getByRole('button', { name: 'Back' }) as HTMLButtonElement).disabled).toBe(false);
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  expect(await screen.findByRole('button', { name: 'Create skills repo' })).toBeTruthy();
});

it('starts a new Fleet with the admin token and skips straight to the matrix', async () => {
  vi.mocked(api.join).mockImplementation(async () => { current = joined; return { machine_id: 'first', repo: '/home/me/skills', setup: 'cloned', harnesses: [], revision: 1 }; });
  render(<App />);
  await continueTo('Join as the first Machine');
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  fireEvent.change(await screen.findByLabelText('Server URL'), { target: { value: 'https://skills.example.com' } });
  fireEvent.change(screen.getByLabelText('Admin token'), { target: { value: 'secret' } });
  fireEvent.change(screen.getByLabelText('Machine id'), { target: { value: 'first' } });
  expect(screen.queryByLabelText('Copy Assignment from Machine')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Join Fleet' }));
  await waitFor(() => expect(api.join).toHaveBeenCalledWith({ server_url: 'https://skills.example.com', code: '', admin_token: 'secret', machine_id: 'first', copy_from: undefined }));
  fireEvent.click(await screen.findByRole('button', { name: 'Choose in the Assignment matrix' }));
  expect(await screen.findByRole('heading', { name: 'Assignment matrix' })).toBeTruthy();
  expect(api.setHarnessAssignment).not.toHaveBeenCalled();
});

it('skips the starting Assignment when it was copied from another Machine', async () => {
  vi.mocked(api.join).mockImplementation(async () => { current = joined; return { machine_id: 'box', repo: '/home/me/skills', setup: 'cloned', harnesses: [], revision: 1 }; });
  render(<App />);
  await continueTo('Join a Fleet');
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  fireEvent.change(await screen.findByLabelText('Server URL'), { target: { value: 'https://skills.example.com' } });
  fireEvent.change(screen.getByLabelText('Join code'), { target: { value: 'ONCE' } });
  fireEvent.change(screen.getByLabelText('Copy Assignment from Machine'), { target: { value: 'laptop' } });
  fireEvent.click(screen.getByRole('button', { name: 'Join Fleet' }));
  expect(await screen.findByRole('heading', { name: 'Fleet' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Turn everything on' })).toBeNull();
});

it('keeps the wizard and shows the error when joining fails', async () => {
  vi.mocked(api.join).mockRejectedValue('the server rejected the admin token');
  render(<App />);
  await continueTo('Join as the first Machine');
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  fireEvent.change(await screen.findByLabelText('Server URL'), { target: { value: 'https://skills.example.com' } });
  fireEvent.change(screen.getByLabelText('Admin token'), { target: { value: 'wrong' } });
  fireEvent.click(screen.getByRole('button', { name: 'Join Fleet' }));
  expect(await screen.findByText('the server rejected the admin token')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Join Fleet' })).toBeTruthy();
});

it('starts the Daemon from the connection screen', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'cannot reach the daemon', status: null, unreachable: true });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: '/home/me/.cargo/bin/sk1ll', manager: 'systemd', available: true, active: false, restartable: false, unit: null });
  vi.mocked(api.startDaemon).mockResolvedValue('systemd');
  render(<App />);
  expect(await screen.findByText(/Installs the sk1ll-daemon user service/)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Start the Daemon' }));
  await waitFor(() => expect(api.startDaemon).toHaveBeenCalledOnce());
  expect(await screen.findByRole('button', { name: 'Daemon started' })).toBeTruthy();
});

it('explains a missing sk1ll command instead of offering to start the Daemon', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'cannot reach the daemon', status: null, unreachable: true });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: null, manager: 'systemd', available: true, active: false, restartable: false, unit: null });
  render(<App />);
  expect(await screen.findByText('The sk1ll command is not installed')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Start the Daemon' })).toBeNull();
  expect(screen.getByRole('button', { name: 'Copy sk1ll daemon' })).toBeTruthy();
});

it('shows the getting-started checklist after setup until it is dismissed', async () => {
  current = joined;
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [{ id: 'box', status: 'in_sync', connected: true, joined_at: 0, last_seen: null, report: joined.report }] });
  showGettingStarted();
  window.location.hash = '#/library';
  render(<App />);
  const panel = await screen.findByRole('region', { name: 'Getting started' });
  expect(within(panel).getByText('0 of 3 done')).toBeTruthy();
  fireEvent.click(within(panel).getByRole('button', { name: 'New Rule' }));
  expect(screen.getByText('Local library').getAttribute('data-create')).toBe('rule');
  expect(within(panel).getByRole('button', { name: 'Create join code' })).toBeTruthy();
  fireEvent.click(within(panel).getByRole('button', { name: 'Dismiss' }));
  expect(screen.queryByRole('region', { name: 'Getting started' })).toBeNull();
  expect(localStorage.getItem('sk1ll-getting-started')).toBeNull();
});

it('reopens the checklist for Machines set up before', async () => {
  current = joined;
  window.location.hash = '#/library';
  render(<App />);
  await screen.findByText('Local library');
  expect(screen.queryByRole('region', { name: 'Getting started' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Open command palette' }));
  fireEvent.click(await screen.findByRole('option', { name: 'Action: Show getting started' }));
  expect(await screen.findByRole('region', { name: 'Getting started' })).toBeTruthy();
});


it.each([true, false])('offers restart guidance on an API mismatch (active service: %s)', async active => {
  vi.mocked(api.status).mockRejectedValue({ message: 'local API mismatch', status: 426, unreachable: false });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: null, manager: 'systemd', available: true, active, restartable: active, unit: '/user/sk1ll-daemon.service' });
  render(<App />);
  if (active) {
    fireEvent.click(await screen.findByRole('button', { name: 'Restart daemon' }));
    await waitFor(() => expect(api.restartDaemon).toHaveBeenCalledOnce());
  } else {
    expect(await screen.findByRole('button', { name: 'Copy systemctl --user restart sk1ll-daemon' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Restart daemon' })).toBeNull();
  }
});

it('shows terminal restart instructions when the service manager query fails', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'local API mismatch', status: 426, unreachable: false });
  vi.mocked(api.daemonService).mockRejectedValue(new Error('service info unavailable'));
  render(<App />);
  expect(await screen.findByRole('button', { name: 'Copy sk1ll daemon' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: /Copy (systemctl|launchctl)/ })).toBeNull();
});

it('offers a complete one-line start command for an installed launch agent', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'cannot reach the daemon', status: null, unreachable: true });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: null, manager: 'launchd', available: true, active: false, restartable: false, unit: '/Users/me/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist' });
  render(<App />);
  const target = 'gui/$(id -u)/app.v1p3r.sk1ll.daemon';
  const command = `mkdir -p ~/Library/Logs/sk1ll && launchctl enable ${target} && (launchctl print ${target} >/dev/null 2>&1 || launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist) && launchctl kickstart ${target}`;
  expect(await screen.findByRole('button', { name: `Copy ${command}` })).toBeTruthy();
});

it('shows launchd commands on macOS', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'local API mismatch', status: 426, unreachable: false });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: null, manager: 'launchd', available: true, active: false, restartable: false, unit: null });
  render(<App />);
  expect(await screen.findByRole('button', { name: 'Copy launchctl kickstart -k gui/$(id -u)/app.v1p3r.sk1ll.daemon' })).toBeTruthy();
  expect(screen.queryByText(/systemctl/)).toBeNull();
});

it('installs the launch agent from the connection screen on macOS', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'cannot reach the daemon', status: null, unreachable: true });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: '/Applications/0xSk1ll.app/Contents/MacOS/sk1ll', manager: 'launchd', available: true, active: false, restartable: false, unit: null });
  render(<App />);
  expect(await screen.findByText(/Installs the app.v1p3r.sk1ll.daemon launch agent/)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Start it yourself' }));
  expect(await screen.findByRole('button', { name: 'Copy sk1ll daemon' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: /Copy launchctl/ })).toBeNull();
});

it('opens the selected Machine from the command palette', async () => {
  current = joined;
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: ['desktop', 'laptop'].map(id => ({
    id, status: 'offline', connected: false, joined_at: 0, last_seen: null, report: null,
  })) });
  window.location.hash = '#/library';
  render(<App />);
  fireEvent.click(await screen.findByRole('button', { name: 'Open command palette' }));
  fireEvent.click(await screen.findByRole('option', { name: 'Machine: laptop' }));
  expect(await screen.findByRole('button', { name: 'Hide details for laptop' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Show details for desktop' })).toBeNull();
});

it('names origin when a standalone push fails', async () => {
  current = { ...firstRun, has_origin: true, repo_ready: true };
  vi.mocked(api.push).mockResolvedValue({ pushed: false });
  window.location.hash = '#/library';
  render(<App />);
  fireEvent.click(await screen.findByRole('button', { name: 'Open command palette' }));
  fireEvent.click(await screen.findByRole('option', { name: 'Action: Push' }));
  expect(await screen.findByText('Push did not reach origin')).toBeTruthy();
});

it('leaves the restart screen when the Daemon becomes unreachable after a mismatch', async () => {
  vi.mocked(api.status).mockRejectedValueOnce({ message: 'local API mismatch', status: 426, unreachable: false })
    .mockRejectedValue({ message: 'connection refused', unreachable: true });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: '/home/me/.cargo/bin/sk1ll', manager: 'systemd', available: true, active: false, restartable: false, unit: null });
  render(<App />);
  expect(await screen.findByText('Daemon is not running')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Restart daemon' })).toBeNull();
});

it('closes a reopened checklist once its last step is done', async () => {
  const ready = { ...firstRun, repo_ready: true, report: { ...firstRun.report, baseline: 'abc' } };
  const props = { status: ready, machines: undefined, reopened: true, onDismiss: vi.fn(), addImport: vi.fn(), newSkill: vi.fn(), newRule: vi.fn(), showUnmanaged: vi.fn(), reviewDraft: vi.fn() };
  const { rerender } = render(<GettingStarted {...props} entries={[]} />);
  expect(await screen.findByRole('region', { name: 'Getting started' })).toBeTruthy();
  rerender(<GettingStarted {...props} entries={[{ type: 'personal', name: 'pdf' } as ContentEntry]} />);
  await waitFor(() => expect(screen.queryByRole('region', { name: 'Getting started' })).toBeNull());
  expect(props.onDismiss).toHaveBeenCalled();
});
