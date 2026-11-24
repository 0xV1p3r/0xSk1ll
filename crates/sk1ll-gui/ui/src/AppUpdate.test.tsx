// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { AppUpdateNotice, AppUpdateSettings } from './AppUpdate';
import { api } from './api';
import { checkAtStartup, resetAppUpdate, setCheckAutomatically } from './updateStore';
import type { AvailableUpdate } from './types';
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock('./api', () => ({ api: { updaterAvailable: vi.fn(), checkUpdate: vi.fn(), installUpdate: vi.fn() }, errorMessage: String }));
const update: AvailableUpdate = { version: '0.2.0', current_version: '0.1.0', notes: '### Added\n\n- Updates', date: null };
beforeEach(() => {
  vi.resetAllMocks(); localStorage.clear(); resetAppUpdate();
  vi.mocked(api.updaterAvailable).mockResolvedValue(true);
  vi.mocked(api.checkUpdate).mockResolvedValue(update);
  vi.mocked(api.installUpdate).mockResolvedValue();
});
afterEach(cleanup);

it('checks at startup and offers the update with its notes', async () => {
  render(<AppUpdateNotice canLeave={() => true} />);
  checkAtStartup();
  expect(await screen.findByText('0xSk1ll 0.2.0 is available')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: /Release notes/ }));
  expect(await screen.findByText(/- Updates/)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Update and restart' }));
  await waitFor(() => expect(api.installUpdate).toHaveBeenCalledWith('0.2.0'));
});
it('keeps unsaved edits when the discard prompt is cancelled', async () => {
  render(<AppUpdateNotice canLeave={() => false} />);
  checkAtStartup();
  fireEvent.click(await screen.findByRole('button', { name: 'Update and restart' }));
  expect(api.installUpdate).not.toHaveBeenCalled();
});
it('hides a skipped version', async () => {
  render(<AppUpdateNotice canLeave={() => true} />);
  checkAtStartup();
  fireEvent.click(await screen.findByRole('button', { name: 'Skip this version' }));
  expect(screen.queryByText('0xSk1ll 0.2.0 is available')).toBeNull();
  expect(JSON.parse(localStorage.getItem('sk1ll-skipped-updates') ?? '[]')).toEqual(['0.2.0']);
});
it('skips the startup check when it is turned off, and checks on demand in Settings', async () => {
  setCheckAutomatically(false);
  vi.mocked(api.checkUpdate).mockResolvedValue(null);
  render(<AppUpdateSettings canLeave={() => true} />);
  checkAtStartup();
  await waitFor(() => expect(api.updaterAvailable).toHaveBeenCalled());
  expect(api.checkUpdate).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Check now' }));
  expect(await screen.findByText('Up to date')).toBeTruthy();
});
it('says when this build cannot update itself', async () => {
  vi.mocked(api.updaterAvailable).mockResolvedValue(false);
  render(<AppUpdateSettings canLeave={() => true} />);
  checkAtStartup();
  expect(await screen.findByText(/can't update itself/)).toBeTruthy();
  expect(api.checkUpdate).not.toHaveBeenCalled();
});
it('shows why an install failed', async () => {
  vi.mocked(api.installUpdate).mockRejectedValue('Installing the update failed: dpkg refused');
  render(<AppUpdateNotice canLeave={() => true} />);
  checkAtStartup();
  fireEvent.click(await screen.findByRole('button', { name: 'Update and restart' }));
  expect(await screen.findByText(/dpkg refused/)).toBeTruthy();
});
