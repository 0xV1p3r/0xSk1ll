// Getting a Machine going: the first-run wizard, joining a Fleet, and reaching a Daemon that isn't running.
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import type { UserEvent } from '@testing-library/user-event';
import { checked, heading, isDisabled, launch } from './harness';
import type { MockDaemon } from '../dev/mock';
import type { ServiceInfo } from '../types';

const toHarnesses = async (user: UserEvent, path?: RegExp) => {
  if (path) await user.click(await screen.findByRole('radio', { name: path }));
  await user.click(await screen.findByRole('button', { name: 'Continue' }));
  return within(await screen.findByRole('list', { name: 'Harnesses' }));
};
const toJoin = async (user: UserEvent, path: RegExp) => {
  await toHarnesses(user, path);
  await user.click(screen.getByRole('button', { name: 'Continue' }));
  return screen.findByLabelText('Server URL');
};

it('sets up a standalone Machine: chooses Harnesses before anything syncs, creates the repo, and follows the checklist', async () => {
  const { daemon, user } = launch('standalone-first-run');
  const harnesses = await toHarnesses(user);
  expect(checked(harnesses.getByRole('checkbox', { name: 'Sync claude' }))).toBe(true);
  expect(harnesses.queryByRole('checkbox', { name: 'Sync gemini' })).toBeNull();
  expect(screen.getByText('Not found on this Machine: gemini')).toBeTruthy();

  // While a choice saves the wizard stays put; a failed save doesn't trap it.
  let fail!: (error: unknown) => void;
  daemon.override('set_setup_harness', () => new Promise((_, reject) => { fail = reject; }));
  await user.click(harnesses.getByRole('checkbox', { name: 'Sync codex' }));
  expect(isDisabled(screen.getByRole('button', { name: 'Continue' }))).toBe(true);
  expect(isDisabled(screen.getByRole('button', { name: 'Back' }))).toBe(true);
  fail({ message: 'machine.toml is read-only' });
  expect(await screen.findByText('machine.toml is read-only')).toBeTruthy();
  expect(isDisabled(screen.getByRole('button', { name: 'Continue' }))).toBe(false);
  daemon.restore('set_setup_harness');
  await user.click(harnesses.getByRole('checkbox', { name: 'Sync codex' }));
  await waitFor(() => expect(checked(harnesses.getByRole('checkbox', { name: 'Sync codex' }))).toBe(false));
  expect(daemon.sent('set_setup_harness').at(-1)).toEqual({ name: 'codex', enabled: false });
  await user.click(screen.getByRole('button', { name: 'Back' }));
  expect(await screen.findByRole('radio', { name: /Only this Machine/ })).toBeTruthy();

  await toHarnesses(user);
  await user.click(screen.getByRole('button', { name: 'Continue' }));
  await user.click(await screen.findByRole('button', { name: 'Create skills repo' }));
  await heading('Library');
  expect(daemon.sent('init')).toHaveLength(1);
  const checklist = within(await screen.findByRole('region', { name: 'Getting started' }));
  expect(checklist.getByText('0 of 2 done')).toBeTruthy();
  expect(within(checklist.getByText(/^Commit your Draft/).closest('li')!).getByText(', to do')).toBeTruthy();
  await user.click(checklist.getByRole('button', { name: 'New Rule' }));
  expect(await screen.findByRole('dialog', { name: 'New Rule' })).toBeTruthy();
  await user.keyboard('{Escape}');
  await user.click(checklist.getByRole('button', { name: 'Dismiss' }));
  expect(screen.queryByRole('region', { name: 'Getting started' })).toBeNull();
  expect(localStorage.getItem('sk1ll-getting-started')).toBeNull();
  expect(daemon.sent('fleet')).toEqual([]);
});

it('joins a Fleet: warns about plain HTTP, waits out the Daemon restart, then turns everything on and reports what failed', async () => {
  const { daemon, user } = launch('standalone-first-run');
  const url = await toJoin(user, /Join a Fleet/);
  await user.type(url, 'http://10.0.0.5:7878');
  expect(screen.getByText(/Plain HTTP sends this Machine's token unencrypted/)).toBeTruthy();
  await user.clear(url); await user.type(url, 'http://localhost:7878');
  expect(screen.queryByText(/Plain HTTP/)).toBeNull();
  await user.clear(url); await user.type(url, ' https://skills.example.com ');
  await user.type(screen.getByLabelText('Join code'), 'ONCE');
  await user.click(screen.getByRole('button', { name: 'Join Fleet' }));

  expect(await screen.findByText('Choose the starting Assignment')).toBeTruthy();
  expect(daemon.sent('join')).toEqual([{ request: { server_url: 'https://skills.example.com', code: 'ONCE' } }]);
  daemon.override('set_harness_assignment', args => args.harness === 'codex' ? Promise.reject({ message: 'Server unreachable' }) : undefined);
  await user.click(screen.getByRole('button', { name: 'Turn everything on' }));
  const notice = within(await screen.findByRole('alert'));
  expect(notice.getByText('Not everything was turned on')).toBeTruthy();
  expect(notice.getByText('codex: Server unreachable')).toBeTruthy();
  expect(daemon.sent('set_harness_assignment')).toEqual([{ harness: 'claude', enabled: true }, { harness: 'codex', enabled: true }]);
  expect(screen.queryByRole('button', { name: 'Leave everything off' })).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Continue' }));
  await heading('Fleet');
});

it('joins with an Assignment copied from another Machine, skipping the starting Assignment', async () => {
  const { daemon, user } = launch('standalone-first-run');
  await user.type(await toJoin(user, /Join a Fleet/), 'https://skills.example.com');
  await user.type(screen.getByLabelText('Join code'), 'ONCE');
  await user.type(screen.getByLabelText('Copy Assignment from Machine'), 'laptop');
  await user.click(screen.getByRole('button', { name: 'Join Fleet' }));
  await heading('Fleet');
  expect(screen.queryByRole('button', { name: 'Turn everything on' })).toBeNull();
  expect(daemon.sent('join')[0]).toMatchObject({ request: { copy_from: 'laptop' } });
});

it('starts a new Fleet as its first Machine with the admin token, keeping the wizard while the join fails', async () => {
  const { daemon, user } = launch('standalone-first-run');
  await user.type(await toJoin(user, /Join as the first Machine/), 'https://skills.example.com');
  await user.type(screen.getByLabelText('Admin token'), 'wrong');
  await user.type(screen.getByLabelText('Machine id'), 'first');
  expect(screen.queryByLabelText('Copy Assignment from Machine')).toBeNull();
  daemon.override('join', () => Promise.reject({ message: 'the server rejected the admin token' }));
  await user.click(screen.getByRole('button', { name: 'Join Fleet' }));
  expect(await screen.findByText('the server rejected the admin token')).toBeTruthy();
  daemon.restore('join');
  await user.click(screen.getByRole('button', { name: 'Join Fleet' }));
  expect(daemon.sent('join').at(-1)).toEqual({ request: { server_url: 'https://skills.example.com', code: '', admin_token: 'wrong', machine_id: 'first' } });
  await user.click(await screen.findByRole('button', { name: 'Choose in the Assignment matrix' }));
  await heading('Assignment matrix');
  expect(daemon.sent('set_harness_assignment')).toEqual([]);
});

const service = (overrides: Partial<ServiceInfo>): ServiceInfo => ({ default_socket: true, binary: '/home/dev/.cargo/bin/sk1ll', manager: 'systemd', available: true, active: false, restartable: false, unit: null, ...overrides });
const mismatch = { message: 'local API mismatch', status: 426, unreachable: false };
const launchd = 'gui/$(id -u)/app.v1p3r.sk1ll.daemon';
it.each<{ name: string; mismatch?: boolean; info: ServiceInfo | 'fails'; check: (user: UserEvent, daemon: MockDaemon) => Promise<void> }>([
  { name: 'starts a stopped Daemon as its systemd user service', info: service({}), check: async (user, daemon) => {
    expect(screen.getByText(/Installs the sk1ll-daemon user service/)).toBeTruthy();
    await user.click(screen.getByRole('button', { name: 'Start the Daemon' }));
    expect(await screen.findByRole('button', { name: 'Daemon started' })).toBeTruthy();
    expect(daemon.sent('start_daemon')).toHaveLength(1);
  } },
  { name: 'explains a missing sk1ll command instead of offering to start it', info: service({ binary: null }), check: async () => {
    expect(screen.getByText('The sk1ll command is not installed')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Start the Daemon' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Copy sk1ll daemon' })).toBeTruthy();
  } },
  { name: 'gives one complete command for an installed launch agent', info: service({ binary: null, manager: 'launchd', unit: '/Users/me/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist' }), check: async () => {
    expect(screen.getByRole('button', { name: `Copy mkdir -p ~/Library/Logs/sk1ll && launchctl enable ${launchd} && (launchctl print ${launchd} >/dev/null 2>&1 || launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/app.v1p3r.sk1ll.daemon.plist) && launchctl kickstart ${launchd}` })).toBeTruthy();
  } },
  { name: 'installs the launch agent on macOS, or runs it in a terminal', info: service({ binary: '/Applications/0xSk1ll.app/Contents/MacOS/sk1ll', manager: 'launchd' }), check: async user => {
    expect(screen.getByText(/Installs the app.v1p3r.sk1ll.daemon launch agent/)).toBeTruthy();
    await user.click(screen.getByRole('button', { name: 'Start it yourself' }));
    expect(await screen.findByRole('button', { name: 'Copy sk1ll daemon' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: /Copy launchctl/ })).toBeNull();
  } },
  { name: 'restarts an incompatible Daemon run by an active service', mismatch: true, info: service({ active: true, restartable: true, unit: '/user/sk1ll-daemon.service' }), check: async (user, daemon) => {
    await user.click(screen.getByRole('button', { name: 'Restart daemon' }));
    await waitFor(() => expect(daemon.sent('restart_daemon')).toHaveLength(1));
  } },
  { name: 'gives the systemd restart command for an incompatible Daemon it cannot restart', mismatch: true, info: service({ binary: null, unit: '/user/sk1ll-daemon.service' }), check: async () => {
    expect(screen.getByRole('button', { name: 'Copy systemctl --user restart sk1ll-daemon' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Restart daemon' })).toBeNull();
  } },
  { name: 'gives the launchd restart command on macOS', mismatch: true, info: service({ binary: null, manager: 'launchd' }), check: async () => {
    expect(screen.getByRole('button', { name: `Copy launchctl kickstart -k ${launchd}` })).toBeTruthy();
    expect(screen.queryByText(/systemctl/)).toBeNull();
  } },
  { name: 'falls back to the terminal command when the service manager cannot be asked', mismatch: true, info: 'fails', check: async () => {
    expect(screen.getByRole('button', { name: 'Copy sk1ll daemon' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: /Copy (systemctl|launchctl)/ })).toBeNull();
  } },
])('connection screen $name', async ({ mismatch: incompatible, info, check }) => {
  const { daemon, user } = launch(incompatible ? 'fleet' : 'unreachable', { setup: daemon => {
    daemon.override('daemon_service', () => info === 'fails' ? Promise.reject({ message: 'service info unavailable' }) : info);
    if (incompatible) daemon.override('status', () => Promise.reject(mismatch));
  } });
  await screen.findByText(incompatible ? 'Restart the Daemon' : 'Daemon is not running');
  await waitFor(() => expect(daemon.sent('daemon_service').length).toBeGreaterThan(0));
  await new Promise(resolve => setTimeout(resolve));
  await check(user, daemon);
});

it('leaves the restart screen once the incompatible Daemon stops answering', async () => {
  let calls = 0;
  launch('fleet', { setup: daemon => daemon.override('status', () => Promise.reject(calls++ ? { message: 'connection refused', unreachable: true } : mismatch)) });
  expect(await screen.findByText('Daemon is not running')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Restart daemon' })).toBeNull();
});
