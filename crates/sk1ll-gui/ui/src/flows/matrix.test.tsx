// Choosing what each Machine's Harnesses get in the Assignment matrix.
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import { checked, dialog, heading, isDisabled, launch, notify, toastButton } from './harness';

const cell = (item: string, column: string, state: 'explicit' | 'inherited') => screen.getByRole('checkbox', { name: `${item} in ${column}, ${state}` });
const expected = (machine: string, revision: number) => ({ revision, registration: `reg-${machine}` });

it('writes single toggles against the revision shown, and reloads instead when someone changed it first', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-matrix' });
  await heading('Assignment matrix');
  const review = await screen.findByRole('checkbox', { name: 'skill:code-review in workstation / claude, inherited' });
  expect(checked(review)).toBe(true);
  await user.click(review);
  await waitFor(() => expect(checked(cell('skill:code-review', 'workstation / claude', 'explicit'))).toBe(false));
  expect(daemon.sent('set_assignment').at(-1)).toEqual({ machine: 'workstation', changes: {
    changes: [{ harness: 'claude', kind: 'skill', item_id: 'id-code-review', name: 'code-review', enabled: false }], expected: expected('workstation', 42) } });

  // An explicit value that differs from the default says so; resetting it inherits the default again.
  const deploy = cell('skill:deploy', 'workstation / claude', 'explicit');
  expect(deploy.getAttribute('aria-description')).toBe('Overrides the Harness default.');
  await user.click(screen.getByRole('button', { name: 'Use default for skill:deploy in workstation / claude' }));
  await waitFor(() => expect(checked(cell('skill:deploy', 'workstation / claude', 'inherited'))).toBe(true));
  expect(cell('skill:deploy', 'workstation / claude', 'inherited').hasAttribute('aria-description')).toBe(false);
  expect(daemon.sent('set_assignment').at(-1)).toMatchObject({ changes: { changes: [{ item_id: 'id-deploy', enabled: null }], expected: expected('workstation', 43) } });
  await user.click(screen.getByRole('switch', { name: 'Default for new rules in workstation / codex' }));
  await waitFor(() => expect(daemon.sent('set_assignment').at(-1)).toMatchObject({ changes: { changes: [{ harness: 'codex', kind: 'new_rules_default', enabled: true }] } }));

  // Offline Machines stay editable; another Machine's Draft-only item can't be assigned there yet.
  expect(within(screen.getByRole('columnheader', { name: 'ci-runner, Offline' })).getByText('Offline')).toBeTruthy();
  expect(cell('skill:code-review', 'ci-runner / codex', 'inherited').getAttribute('aria-description')).toBe('Applies when ci-runner reconnects.');
  expect(screen.getByRole('group', { name: 'skill:release-notes in laptop / claude, unavailable' })).toBeTruthy();
  expect(isDisabled(cell('skill:release-notes', 'workstation / claude', 'inherited'))).toBe(false);
  // Content not yet in the Fleet's Baseline says so, even when the Baseline has an older version.
  const label = (item: string) => within(cell(item, 'workstation / claude', 'inherited').closest('tr')!).queryByText(/^(Unpushed|Draft only)$/)?.textContent;
  expect(label('skill:release-notes')).toBe('Unpushed');
  expect(label('rule:security')).toBeUndefined();

  // Someone turns off laptop's default for new Skills: the stale toggle is refused and the matrix shows theirs.
  daemon.assignments.laptop.harnesses.claude.new_skills_default = false;
  daemon.assignments.laptop.revision++;
  await user.click(cell('skill:code-review', 'laptop / claude', 'explicit'));
  expect(await screen.findByText('Assignment changed')).toBeTruthy();
  await waitFor(() => expect(checked(screen.getByRole('switch', { name: 'Default for new skills in laptop / claude' }))).toBe(false));
  expect(checked(cell('skill:code-review', 'laptop / claude', 'explicit'))).toBe(true);
  expect(daemon.assignments.laptop.harnesses.claude.skills['id-code-review']).toBe(true);
});

it('shows a reported Harness the Server has no entry for yet as all off, as the Server will assign it', async () => {
  const { user } = launch('fleet', { hash: '#/assignment-matrix', setup: daemon => { delete daemon.assignments['build-box'].harnesses.codex; } });
  await heading('Assignment matrix');
  const review = await screen.findByRole('checkbox', { name: 'skill:code-review in build-box / codex, inherited' });
  expect(checked(review)).toBe(false);
  expect(checked(cell('rule:security', 'build-box / codex', 'inherited'))).toBe(false);
  expect(checked(screen.getByRole('switch', { name: 'Default for new skills in build-box / codex' }))).toBe(false);
  expect(checked(screen.getByRole('switch', { name: 'Default for new rules in build-box / codex' }))).toBe(false);
  // Turning one item on there leaves the others off.
  await user.click(review);
  await waitFor(() => expect(checked(cell('skill:code-review', 'build-box / codex', 'explicit'))).toBe(true));
  expect(checked(cell('rule:security', 'build-box / codex', 'inherited'))).toBe(false);
});

it('bulk-changes rows and filtered columns once per Machine, and Undo restores what was accepted', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-matrix', setup: daemon => {
    daemon.override('set_assignment', args => args.machine === 'build-box' ? Promise.reject({ message: 'build-box rejected', status: 500 }) : undefined);
  } });
  await heading('Assignment matrix');
  await screen.findByRole('checkbox', { name: 'rule:security in workstation / codex, explicit' });
  await user.click(screen.getByRole('button', { name: 'Actions for rule:security' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
  expect(await screen.findByText('Enabled security in 3 Harnesses')).toBeTruthy();
  expect(screen.getByText('Applies when ci-runner reconnects.')).toBeTruthy();
  expect(await screen.findByText('build-box: build-box rejected')).toBeTruthy();
  expect(daemon.sent('set_assignment').map(args => args.machine).sort()).toEqual(['build-box', 'ci-runner', 'laptop', 'workstation']);
  expect(daemon.sent('set_assignment').find(args => args.machine === 'workstation')).toEqual({ machine: 'workstation', changes: { changes: [
    { harness: 'claude', kind: 'rule', item_id: 'id-security', name: 'security', enabled: true },
  ], expected: expected('workstation', 42) } });
  await waitFor(() => expect(checked(cell('rule:security', 'workstation / claude', 'explicit'))).toBe(true));

  await user.click(toastButton('Undo'));
  expect(await screen.findByText('Restored 3 Assignments')).toBeTruthy();
  await waitFor(() => expect(checked(cell('rule:security', 'workstation / claude', 'inherited'))).toBe(false));
  expect(checked(cell('rule:security', 'workstation / codex', 'explicit'))).toBe(true);
  expect(daemon.assignments.workstation.harnesses.claude.rules).not.toHaveProperty('id-security');
  daemon.restore('set_assignment');

  // Hidden Machines and collapsed columns are remembered; a column action covers only the items the filter shows.
  await user.click(screen.getByRole('button', { name: 'Machines (4)' }));
  await user.click(await screen.findByRole('menuitemcheckbox', { name: 'laptop' }));
  expect(localStorage.getItem('assignment-matrix-machines')).toBe('["workstation","ci-runner","build-box"]');
  await user.keyboard('{Escape}');
  expect(screen.queryByRole('checkbox', { name: /in laptop \// })).toBeNull();
  await user.click(screen.getByRole('button', { name: 'build-box Harnesses' }));
  expect(screen.getAllByText('Enabled in 0 of 2 Harnesses').length).toBeGreaterThan(0);
  expect(localStorage.getItem('assignment-matrix-collapsed')).toBe('["build-box"]');
  await user.type(screen.getByRole('textbox', { name: 'Search items' }), 'ships the server');
  await waitFor(() => expect(screen.queryByText('code-review')).toBeNull());
  const before = daemon.sent('set_assignment').length;
  await user.click(screen.getByRole('button', { name: 'Actions for workstation / claude' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Enable all visible items' }));
  expect(await screen.findByText('Enabled 1 item in workstation / claude')).toBeTruthy();
  expect(daemon.sent('set_assignment').slice(before)).toEqual([{ machine: 'workstation', changes: {
    changes: [{ harness: 'claude', kind: 'skill', item_id: 'id-deploy', name: 'deploy', enabled: true }], expected: expected('workstation', 44) } }]);
  await user.clear(screen.getByRole('textbox', { name: 'Search items' }));
  await user.selectOptions(screen.getByRole('combobox', { name: 'Item kind' }), 'rule');
  expect(screen.queryByText('deploy')).toBeNull();
  expect(screen.getByText('House code style rules shared by every Harness.')).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Machines (3)' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Only this Machine' }));
  await waitFor(() => expect(screen.queryByRole('checkbox', { name: /in ci-runner \// })).toBeNull());
  expect(localStorage.getItem('assignment-matrix-machines')).toBe('["workstation"]');
});

it('drops inactive Harnesses from editable columns and copy options while keeping their saved Assignments', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-matrix', setup: daemon => {
    daemon.status.report.harnesses.push({ name: 'myagent', kind: null });
    daemon.assignments.workstation.harnesses.myagent = structuredClone(daemon.assignments.workstation.harnesses.codex);
  } });
  await screen.findByRole('button', { name: 'Actions for workstation / myagent' });
  await screen.findByRole('button', { name: 'Actions for build-box / codex' });

  daemon.machines[0].report = { ...daemon.status.report }; // The server still has the old local report.
  daemon.status.report.harnesses = [{ name: 'claude', kind: 'claude' }, { name: 'codex', kind: 'codex' }];
  daemon.machines.find(machine => machine.id === 'build-box')!.report!.harnesses = [{ name: 'claude', kind: 'claude' }];
  await notify('assignment');
  await waitFor(() => {
    expect(screen.queryByRole('button', { name: 'Actions for workstation / myagent' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Actions for build-box / codex' })).toBeNull();
  });
  expect(daemon.assignments.workstation.harnesses).toHaveProperty('myagent');
  expect(daemon.assignments['build-box'].harnesses).toHaveProperty('codex');

  await user.click(screen.getByRole('button', { name: 'Actions for workstation' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Copy Assignment from…' }));
  const copy = await dialog('Copy Assignment from…');
  await user.selectOptions(copy.getByLabelText('Source Machine'), 'build-box');
  expect(copy.queryByLabelText('Source Harness for myagent')).toBeNull();
  expect(within(copy.getByLabelText('Source Harness for codex')).queryByRole('option', { name: 'codex' })).toBeNull();
});

it('copies another Machine\'s Assignment after previewing it, and previews again when the target changed', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-matrix' });
  await heading('Assignment matrix');
  await user.click(await screen.findByRole('button', { name: 'Actions for build-box' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Copy Assignment from…' }));
  const copy = await dialog('Copy Assignment from…');
  await user.selectOptions(copy.getByLabelText('Source Machine'), 'laptop');
  await user.selectOptions(copy.getByLabelText('Mode'), 'merge');
  // laptop has no codex: only claude maps by name.
  expect((copy.getByLabelText('Source Harness for codex') as HTMLSelectElement).value).toBe('');
  await user.click(copy.getByRole('button', { name: 'Preview changes' }));
  expect(await copy.findByText('1 change to item toggles and defaults')).toBeTruthy();
  expect(daemon.sent('copy_machine_assignment')).toEqual([]);
  // Someone else changes build-box before the copy is confirmed.
  daemon.assignments['build-box'].revision++;
  await user.click(copy.getByRole('button', { name: 'Copy Assignment' }));
  expect(await copy.findByText('Assignment changed since the preview')).toBeTruthy();
  await user.click(copy.getByRole('button', { name: 'Preview changes' }));
  await copy.findByText('1 change to item toggles and defaults');
  await user.click(copy.getByRole('button', { name: 'Copy Assignment' }));
  expect(await screen.findByText('Copied Assignment to build-box')).toBeTruthy();
  const body = { source: 'laptop', mode: 'merge', harnesses: [{ from: 'claude', to: 'claude' }] };
  expect(daemon.sent('preview_assignment_copy')).toEqual([
    { machine: 'build-box', copy: { ...body, expected: expected('build-box', 5) } },
    { machine: 'build-box', copy: { ...body, expected: expected('build-box', 6) } },
  ]);
  expect(daemon.sent('copy_machine_assignment').at(-1)).toEqual({ machine: 'build-box', copy: { ...body,
    expected: expected('build-box', 6), source_expected: expected('laptop', 17) } });
  await waitFor(() => expect(checked(cell('skill:code-review', 'build-box / claude', 'explicit'))).toBe(true));
  await user.click(toastButton('View log'));
  await heading('Assignment log');
});

it('clears orphan toggles across the Fleet, or only its own on a Self-only Machine', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-matrix' });
  await heading('Assignment matrix');
  expect(await screen.findByText('1 toggle for items that no longer exist')).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Clear' }));
  const confirm = await dialog('Clear orphan toggles?');
  const orphans = within(confirm.getByRole('table', { name: 'Orphan toggles' }));
  expect(orphans.getByText('id-old-skill')).toBeTruthy();
  expect(orphans.getByText('workstation')).toBeTruthy();
  await user.click(confirm.getByRole('button', { name: 'Clear' }));
  await waitFor(() => expect(screen.queryByText('1 toggle for items that no longer exist')).toBeNull());
  expect(daemon.assignments.workstation.harnesses.claude.skills).not.toHaveProperty('id-old-skill');
});

it('lets a Self-only Machine clear only its own orphan toggles', async () => {
  const { daemon, user } = launch('self', { hash: '#/assignment-matrix' });
  await heading('Assignment matrix');
  expect(await screen.findByText('2 toggles for items that no longer exist')).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Clear' }));
  const confirm = await dialog('Clear orphan toggles?');
  expect(confirm.getByText('Clear these orphan toggles for this Machine.')).toBeTruthy();
  expect(confirm.queryByText('laptop')).toBeNull();
  await user.click(confirm.getByRole('button', { name: 'Clear' }));
  expect(await screen.findByText('1 toggle for items that no longer exist')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Clear' })).toBeNull();
  expect(daemon.assignments.laptop.harnesses.claude.skills).toHaveProperty('id-laptop-orphan');
});

it('keeps the other Machines usable while one Assignment fails to load, retrying only that column, and rolls back a refused toggle', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/assignment-matrix', setup: daemon => {
    daemon.override('assignment', args => args.machine === 'laptop' ? Promise.reject({ message: 'Machine not found' }) : undefined);
  } });
  await heading('Assignment matrix');
  expect(await screen.findByText('Assignment not loaded: Machine not found')).toBeTruthy();
  expect(screen.queryByRole('checkbox', { name: /in laptop \// })).toBeNull();
  // Bulk changes skip it.
  await user.click(screen.getByRole('button', { name: 'Actions for rule:security' }));
  await user.click(await screen.findByRole('menuitem', { name: 'Enable in all visible Harnesses' }));
  await screen.findByText(/^Enabled security in/);
  expect(daemon.sent('set_assignment').map(args => args.machine)).not.toContain('laptop');
  // A retry that fails again keeps the error; one that works shows the column.
  daemon.override('assignment', args => args.machine === 'laptop' ? Promise.reject({ message: 'Server unreachable' }) : undefined);
  await user.click(screen.getByRole('button', { name: 'Retry loading the Assignment of laptop' }));
  expect(await screen.findByText('Assignment not loaded: Server unreachable')).toBeTruthy();
  daemon.restore('assignment');
  await user.click(screen.getByRole('button', { name: 'Retry loading the Assignment of laptop' }));
  expect(await screen.findByRole('checkbox', { name: 'skill:code-review in laptop / claude, explicit' })).toBeTruthy();
  expect(screen.queryByText(/Assignment not loaded/)).toBeNull();

  daemon.override('set_assignment', () => Promise.reject({ message: 'Server refused' }));
  await user.click(cell('skill:code-review', 'laptop / claude', 'explicit'));
  expect(await screen.findByText('laptop: Server refused')).toBeTruthy();
  await waitFor(() => expect(checked(cell('skill:code-review', 'laptop / claude', 'explicit'))).toBe(true));
  daemon.restore('set_assignment');
  await user.click(cell('skill:code-review', 'laptop / claude', 'explicit'));
  await waitFor(() => expect(checked(cell('skill:code-review', 'laptop / claude', 'explicit'))).toBe(false));
});

it('assigns this Machine\'s Draft-only items in standalone, without asking for the Fleet', async () => {
  const { daemon, user } = launch('standalone', { hash: '#/assignment-matrix' });
  await heading('Assignment');
  const notes = await screen.findByRole('checkbox', { name: 'skill:release-notes in workstation / claude, inherited' });
  expect(isDisabled(notes)).toBe(false);
  await user.click(notes);
  await waitFor(() => expect(checked(cell('skill:release-notes', 'workstation / claude', 'explicit'))).toBe(false));
  expect(daemon.sent('set_assignment').at(-1)).toMatchObject({ machine: 'workstation' });
  expect(daemon.sent('fleet')).toEqual([]);
});

it('marks each reported Harness with its tool, and shows a kind it doesn\'t know as written', async () => {
  // Kinds come from other Machines' reports: one named like an object's own property is still just a name.
  launch('fleet', { hash: '#/assignment-matrix', setup: daemon => daemon.machines.find(m => m.id === 'laptop')!.report!.harnesses.push(
    { name: 'claude_2', kind: 'claude' }, { name: 'odd', kind: 'constructor' }, { name: 'proto', kind: '__proto__' }) });
  await heading('Assignment matrix');
  expect(within(await screen.findByRole('columnheader', { name: /^claude_2/ })).getByText('Claude Code')).toBeTruthy();
  expect(within(screen.getByRole('columnheader', { name: /^odd/ })).getByText('constructor')).toBeTruthy();
  expect(within(screen.getByRole('columnheader', { name: /^proto/ })).getByText('__proto__')).toBeTruthy();
});
