// Watching the Fleet: health, filters, each Machine's details, this Machine's queue and Assignments.
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import { checked, dialog, heading, launch, nav, notify } from './harness';

it('reads Fleet health, filters Machines, and removes one after confirming', async () => {
  const { daemon, user } = launch();
  await heading('Fleet');
  const health = within(await screen.findByRole('region', { name: 'Fleet health' }));
  expect(await health.findByRole('img', { name: 'Fleet health: 0 In sync, 0 Drafting, 1 Behind, 1 Conflict, 1 Blocked, 1 Offline. 4 Machines total.' })).toBeTruthy();
  expect(health.getByText('9f3c2a17')).toBeTruthy();
  const filters = within(screen.getByRole('group', { name: 'Machine filter' }));
  const machines = () => within(screen.getByRole('table', { name: 'Machines' }));
  expect(filters.getByRole('button', { name: 'Filter: All' }).textContent).toBe('All4');
  expect(filters.getByRole('button', { name: 'Filter: Attention' }).textContent).toBe('Attention3');
  await user.click(filters.getByRole('button', { name: 'Filter: Attention' }));
  expect(machines().queryByText('laptop')).toBeNull();
  expect(machines().getByText('ci-runner')).toBeTruthy();
  expect(filters.getByRole('button', { name: 'Filter: Attention' }).getAttribute('aria-pressed')).toBe('true');
  await user.click(filters.getByRole('button', { name: 'Filter: Connected' }));
  expect(machines().queryByText('ci-runner')).toBeNull();
  expect(machines().getByText('laptop')).toBeTruthy();
  expect(health.getByRole('img').getAttribute('aria-label')).toContain('4 Machines total');
  await user.click(filters.getByRole('button', { name: 'Filter: All' }));
  expect(machines().getAllByRole('row')).toHaveLength(5);

  // Blocked guidance leads with Keep mine, and Restore takes the Baseline.
  expect(screen.getByText(/^On build-box, Keep mine on the clashing items/).textContent).toMatch(/or Restore them to take the Baseline/);

  const toggle = screen.getByRole('button', { name: 'Show details for workstation' });
  await user.click(toggle);
  expect(toggle.getAttribute('aria-expanded')).toBe('true');
  const details = within(document.getElementById(toggle.getAttribute('aria-controls')!)!);
  for (const label of ['Connection', 'Joined', 'Last seen', 'Harnesses']) expect(details.getByText(label)).toBeTruthy();
  expect(details.getByText('skill:code-review in codex')).toBeTruthy();
  expect(details.getByText('skill:release-notes, rule:style')).toBeTruthy();
  await user.click(details.getByRole('button', { name: 'Remove workstation from the Fleet' }));
  let confirm = await dialog('Remove workstation from the Fleet?');
  expect(confirm.getByText(/This removes THIS Machine/)).toBeTruthy();
  await user.click(confirm.getByRole('button', { name: 'Cancel' }));
  // A Machine with nothing to report hides the empty work fields.
  await user.click(screen.getByRole('button', { name: 'Show details for ci-runner' }));
  const offline = within(document.getElementById('machine-ci-runner')!);
  for (const label of ['Draft', 'Unpushed', 'Conflicts', 'Clashes']) expect(offline.queryByText(label)).toBeNull();
  await user.click(offline.getByRole('button', { name: 'Remove ci-runner from the Fleet' }));
  confirm = await dialog('Remove ci-runner from the Fleet?');
  expect(confirm.getByText(/revokes the Machine's credential/)).toBeTruthy();
  expect((confirm.getByRole('button', { name: 'Remove Machine' }) as HTMLButtonElement).disabled).toBe(true);
  await user.type(confirm.getByLabelText('Type ci-runner to confirm'), 'ci-runner');
  await user.click(confirm.getByRole('button', { name: 'Remove Machine' }));
  await waitFor(() => expect(machines().queryByText('ci-runner')).toBeNull());
  expect(daemon.sent('remove_machine')).toEqual([{ machine: 'ci-runner' }]);

  await user.click(screen.getByRole('button', { name: 'Add Machine' }));
  const code = await dialog('Join code');
  expect(await code.findByText('sk1ll-join-7Q2M-XK9P-44RD-ZT8W')).toBeTruthy();
  await user.keyboard('{Escape}');
  await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Add Machine' })));
  // A code may be for a Machine that manages only itself.
  await user.selectOptions(screen.getByLabelText('New Machine may manage'), 'self');
  await user.click(screen.getByRole('button', { name: 'Add Machine' }));
  expect((await dialog('Join code')).getByText('Only itself.')).toBeTruthy();
  expect(daemon.sent('join_code')).toEqual([{ scope: 'fleet' }, { scope: 'self' }]);

  // A failed refresh keeps the last Fleet data on screen.
  daemon.override('fleet', () => Promise.reject({ message: 'Refresh unavailable' }));
  await user.keyboard('{Escape}');
  await notify('fleet');
  expect(await screen.findByText('Refresh unavailable')).toBeTruthy();
  expect(screen.getByText('Showing last available Fleet data')).toBeTruthy();
  expect(machines().getByText('laptop')).toBeTruthy();
});

it('works through this Machine\'s queue and its Assignments from the Fleet screen', async () => {
  const { daemon, user } = launch();
  await heading('Fleet');
  const next = within(await screen.findByRole('region', { name: 'Up next' }));
  expect(await next.findByText('code-review in codex')).toBeTruthy();
  expect(next.getByText('2 Draft items')).toBeTruthy();
  expect(next.getByText('1 unmanaged skill · notes')).toBeTruthy();
  // Other Machines' work is theirs to do.
  expect(next.queryByText(/deploy/)).toBeNull();
  await user.click(next.getByRole('button', { name: 'Push' }));
  await waitFor(() => expect(next.queryByRole('button', { name: 'Push' })).toBeNull());
  expect(daemon.sent('push')).toHaveLength(1);

  const panel = within(await screen.findByRole('table', { name: 'This Machine assignments' }));
  const cell = await panel.findByRole('checkbox', { name: 'skill:code-review in workstation / claude, Inherited' });
  expect(checked(cell)).toBe(true);
  await user.click(cell);
  await waitFor(() => expect(checked(panel.getByRole('checkbox', { name: 'skill:code-review in workstation / claude, Explicit' }))).toBe(false));
  expect(daemon.sent('set_assignment')).toEqual([{ machine: 'workstation', changes: {
    changes: [{ harness: 'claude', kind: 'skill', item_id: 'id-code-review', name: 'code-review', enabled: false }],
    expected: { revision: 42, registration: 'reg-workstation' } } }]);
  // A refused toggle rolls back and says why.
  daemon.override('set_assignment', () => Promise.reject({ message: 'Server refused' }));
  await user.click(panel.getByRole('checkbox', { name: 'skill:code-review in workstation / claude, Explicit' }));
  expect(await screen.findByText('Server refused')).toBeTruthy();
  expect(checked(panel.getByRole('checkbox', { name: 'skill:code-review in workstation / claude, Explicit' }))).toBe(false);

  await user.click(next.getByRole('button', { name: 'Adopt unmanaged skills' }));
  await heading('Library');
  expect(await screen.findByText('notes')).toBeTruthy();
  await user.click(nav().getByRole('button', { name: 'Fleet' }));
  await user.click(await screen.findByRole('button', { name: 'Open matrix' }));
  await heading('Assignment matrix');
});

it('shows a Self-only Machine that it can remove only itself and cannot add Machines', async () => {
  const { user } = launch('self');
  await heading('Fleet');
  expect(await screen.findByText('Self-only')).toBeTruthy();
  expect(screen.getByText(/joined with a Self-only join code/)).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Add Machine' })).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Show details for workstation' }));
  await user.click(screen.getByRole('button', { name: 'Show details for laptop' }));
  expect(screen.getByText('Only itself')).toBeTruthy();
  expect(screen.getAllByText('Whole Fleet').length).toBeGreaterThan(0);
  expect(screen.getByRole('button', { name: 'Remove workstation from the Fleet' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Remove laptop from the Fleet' })).toBeNull();
});
