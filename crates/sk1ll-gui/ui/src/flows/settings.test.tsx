// Settings: this Machine's details and diagnostics, its editor, rejoining a Fleet, and its Harnesses in machine.toml.
import { expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import { checked, dialog, heading, isDisabled, launch } from './harness';
import type { HarnessChoice, HarnessPreview, PlacementView } from '../types';

// A Harness added to machine.toml by hand.
const custom = (): HarnessChoice => ({ name: 'agents', installed: true, enabled: true, preset: false, configured: true,
  harness: { skills: '~/.agents/skills', instructions: null, instructions_format: 'block', mode: 'copy' } });
const row = (name: string) => within(screen.getByRole('cell', { name }).closest('tr')!);

it('shows this Machine\'s details in order, copies diagnostics as text, and saves the preferred editor', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings' });
  await heading('Settings');
  await screen.findByLabelText('Preferred editor');
  expect(screen.getAllByRole('heading', { level: 2 }).map(node => node.textContent)).toEqual(['This Machine', 'Fleet membership', 'Editor', 'App updates', 'Diagnostics']);
  // Inactive diagnostics stay hidden.
  expect(screen.queryByText('Auth rejected since')).toBeNull();
  expect(screen.getByText('/run/user/1000/sk1ll/daemon.sock')).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Copy diagnostics' }));
  await waitFor(async () => expect(await navigator.clipboard.readText()).toContain('Socket: /run/user/1000/sk1ll/daemon.sock\nAssignment revision: 42'));

  await user.selectOptions(screen.getByLabelText('Preferred editor'), 'nvim');
  await waitFor(() => expect(daemon.sent('set_preferred_editor')).toEqual([{ editorId: 'nvim' }]));

  // Paths stay readable as code, modes and origins in words.
  const codex = row('codex');
  expect(codex.getByText('~/.codex/skills').tagName).toBe('CODE');
  expect(codex.getByText('Copy')).toBeTruthy();
  expect(codex.getByText('Preset, adjusted')).toBeTruthy();
  expect(row('claude').getByText('Symbolic link')).toBeTruthy();
  expect(screen.getByText('/home/dev/.config/sk1ll/machine.toml').tagName).toBe('CODE');
  await user.click(screen.getByRole('button', { name: 'Open configuration' }));
  await waitFor(() => expect(daemon.sent('open_machine_config')).toEqual([{ editorId: 'nvim' }]));
});

it('says when no editor was found and looks again on request', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings', setup: daemon => daemon.override('editors', () => ({ preferred_editor: '', editors: [] })) });
  expect(await screen.findByText('No editors found')).toBeTruthy();
  expect(screen.queryByLabelText('Preferred editor')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Refresh editors' }));
  await waitFor(() => expect(daemon.sent('editors').at(-1)).toEqual({ refresh: true }));
});

it('keeps an active auth rejection in the diagnostics, with a timestamp reachable by keyboard', async () => {
  launch('auth', { hash: '#/settings' });
  await heading('Settings');
  expect(await screen.findByText('Auth rejected since')).toBeTruthy();
  expect(screen.getAllByText('Server returned 401 Unauthorized').length).toBeGreaterThan(0);
  expect(document.querySelector('time')?.getAttribute('tabindex')).toBe('0');
});

it('tucks Rejoin away while joined, and confirms the effective Machine id and copied Assignment before rejoining', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings' });
  await heading('Settings');
  expect(screen.queryByLabelText('Server URL')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Rejoin this Machine…' }));
  expect(await screen.findByLabelText('Server URL')).toBeTruthy();

  const id = screen.getByLabelText('Machine id') as HTMLInputElement;
  expect(id.value).toBe('');
  expect(id.placeholder).toBe('workstation');
  const code = screen.getByLabelText('Join code') as HTMLInputElement;
  expect(code.type).toBe('password');
  await user.click(screen.getByRole('button', { name: 'Show join code' }));
  expect(code.type).toBe('text');
  await user.click(screen.getByRole('button', { name: 'Hide join code' }));
  expect(code.type).toBe('password');
  await user.type(code, 'secret');
  await screen.findByRole('option', { name: 'laptop' });
  await user.selectOptions(screen.getByLabelText('Copy Assignment from Machine'), 'laptop');
  await user.click(screen.getByRole('button', { name: 'Join / rejoin' }));
  const confirm = await dialog('Join this Machine to the Server?');
  const summary = screen.getByRole('dialog', { name: 'Join this Machine to the Server?' }).textContent;
  expect(summary).toContain('workstation');
  expect(summary).toContain('Copied from laptop');
  await user.click(confirm.getByRole('button', { name: 'Join / rejoin' }));
  await waitFor(() => expect(daemon.sent('join')).toEqual([{ request: { server_url: 'https://skills.example.com', code: 'secret', admin_token: undefined, machine_id: undefined, copy_from: 'laptop' } }]));
});

it('opens Rejoin for a revoked Machine, joining with the admin token and typing the copied Machine when the Fleet can\'t load', async () => {
  const { daemon, user } = launch('revoked', { hash: '#/settings', setup: daemon => daemon.override('fleet', () => Promise.reject({ message: 'Unavailable' })) });
  await heading('Settings');
  await user.selectOptions(await screen.findByLabelText('Join with'), 'admin');
  await waitFor(() => expect(screen.getByLabelText('Copy Assignment from Machine').tagName).toBe('INPUT'));
  const url = screen.getByLabelText('Server URL');
  await user.clear(url); await user.type(url, 'https://new.example.com');
  expect(screen.getByRole('button', { name: 'Show admin token' })).toBeTruthy();
  await user.type(screen.getByLabelText('Admin token'), 'adm{Enter}');
  const confirm = await dialog('Join this Machine to the Server?');
  expect(confirm.getByText(/Admin token/)).toBeTruthy();
  await user.click(confirm.getByRole('button', { name: 'Join / rejoin' }));
  await waitFor(() => expect(daemon.sent('join')).toEqual([{ request: { server_url: 'https://new.example.com', code: '', admin_token: 'adm', machine_id: undefined, copy_from: undefined } }]));
});

it('labels each Harness\'s origin, tucks away presets not on this Machine, and validates a new Harness before previewing it', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings', setup: daemon => { daemon.setupHarnesses.push(custom()); } });
  await heading('Settings');
  await screen.findByRole('table', { name: 'Harnesses on this Machine' });
  expect(['claude', 'codex', 'kiro', 'agents'].map(name => row(name).getAllByText(/Detected|Preset|preset|Custom/)[0].textContent))
    .toEqual(['Detected', 'Preset, adjusted', 'Opt-in preset', 'Custom']);
  expect(row('kiro').getByText('Available: off until you enable it.')).toBeTruthy();
  expect(screen.queryByRole('cell', { name: 'gemini' })).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Show 1 more preset' }));
  expect(row('gemini').getByText('Not detected')).toBeTruthy();
  // Presets reset to their built-in paths; only a custom Harness can be removed.
  expect(screen.getByRole('button', { name: 'Reset codex' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Remove codex' })).toBeNull();
  expect(screen.getByRole('button', { name: 'Remove agents' })).toBeTruthy();

  await user.click(screen.getByRole('button', { name: 'Enable kiro' }));
  expect(await screen.findByRole('dialog', { name: 'Turn on kiro?' })).toBeTruthy();
  expect(daemon.sent('preview_harness').at(-1)).toEqual({ request: { change: { op: 'enable', name: 'kiro' } } });
  await user.keyboard('{Escape}');

  await user.click(screen.getByRole('button', { name: 'Add Harness' }));
  const form = await dialog('Add a Harness');
  await user.type(form.getByLabelText('Name'), 'Tool');
  await user.type(form.getByLabelText('Skills directory'), 'relative');
  await user.click(form.getByRole('button', { name: 'Preview change' }));
  expect(form.getByText(/lowercase letters/)).toBeTruthy();
  await user.clear(form.getByLabelText('Name')); await user.type(form.getByLabelText('Name'), 'tool');
  await user.click(form.getByRole('button', { name: 'Preview change' }));
  expect(form.getByText(/must be absolute/)).toBeTruthy();
  await user.clear(form.getByLabelText('Skills directory')); await user.type(form.getByLabelText('Skills directory'), '/opt/tool/skills');
  await user.selectOptions(form.getByLabelText('Place skills as'), 'copy');
  await user.click(form.getByRole('button', { name: 'Preview change' }));
  const save = await dialog('Save tool?');
  expect(await save.findByText('/opt/tool/skills/code-review')).toBeTruthy();
  expect(daemon.sent('preview_harness').at(-1)).toEqual({ request: { change: { op: 'set', name: 'tool',
    harness: { skills: '/opt/tool/skills', instructions: null, instructions_format: 'block', mode: 'copy' } } } });
  await user.click(save.getByRole('button', { name: 'Apply' }));
  expect(await screen.findByRole('cell', { name: 'tool' })).toBeTruthy();
});

it('turns a Harness off after a preview, taking out what sk1ll placed or leaving it, and retries when Take out fails', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings' });
  await heading('Settings');
  await user.click(await screen.findByRole('checkbox', { name: 'Sync into claude' }));
  let change = await dialog('Turn off claude?');
  expect(await change.findByText('~/.claude/skills/code-review')).toBeTruthy();
  expect(change.getByText(/Take out turns claude's items off in the Assignment, so sync removes what it placed/)).toBeTruthy();
  expect(daemon.sent('preview_harness')).toEqual([{ request: { change: { op: 'disable', name: 'claude' } } }]);

  // Take out fails partway: machine.toml stays as it was, the Harness stays on, and Take out can run again.
  const left: PlacementView = { harness: 'claude', kind: 'remove', item: 'skill', name: 'code-review', dest: '~/.claude/skills/code-review', reason: null };
  daemon.override('take_out_harness', () => ({ applied: false, taken_out: [], left: [left], harnesses: daemon.setupHarnesses, errors: ['remove ~/.claude/skills/code-review: permission denied'] }));
  await user.click(change.getByRole('button', { name: 'Take out and turn off' }));
  expect(await change.findByText(/machine.toml was not changed: 1 placed item is still there/)).toBeTruthy();
  expect(change.getByText(/claude's items stay off in the Assignment/)).toBeTruthy();
  expect(change.getByText('remove ~/.claude/skills/code-review: permission denied')).toBeTruthy();
  daemon.restore('take_out_harness');
  await waitFor(() => expect(isDisabled(change.getByRole('button', { name: 'Take out and turn off' }))).toBe(false));
  await user.click(change.getByRole('button', { name: 'Take out and turn off' }));
  await waitFor(() => expect(screen.queryByRole('dialog', { name: 'Turn off claude?' })).toBeNull());
  expect(await screen.findByText('Took out 1 placed item.')).toBeTruthy();
  expect(daemon.sent('apply_harness')).toEqual([]);
  await waitFor(() => expect(checked(screen.getByRole('checkbox', { name: 'Sync into claude' }))).toBe(false));

  // Turning codex off can leave its placements where they are, unmanaged; a hand-edited one is called out.
  daemon.override('preview_harness', () => ({ ...preview(daemon.setupHarnesses), placed: [
    { harness: 'codex', kind: 'remove', item: 'skill', name: 'pdf', dest: '~/.codex/skills/pdf', reason: null },
    { harness: 'codex', kind: 'conflict', item: 'skill', name: 'notes', dest: '~/.codex/skills/notes', reason: 'edited' },
  ] }));
  await user.click(screen.getByRole('checkbox', { name: 'Sync into codex' }));
  change = await dialog('Turn off codex?');
  expect(await change.findByText('Edited by hand')).toBeTruthy();
  expect(change.getByText(/1 edited by hand stays either way/)).toBeTruthy();
  expect(change.queryByRole('checkbox')).toBeNull();
  await user.click(change.getByRole('button', { name: 'Leave in place and turn off' }));
  await waitFor(() => expect(screen.queryByRole('dialog', { name: 'Turn off codex?' })).toBeNull());
  expect(daemon.sent('apply_harness').map(args => (args.request as { change: unknown }).change)).toEqual([{ op: 'disable', name: 'codex' }]);
});

// A preview of turning codex off, fingerprinted as the mock Daemon does.
const preview = (harnesses: HarnessChoice[]): HarnessPreview => ({ fingerprint: JSON.stringify(harnesses), path: '/home/dev/.config/sk1ll/machine.toml', name: 'codex',
  before: harnesses.find(h => h.name === 'codex')!.harness, after: null, placed: [], sync: [], awaiting_assignment: false });

it('asks for a fresh preview when machine.toml changed meanwhile, lists what failed after applying, and refuses an invalid change', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings', setup: daemon => { daemon.setupHarnesses.push(custom()); } });
  await heading('Settings');
  await user.click(await screen.findByRole('button', { name: 'Remove agents' }));
  const change = await dialog('Remove agents from machine.toml?');
  expect(await change.findByRole('button', { name: 'Take out and remove' })).toBeTruthy();
  expect(change.getByRole('button', { name: 'Leave in place and remove' })).toBeTruthy();
  // Someone edits machine.toml while the preview is open.
  daemon.setupHarnesses.find(h => h.name === 'kiro')!.configured = true;
  await user.click(change.getByRole('button', { name: 'Take out and remove' }));
  expect(await change.findByText(/preview again/)).toBeTruthy();
  // The stale preview is gone: nothing can be applied until it is taken again.
  expect(isDisabled(change.getByRole('button', { name: 'Apply' }))).toBe(true);
  await user.click(change.getByRole('button', { name: 'Preview again' }));
  await waitFor(() => expect(daemon.sent('preview_harness')).toHaveLength(2));

  daemon.override('apply_harness', () => ({ harnesses: daemon.setupHarnesses, errors: ['sync ~/.agents/skills: permission denied'] }));
  const leave = await change.findByRole('button', { name: 'Leave in place and remove' });
  await waitFor(() => expect(isDisabled(leave)).toBe(false));
  await user.click(leave);
  expect(await change.findByText('sync ~/.agents/skills: permission denied')).toBeTruthy();
  await user.click(change.getByRole('button', { name: 'Done' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());

  // A change the Daemon refuses says why and can't be applied; with nothing placed there is only Apply.
  daemon.override('preview_harness', () => Promise.reject({ message: '`kiro` would sync into ~/.codex/skills, which `codex` already uses' }));
  await user.click(screen.getByRole('checkbox', { name: 'Sync into kiro' }));
  const refused = await dialog('Turn on kiro?');
  expect(await refused.findByText(/already uses/)).toBeTruthy();
  expect(isDisabled(refused.getByRole('button', { name: 'Apply' }))).toBe(true);
  await user.click(refused.getByRole('button', { name: 'Cancel' }));
  daemon.override('preview_harness', () => preview(daemon.setupHarnesses));
  await user.click(screen.getByRole('checkbox', { name: 'Sync into codex' }));
  const nothing = await dialog('Turn off codex?');
  expect(await nothing.findByText('Nothing that sk1ll placed is left behind by this change.')).toBeTruthy();
  expect(nothing.queryByRole('button', { name: 'Take out and turn off' })).toBeNull();
  expect(nothing.getByRole('button', { name: 'Apply' })).toBeTruthy();
});

it('still opens machine.toml when it can\'t be parsed', async () => {
  const { daemon, user } = launch('fleet', { hash: '#/settings', setup: daemon => daemon.override('harness_settings', () => Promise.reject({ message: 'Invalid TOML' })) });
  expect(await screen.findByText(/Invalid TOML/)).toBeTruthy();
  await user.click(screen.getByRole('button', { name: 'Open configuration' }));
  await waitFor(() => expect(daemon.sent('open_machine_config')).toEqual([{ editorId: 'code' }]));
});
