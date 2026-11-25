// The app shell: which screens a Machine gets, moving between them, the command palette, and what each Daemon
// notification refreshes.
import { expect, it, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import { dialog, heading, launch, nav, notify } from './harness';

it('keeps a standalone Machine to its own screens and never asks for Fleet data, even in the background', async () => {
  for (const hash of ['', '#/fleet', '#/assignment-log']) {
    const { view } = launch('standalone', { hash });
    await heading('Library');
    expect(nav().queryByRole('button', { name: 'Fleet' })).toBeNull();
    expect(nav().queryByRole('button', { name: 'Assignment log' })).toBeNull();
    expect(nav().getByRole('button', { name: 'Assignment' })).toBeTruthy();
    view.unmount();
  }
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const { daemon } = launch('standalone');
  await heading('Library');
  const polls = daemon.sent('status').length;
  await act(() => vi.advanceTimersByTimeAsync(16000));
  expect(daemon.sent('status').length).toBeGreaterThan(polls);
  for (const cmd of ['fleet', 'log', 'log_facets']) expect(daemon.sent(cmd)).toEqual([]);
});

it('opens the screen named in the address, the Fleet otherwise, and moves focus to each screen it goes to', async () => {
  const draft = launch('fleet', { hash: '#/draft' });
  await heading('Draft');
  draft.view.unmount();
  const { user } = launch('fleet', { hash: '#/unknown' });
  await heading('Fleet');
  await user.click(nav().getByRole('button', { name: 'Library' }));
  await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Library', level: 1 })));
  expect(window.location.hash).toBe('#/library');
  expect(nav().getByRole('button', { name: 'Library' }).getAttribute('aria-current')).toBe('page');
});

it('jumps anywhere from the command palette, matching every word in any order, and gives focus back when dismissed', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/library' });
  await heading('Library');
  const trigger = screen.getByRole('button', { name: 'Open command palette' });
  for (const dismiss of [() => user.keyboard('{Escape}'), async () => user.click((await dialog('Go to…')).getByRole('button', { name: 'Close' }))]) {
    await user.click(trigger);
    const input = await screen.findByRole('combobox', { name: 'Search commands' });
    await waitFor(() => expect(document.activeElement).toBe(input));
    await dismiss();
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  }

  await user.keyboard('{Control>}k{/Control}');
  let input = await screen.findByRole('combobox', { name: 'Search commands' });
  await user.type(input, '  laptop   machine ');
  expect(screen.getAllByRole('option').map(option => option.textContent)).toEqual(['Machine: laptop']);
  expect(input.getAttribute('aria-activedescendant')).toBe('machine-laptop');
  await user.keyboard('{Enter}');
  await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Fleet', level: 1 })));

  // A query word nothing matches empties the list and says so.
  await user.click(trigger);
  input = await screen.findByRole('combobox', { name: 'Search commands' });
  await user.type(input, 'baseline missing');
  expect(screen.queryAllByRole('option')).toHaveLength(0);
  expect((await dialog('Go to…')).getByRole('status').textContent).toBe('No matching commands');
  expect(input.hasAttribute('aria-activedescendant')).toBe(false);
  // An action runs in place, so focus goes back to where it was.
  await user.clear(input);
  await user.type(input, 'baseline action');
  await user.click(screen.getByRole('option', { name: 'Action: Pull Baseline' }));
  expect(await screen.findByText('Baseline is up to date')).toBeTruthy();
  expect(daemon.sent('pull')).toEqual([{}]);
  await waitFor(() => expect(document.activeElement).toBe(trigger));

  // An action that opens a form opens it on its screen.
  await user.keyboard('{Control>}k{/Control}');
  await user.type(await screen.findByRole('combobox', { name: 'Search commands' }), 'add import{Enter}');
  expect(await screen.findByRole('dialog', { name: 'Add Import' })).toBeTruthy();
  await user.keyboard('{Escape}');

  // A Skill opens the Library searched for it.
  await user.keyboard('{Control>}k{/Control}');
  await user.type(await screen.findByRole('combobox', { name: 'Search commands' }), 'skill: pdf{Enter}');
  await heading('Library');
  expect((await screen.findByLabelText('Search by name') as HTMLInputElement).value).toBe('pdf');
});

it('refreshes what each Daemon notification touches and leaves the rest alone', async () => {
  const { daemon } = launch('fleet', { hash: '#/library' });
  await heading('Library');
  await screen.findByRole('table', { name: 'Skills' });
  const watched = ['skills', 'imports', 'fleet'];
  await waitFor(() => expect(watched.every(cmd => daemon.sent(cmd).length)).toBe(true));
  const refreshed = async (event: Parameters<typeof notify>[0], expected: string[], payload?: unknown) => {
    const before = watched.map(cmd => daemon.sent(cmd).length);
    await notify(event, payload);
    await waitFor(() => expect(watched.filter((cmd, i) => daemon.sent(cmd).length > before[i])).toEqual(expected));
  };
  // A Draft change can add an Import, so the Library and Imports reload; the Fleet doesn't.
  daemon.content.push({ name: 'triage', item: 'skill:triage', id: 'id-triage', type: 'personal', description: null, draft: 'added', in_baseline: false,
    path: `${daemon.status.repo}/personal/triage`, enabled_harnesses: [], clashes_with_repo: false, variants: [] });
  await refreshed('draft', ['skills', 'imports']);
  expect(await screen.findByRole('row', { name: 'View Personal triage' })).toBeTruthy();
  // Fleet changes never reload what is open in the Library.
  await refreshed('fleet', ['fleet']);
  await refreshed('imports', ['imports']);
  await refreshed('assignment', ['skills', 'fleet']);
  await refreshed('status', ['skills']);
  await refreshed('daemon-connection', ['skills', 'imports', 'fleet'], { reachable: true, message: null, retry_in_ms: 0 });
});
