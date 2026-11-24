// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { SkillFile } from './SkillFile';
import { App } from './App';
import { api } from './api';
import type { StatusView } from './types';
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock('./api', () => ({ api: { status: vi.fn(), restartDaemon: vi.fn(), connectionInfo: vi.fn(), skills: vi.fn(), recovered: vi.fn(), imports: vi.fn(), fleet: vi.fn(), log: vi.fn(), logFacets: vi.fn(), init: vi.fn(), setupHarnesses: vi.fn(), writeSkillFile: vi.fn(), skillFile: vi.fn() }, errorMessage: String, errorStatus: (e: {status?: number}) => e?.status }));
vi.mock('./Skills', () => ({
  Skills: ({ fileEditState }: { fileEditState: { current: string } }) => <SkillFile
    file={{ path: 'SKILL.md', fingerprint: 'original', text: 'Original', binary: false, too_large: false, size: 8 }}
    editor={{ query: { item: 'skill:pdf' }, writable: true, setEditing: () => {}, changed: () => {},
      guard: state => { fileEditState.current = state; }, reload: () => {} }} />,
}));
const status: StatusView = { mode: 'standalone', has_origin: false, repo_ready: true, machine_id: 'local', server_url: null, repo: '/skills', headline: 'in_sync', connection: 'standalone', last_error: null, revoked: false, server_baseline: null, assignment_revision: 0, awaiting_assignment: false, auth_rejected_since: null, report: { seq: 1, items: [], baseline: null, drafts: [], unpushed: [], conflicts: [], harnesses: [], blocked: false, clashes: [], behind: false } };
beforeEach(() => {
  vi.resetAllMocks();
  vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() })));
  vi.mocked(api.status).mockResolvedValue(status);
  vi.mocked(api.connectionInfo).mockResolvedValue('/daemon.sock');
  vi.mocked(api.skills).mockResolvedValue([]); vi.mocked(api.recovered).mockResolvedValue([]);
  vi.mocked(api.imports).mockResolvedValue({ checked_at: null, updates: [], errors: [] });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); window.location.hash = ''; });
it.each(['', '#/fleet', '#/assignment-log'])('defaults standalone %s to Library and hides fleet screens', async hash => {
  window.location.hash = hash;
  render(<App />);
  await screen.findByRole('heading', { name: 'Library' });
  expect(screen.queryByRole('button', { name: 'Fleet' })).toBeNull();
  expect(screen.queryByRole('button', { name: 'Assignment log' })).toBeNull();
  expect(screen.getByRole('button', { name: 'Assignment' })).toBeTruthy();
  expect(api.fleet).not.toHaveBeenCalled(); expect(api.log).not.toHaveBeenCalled(); expect(api.logFacets).not.toHaveBeenCalled();
});
it('creates the skills repo on first run and refreshes status', async () => {
  let ready = false;
  vi.mocked(api.status).mockImplementation(async () => ({ ...status, repo_ready: ready }));
  vi.mocked(api.init).mockImplementation(async () => { ready = true; });
  vi.mocked(api.setupHarnesses).mockResolvedValue([]);
  render(<App />);
  fireEvent.click(await screen.findByRole('button', { name: 'Continue' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Continue' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Create skills repo' }));
  await waitFor(() => expect(api.init).toHaveBeenCalledOnce());
  await screen.findByRole('heading', { name: 'Library' });
  expect(api.fleet).not.toHaveBeenCalled();
});

it('does not request fleet or history during standalone background refresh', async () => {
  vi.useFakeTimers();
  try {
    await act(async () => { render(<App />); });
    await act(async () => { await vi.advanceTimersByTimeAsync(16000); });
    expect(api.status).toHaveBeenCalled();
    expect(api.fleet).not.toHaveBeenCalled(); expect(api.log).not.toHaveBeenCalled(); expect(api.logFacets).not.toHaveBeenCalled();
  } finally { cleanup(); vi.useRealTimers(); }
});

it('keeps a dirty Library editor mounted across daemon failure and saves against the original fingerprint', async () => {
  vi.useFakeTimers();
  try {
    await act(async () => { render(<App />); });
    fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'SKILL.md' }), { target: { value: 'Unsaved draft' } });
    vi.mocked(api.status).mockRejectedValue({ unreachable: true });
    await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
    expect(screen.getByText(/Daemon disconnected/)).toBeTruthy();
    expect((screen.getByRole('textbox', { name: 'SKILL.md' }) as HTMLTextAreaElement).value).toBe('Unsaved draft');
    vi.mocked(api.status).mockResolvedValue(status);
    await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
    expect(screen.queryByText(/Daemon disconnected/)).toBeNull();
    vi.mocked(api.writeSkillFile).mockRejectedValue({ status: 409 });
    vi.mocked(api.skillFile).mockResolvedValue({ path: 'SKILL.md', text: 'New disk content', fingerprint: 'new', binary: false, too_large: false, size: 16 });
    await act(async () => { fireEvent.click(screen.getByRole('button', { name: 'Save' })); });
    expect(api.writeSkillFile).toHaveBeenCalledWith({ item: 'skill:pdf', path: 'SKILL.md', expected: 'original', content: 'Unsaved draft' });
    expect(screen.getByText('File changed on disk')).toBeTruthy();
    vi.spyOn(window, 'confirm').mockReturnValueOnce(true);
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'SKILL.md' }));
  } finally { cleanup(); vi.useRealTimers(); }
});
it('says which side to upgrade when the Server speaks another protocol', async () => {
  vi.mocked(api.status).mockResolvedValue({ ...status, connection: 'incompatible', version: '0.2.0 (abc1234567)', protocol: 2, server: { version: '0.1.0', protocol: 1 } });
  render(<App />);
  expect(await screen.findByText('Server is out of date')).toBeTruthy();
  expect(screen.getByText(/SK1LL_SERVER_VERSION=0\.2\.0 docker compose/)).toBeTruthy();
});
it('offers to restart a Daemon older than the app, and hides a dismissed older Server', async () => {
  localStorage.clear();
  vi.mocked(api.status).mockResolvedValue({ ...status, connection: 'connected', version: '0.0.1 (abc1234567)', protocol: 2, server: { version: '0.0.0', protocol: 2 } });
  vi.mocked(api.restartDaemon).mockResolvedValue();
  render(<App />);
  expect(await screen.findByText('The Daemon still runs 0.0.1')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Restart daemon' }));
  await waitFor(() => expect(api.restartDaemon).toHaveBeenCalled());
  fireEvent.click(screen.getByRole('button', { name: 'Dismiss for Server 0.0.0' }));
  expect(screen.queryByText('The Server runs an older release')).toBeNull();
  expect(JSON.parse(localStorage.getItem('sk1ll-dismissed-server-versions') ?? '[]')).toEqual(['0.0.0']);
});
