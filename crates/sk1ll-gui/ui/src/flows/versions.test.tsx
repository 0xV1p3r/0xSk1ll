// Versions: the app updating itself, and the header's warnings when the app, Daemon and Server run different releases.
// The app's own release comes from package.json; the samples below are pinned relative to 0.1.0.
import { expect, it } from 'vitest';
import { version as appVersion } from '../../package.json';
import { screen, waitFor, within } from '@testing-library/react';
import { heading, launch } from './harness';
import { resetAppUpdate } from '../updateStore';
import type { AvailableUpdate, StatusView } from '../types';

const update: AvailableUpdate = { version: '0.2.0', current_version: '0.1.0', notes: '### Added\n\n- Updates', date: null };
const notice = () => screen.queryByText('0xSk1ll 0.2.0 is available');
// Quits the app and starts it again, keeping what it stored.
const relaunch = (view: { unmount(): void }) => { view.unmount(); resetAppUpdate(); };

it('offers a newer release at startup with its notes, installs it, and says why an install failed', async () => {
  const { daemon, user } = launch('fleet', { update });
  const banner = within((await screen.findByText('0xSk1ll 0.2.0 is available')).closest('[role="status"]') as HTMLElement);
  expect(banner.getByText(/This app is 0.1.0/)).toBeTruthy();
  await user.click(banner.getByRole('button', { name: /Release notes/ }));
  expect(await banner.findByText(/- Updates/)).toBeTruthy();
  daemon.override('install_update', () => Promise.reject('Installing the update failed: dpkg refused'));
  await user.click(banner.getByRole('button', { name: 'Update and restart' }));
  expect(await banner.findByText(/dpkg refused/)).toBeTruthy();
  daemon.restore('install_update');
  await user.click(banner.getByRole('button', { name: 'Update and restart' }));
  await waitFor(() => expect(daemon.sent('install_update')).toEqual([{ version: '0.2.0' }, { version: '0.2.0' }]));
});

it('hides a skipped version for good', async () => {
  const { user, view } = launch('fleet', { update });
  await screen.findByText('0xSk1ll 0.2.0 is available');
  await user.click(screen.getByRole('button', { name: 'Skip this version' }));
  expect(notice()).toBeNull();

  relaunch(view);
  const { daemon } = launch('fleet', { hash: '#/settings', update });
  // Settings still offers it; the header doesn't.
  expect(await screen.findByText('0.2.0 is available')).toBeTruthy();
  expect(daemon.sent('check_update')).toEqual([{}]);
  expect(notice()).toBeNull();
});

it('checks only on demand once the startup check is off, and says when this build can\'t update itself', async () => {
  const first = launch('fleet', { hash: '#/settings', update });
  await first.user.click(await screen.findByRole('checkbox', { name: 'Check for updates when the app starts' }));
  relaunch(first.view);
  const second = launch('fleet', { hash: '#/settings', update });
  await screen.findByRole('button', { name: 'Check now' });
  expect(second.daemon.sent('updater_available')).toHaveLength(1);
  expect(second.daemon.sent('check_update')).toEqual([]);
  second.daemon.override('check_update', () => null);
  await second.user.click(screen.getByRole('button', { name: 'Check now' }));
  expect(await screen.findByText('Up to date')).toBeTruthy();

  relaunch(second.view);
  const { daemon } = launch('fleet', { hash: '#/settings' });
  expect(await screen.findByText(/can't update itself/)).toBeTruthy();
  expect(daemon.sent('check_update')).toEqual([]);
});

const versions = (over: Partial<StatusView>) => ({ setup: (daemon: { status: StatusView }) => { Object.assign(daemon.status, over); } });

it('says which side to upgrade when the Server speaks another protocol, and offers this app\'s update when it is the older one', async () => {
  const server = launch('fleet', versions({ connection: 'incompatible', version: '0.2.0 (abc1234567)', protocol: 2, server: { version: '0.1.0', protocol: 1 } }));
  expect(await screen.findByText('Server is out of date')).toBeTruthy();
  expect(screen.getByText(/SK1LL_SERVER_VERSION=0\.2\.0 docker compose/)).toBeTruthy();

  relaunch(server.view);
  const app = launch('fleet', { update, ...versions({ connection: 'incompatible', version: '0.1.0 (abc1234567)', protocol: 2, server: { version: '0.3.0', protocol: 3 } }) });
  expect(await screen.findByText('This app is out of date')).toBeTruthy();
  expect(await screen.findByRole('button', { name: 'Update to 0.2.0 and restart' })).toBeTruthy();

  relaunch(app.view);
  launch('fleet', versions({ connection: 'incompatible', version: '0.1.0', protocol: 2, server: { version: null, protocol: null }, last_error: 'protocol mismatch' }));
  expect(await screen.findByText('Server speaks another protocol')).toBeTruthy();
  expect(screen.getByText('protocol mismatch')).toBeTruthy();
});

it('offers to restart a Daemon older than the app, and hides an older Server once dismissed, across launches', async () => {
  const older = versions({ connection: 'connected', version: '0.1.0-rc.2 (abc1234567)', protocol: 2, server: { version: '0.1.0-rc.10', protocol: 2 } });
  const first = launch('fleet', older);
  // Pre-releases order before their release, and by number among themselves.
  expect(await screen.findByText('The Daemon still runs 0.1.0-rc.2')).toBeTruthy();
  expect(screen.queryByText('The Server runs an older release')).toBeNull();
  await first.user.click(screen.getByRole('button', { name: 'Restart daemon' }));
  await waitFor(() => expect(first.daemon.sent('restart_daemon')).toEqual([{}]));

  relaunch(first.view);
  const server = versions({ connection: 'connected', version: `${appVersion} (abc1234567)`, protocol: 2, server: { version: '0.0.9', protocol: 2 } });
  const second = launch('fleet', server);
  expect(await screen.findByText('The Server runs an older release')).toBeTruthy();
  expect(screen.queryByText(/The Daemon still runs/)).toBeNull();
  await second.user.click(screen.getByRole('button', { name: 'Dismiss for Server 0.0.9' }));
  expect(screen.queryByText('The Server runs an older release')).toBeNull();
  relaunch(second.view);
  const { view } = launch('fleet', server);
  await heading('Fleet');
  expect(screen.queryByText('The Server runs an older release')).toBeNull();

  // A development build has no release to compare.
  relaunch(view);
  launch('fleet', versions({ connection: 'connected', version: 'dev', protocol: 2, server: { version: '0.0.9', protocol: 2 } }));
  await heading('Fleet');
  expect(screen.queryByText(/The Daemon still runs|The Server runs an older release/)).toBeNull();
});
