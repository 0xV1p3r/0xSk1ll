// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { App } from './App';
import { api } from './api';
import { showGettingStarted } from './Setup';
import type { HarnessChoice, StatusView } from './types';
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock('./api', () => ({ api: { status: vi.fn(), connectionInfo: vi.fn(), skills: vi.fn(), recovered: vi.fn(), imports: vi.fn(), fleet: vi.fn(), log: vi.fn(), logFacets: vi.fn(), init: vi.fn(),
  setupHarnesses: vi.fn(), setSetupHarness: vi.fn(), daemonService: vi.fn(), startDaemon: vi.fn(), join: vi.fn(), harnesses: vi.fn(), items: vi.fn(), setAssignment: vi.fn(), joinCode: vi.fn() }, errorMessage: String }));
vi.mock('./Skills', () => ({ Skills: () => <div>Local library</div> }));
vi.mock('./Matrix', () => ({ Matrix: () => <div>Matrix</div> }));

const firstRun: StatusView = { mode: 'standalone', has_origin: false, repo_ready: false, machine_id: 'box', server_url: null, repo: '/home/me/skills', headline: 'in_sync', connection: 'standalone', last_error: null, revoked: false, server_baseline: null, assignment_revision: 0, awaiting_assignment: false, auth_rejected_since: null, pending_discards: 0, report: { baseline: null, drafts: [], unpushed: [], conflicts: [], harnesses: [], blocked: false, clashes: [], behind: false } };
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
  vi.mocked(api.items).mockResolvedValue([{ item: 'skill:pdf', in_baseline: true, draft: null, unpushed: false, description: null }]);
  vi.mocked(api.setAssignment).mockResolvedValue({ assignment: { harnesses: {} }, revision: 2, entries: [] });
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
  await waitFor(() => expect(api.setAssignment).toHaveBeenCalledWith('box', { changes: [
    { harness: 'claude', kind: 'new_skills_default', enabled: true }, { harness: 'claude', kind: 'new_rules_default', enabled: true },
    { harness: 'claude', kind: 'skill', name: 'pdf', enabled: true },
  ] }));
  expect(await screen.findByRole('heading', { name: 'Fleet' })).toBeTruthy();
});

it('starts a new Fleet with the admin token and skips straight to the matrix', async () => {
  vi.mocked(api.join).mockImplementation(async () => { current = joined; return { machine_id: 'first', repo: '/home/me/skills', setup: 'initialized', harnesses: [], revision: 1 }; });
  render(<App />);
  await continueTo('Start a new Fleet');
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  fireEvent.change(await screen.findByLabelText('Server URL'), { target: { value: 'https://skills.example.com' } });
  fireEvent.change(screen.getByLabelText('Admin token'), { target: { value: 'secret' } });
  fireEvent.change(screen.getByLabelText('Machine id'), { target: { value: 'first' } });
  expect(screen.queryByLabelText('Copy Assignment from Machine')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Start Fleet' }));
  await waitFor(() => expect(api.join).toHaveBeenCalledWith({ server_url: 'https://skills.example.com', code: '', admin_token: 'secret', machine_id: 'first', copy_from: undefined }));
  fireEvent.click(await screen.findByRole('button', { name: 'Choose in the Assignment matrix' }));
  expect(await screen.findByRole('heading', { name: 'Assignment matrix' })).toBeTruthy();
  expect(api.setAssignment).not.toHaveBeenCalled();
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
  await continueTo('Start a new Fleet');
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  fireEvent.change(await screen.findByLabelText('Server URL'), { target: { value: 'https://skills.example.com' } });
  fireEvent.change(screen.getByLabelText('Admin token'), { target: { value: 'wrong' } });
  fireEvent.click(screen.getByRole('button', { name: 'Start Fleet' }));
  expect(await screen.findByText('the server rejected the admin token')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Start Fleet' })).toBeTruthy();
});

it('starts the Daemon from the connection screen', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'cannot reach the daemon', status: null, unreachable: true });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: '/home/me/.cargo/bin/sk1ll', systemd: true, unit: null });
  vi.mocked(api.startDaemon).mockResolvedValue('systemd');
  render(<App />);
  expect(await screen.findByText(/Installs the sk1ll-daemon user service/)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Start the Daemon' }));
  await waitFor(() => expect(api.startDaemon).toHaveBeenCalledOnce());
  expect(await screen.findByRole('button', { name: 'Daemon started' })).toBeTruthy();
});

it('explains a missing sk1ll command instead of offering to start the Daemon', async () => {
  vi.mocked(api.status).mockRejectedValue({ message: 'cannot reach the daemon', status: null, unreachable: true });
  vi.mocked(api.daemonService).mockResolvedValue({ default_socket: true, binary: null, systemd: true, unit: null });
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
  expect(within(panel).getByRole('button', { name: 'Create join code' })).toBeTruthy();
  fireEvent.click(within(panel).getByRole('button', { name: 'Dismiss' }));
  expect(screen.queryByRole('region', { name: 'Getting started' })).toBeNull();
  expect(localStorage.getItem('sk1ll-getting-started')).toBeNull();
});

it('does not show the checklist to Machines set up before', async () => {
  current = joined;
  window.location.hash = '#/library';
  render(<App />);
  await screen.findByText('Local library');
  expect(screen.queryByRole('region', { name: 'Getting started' })).toBeNull();
});
