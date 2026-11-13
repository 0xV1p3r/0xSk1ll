// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { App } from './App';
import { api } from './api';
import type { StatusView } from './types';
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock('./api', () => ({ api: { status: vi.fn(), connectionInfo: vi.fn(), skills: vi.fn(), recovered: vi.fn(), imports: vi.fn(), fleet: vi.fn(), log: vi.fn(), logFacets: vi.fn(), init: vi.fn() }, errorMessage: String }));
vi.mock('./Skills', () => ({ Skills: () => <div>Local library</div> }));
const status: StatusView = { mode: 'standalone', has_origin: false, repo_ready: true, machine_id: 'local', server_url: null, repo: '/skills', headline: 'in_sync', connection: 'standalone', last_error: null, revoked: false, server_baseline: null, assignment_revision: 0, awaiting_assignment: false, auth_rejected_since: null, pending_discards: 0, report: { baseline: null, drafts: [], unpushed: [], conflicts: [], harnesses: [], blocked: false, clashes: [], behind: false } };
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
  render(<App />);
  fireEvent.click(await screen.findByRole('button', { name: 'Create a skills repo' }));
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
