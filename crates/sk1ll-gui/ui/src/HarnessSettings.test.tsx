// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { HarnessPanel, harnessOrigin } from './HarnessSettings';
import { api } from './api';
import { toast } from './toast';
import type { HarnessChoice, HarnessPreview } from './types';
vi.mock('./api', () => ({ api: { harnessSettings: vi.fn(), previewHarness: vi.fn(), applyHarness: vi.fn(), takeOutHarness: vi.fn(), openMachineConfig: vi.fn(),
  setHarnessAssignment: vi.fn() }, errorMessage: String, errorStatus: (e: { status?: number }) => e?.status }));
vi.mock('./toast', () => ({ toast: vi.fn() }));
afterEach(cleanup);
const paths = (skills: string | null): HarnessChoice['harness'] => ({ skills, instructions: null, instructions_format: 'block', mode: 'copy' });
const harnesses: HarnessChoice[] = [
  { name: 'codex', installed: true, enabled: true, preset: true, configured: false, harness: paths('~/.codex/skills') },
  { name: 'agents', installed: true, enabled: true, preset: false, configured: true, harness: paths('~/.agents/skills') },
  { name: 'kiro', installed: false, enabled: false, preset: true, configured: false, harness: paths('~/.kiro/skills') },
  { name: 'cursor', installed: true, enabled: false, preset: true, configured: false, available: true, harness: paths('~/.cursor/skills') },
  { name: 'gemini', installed: true, enabled: true, preset: true, configured: false, available: true, harness: paths(null) },
];
const preview = (over: Partial<HarnessPreview> = {}): HarnessPreview => ({ fingerprint: "preview", path: '/cfg/machine.toml', name: 'codex', before: paths('~/.codex/skills'), after: null,
  placed: [{ harness: 'codex', kind: 'remove', item: 'skill', name: 'pdf', dest: '/h/.codex/skills/pdf', reason: null },
    { harness: 'codex', kind: 'conflict', item: 'skill', name: 'notes', dest: '/h/.codex/skills/notes', reason: 'edited' }],
  sync: [], awaiting_assignment: false, ...over });
beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.harnessSettings).mockResolvedValue({ path: '/cfg/machine.toml', harnesses });
  vi.mocked(api.previewHarness).mockResolvedValue(preview());
  vi.mocked(api.applyHarness).mockResolvedValue({ harnesses, errors: [] });
  vi.mocked(api.takeOutHarness).mockResolvedValue({ applied: true, taken_out: [preview().placed[0]], harnesses, errors: [] });
  vi.mocked(api.setHarnessAssignment).mockResolvedValue({
    assignment: { harnesses: {} }, revision: 1, entries: [],
  });
});
const mount = (changed = vi.fn()) => { render(<HarnessPanel revision={0} editorId="code" changed={changed} />); return changed; };

it('labels origins and hides presets that are not on this Machine', async () => {
  mount();
  expect(await screen.findByText('agents')).toBeTruthy();
  expect(screen.queryByText('kiro')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Show 1 more preset' }));
  expect(screen.getByText('kiro')).toBeTruthy();
  expect(harnesses.map(harnessOrigin)).toEqual(['Detected', 'Custom', 'Not detected', 'Opt-in preset', 'Detected']);
  expect(screen.queryByRole('button', { name: 'Remove codex' })).toBeNull();
  expect(screen.getByRole('button', { name: 'Remove agents' })).toBeTruthy();
});

it('previews a disable and can leave what sk1ll placed in place', async () => {
  const changed = mount();
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into codex' }));
  const dialog = await screen.findByRole('dialog');
  expect(api.previewHarness).toHaveBeenCalledWith({ change: { op: 'disable', name: 'codex' } });
  expect(await within(dialog).findByText('/h/.codex/skills/pdf')).toBeTruthy();
  expect(within(dialog).getByText('Edited by hand')).toBeTruthy();
  expect(within(dialog).queryByRole('checkbox')).toBeNull();
  expect(within(dialog).queryByRole('button', { name: "Turn off this Harness's items first" })).toBeNull();
  fireEvent.click(within(dialog).getByRole('button', { name: 'Leave in place and turn off' }));
  await waitFor(() => expect(api.applyHarness).toHaveBeenCalledWith({ fingerprint: "preview", change: { op: 'disable', name: 'codex' } }));
  await waitFor(() => expect(changed).toHaveBeenCalled());
});

it('takes out what sk1ll placed and turns the Harness off in one step', async () => {
  const changed = mount();
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into codex' }));
  const dialog = await screen.findByRole('dialog');
  await within(dialog).findByText('/h/.codex/skills/pdf');
  expect(within(dialog).getByText(/Take out turns codex's items off in the Assignment, so sync removes what it placed/)).toBeTruthy();
  expect(within(dialog).getByText(/1 edited by hand stays either way/)).toBeTruthy();
  fireEvent.click(within(dialog).getByRole('button', { name: 'Take out and turn off' }));
  await waitFor(() => expect(api.takeOutHarness).toHaveBeenCalledWith({ fingerprint: 'preview', change: { op: 'disable', name: 'codex' } }));
  await waitFor(() => expect(changed).toHaveBeenCalled());
  expect(toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Turned off codex', description: 'Took out 1 placed item.' }));
  expect(api.applyHarness).not.toHaveBeenCalled();
  expect(api.setHarnessAssignment).not.toHaveBeenCalled();
});

it('leaves the Harness on when placements fail to come out, and retries', async () => {
  const left = preview().placed[0];
  vi.mocked(api.takeOutHarness).mockResolvedValueOnce({ applied: false, taken_out: [], left: [left], harnesses, errors: ['remove /h/.codex/skills/pdf: permission denied'] });
  const changed = mount();
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into codex' }));
  const dialog = await screen.findByRole('dialog');
  await within(dialog).findByText('/h/.codex/skills/pdf');
  const button = within(dialog).getByRole('button', { name: 'Take out and turn off' });
  fireEvent.click(button);
  expect(await within(dialog).findByText(/machine.toml was not changed: 1 placed item is still there/)).toBeTruthy();
  expect(within(dialog).getByText(/codex's items stay off in the Assignment/)).toBeTruthy();
  expect(within(dialog).getByText('remove /h/.codex/skills/pdf: permission denied')).toBeTruthy();
  expect(changed).not.toHaveBeenCalled();
  await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(button);
  await waitFor(() => expect(changed).toHaveBeenCalled());
  expect(api.takeOutHarness).toHaveBeenCalledTimes(2);
});

it('says what failed when nothing is left but the scan or machine.toml stopped Take out', async () => {
  vi.mocked(api.takeOutHarness).mockResolvedValueOnce({ applied: false, taken_out: [], harnesses, errors: ['Could not list old placements for codex: bad index'] });
  mount();
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into codex' }));
  const dialog = await screen.findByRole('dialog');
  await within(dialog).findByText('/h/.codex/skills/pdf');
  fireEvent.click(within(dialog).getByRole('button', { name: 'Take out and turn off' }));
  expect(await within(dialog).findByText(/^machine.toml was not changed\. codex's items stay off/)).toBeTruthy();
  expect(within(dialog).getByText('Could not list old placements for codex: bad index')).toBeTruthy();
});

it('offers Take out and remove for a removal, and only Apply when nothing is placed', async () => {
  vi.mocked(api.previewHarness).mockResolvedValue(preview({ name: 'agents' }));
  mount();
  fireEvent.click(await screen.findByRole('button', { name: 'Remove agents' }));
  let dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByRole('button', { name: 'Take out and remove' })).toBeTruthy();
  expect(within(dialog).getByRole('button', { name: 'Leave in place and remove' })).toBeTruthy();
  fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  vi.mocked(api.previewHarness).mockResolvedValue(preview({ placed: [] }));
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into codex' }));
  dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByText('Nothing that sk1ll placed is left behind by this change.')).toBeTruthy();
  expect(within(dialog).getByRole('button', { name: 'Apply' })).toBeTruthy();
  expect(within(dialog).queryByRole('button', { name: 'Take out and turn off' })).toBeNull();
});

it('validates a new Harness before previewing it', async () => {
  vi.mocked(api.previewHarness).mockResolvedValue(preview({ name: 'tool', before: null, after: paths('/opt/tool/skills'), placed: [],
    sync: [{ harness: 'tool', kind: 'create', item: 'skill', name: 'pdf', dest: '/opt/tool/skills/pdf', reason: null }] }));
  mount();
  fireEvent.click(await screen.findByRole('button', { name: 'Add Harness' }));
  fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Tool' } });
  fireEvent.change(screen.getByLabelText('Skills directory'), { target: { value: 'relative' } });
  fireEvent.click(screen.getByRole('button', { name: 'Preview change' }));
  expect(screen.getByText(/lowercase letters/)).toBeTruthy();
  fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'tool' } });
  fireEvent.click(screen.getByRole('button', { name: 'Preview change' }));
  expect(screen.getByText(/must be absolute/)).toBeTruthy();
  fireEvent.change(screen.getByLabelText('Skills directory'), { target: { value: '/opt/tool/skills' } });
  fireEvent.change(screen.getByLabelText('Place skills as'), { target: { value: 'copy' } });
  fireEvent.click(screen.getByRole('button', { name: 'Preview change' }));
  const dialog = await screen.findByRole('dialog', { name: 'Save tool?' });
  expect(await within(dialog).findByText('/opt/tool/skills/pdf')).toBeTruthy();
  expect(api.previewHarness).toHaveBeenCalledWith({ change: { op: 'set', name: 'tool', harness: { skills: '/opt/tool/skills', instructions: null, instructions_format: 'block', mode: 'copy' } } });
});

it('shows why a change is refused and does not apply it', async () => {
  vi.mocked(api.previewHarness).mockRejectedValue(new Error('`agents` would sync into /x, which `codex` already uses'));
  mount();
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into agents' }));
  const dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByText(/already uses/)).toBeTruthy();
  expect((within(dialog).getByRole('button', { name: 'Apply' }) as HTMLButtonElement).disabled).toBe(true);
});

it('offers opt-in presets as available with Enable, and gemini its skills directory', async () => {
  mount();
  const cursor = (await screen.findByRole('checkbox', { name: 'Sync into cursor' })) as HTMLButtonElement;
  expect(cursor.getAttribute('aria-checked')).toBe('false');
  expect(screen.getByText('Available: off until you enable it.')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Enable cursor' }));
  await screen.findByRole('dialog');
  expect(api.previewHarness).toHaveBeenCalledWith({ change: { op: 'enable', name: 'cursor' } });
  cleanup();
  mount();
  fireEvent.click(await screen.findByRole('button', { name: "Sync gemini's skills directory too" }));
  await screen.findByRole('dialog');
  expect(api.previewHarness).toHaveBeenLastCalledWith({ change: { op: 'enable', name: 'gemini' } });
});

it('asks for a fresh preview when machine.toml changed meanwhile, and lists what failed after applying', async () => {
  vi.mocked(api.applyHarness).mockRejectedValueOnce(Object.assign(new Error('machine.toml changed while sk1ll was changing it; nothing was written'), { status: 409 }));
  const changed = mount();
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into codex' }));
  const dialog = await screen.findByRole('dialog');
  await within(dialog).findByText('/h/.codex/skills/pdf');
  fireEvent.click(within(dialog).getByRole('button', { name: 'Leave in place and turn off' }));
  expect(await within(dialog).findByText(/nothing was written/)).toBeTruthy();
  fireEvent.click(within(dialog).getByRole('button', { name: 'Preview again' }));
  await waitFor(() => expect(api.previewHarness).toHaveBeenCalledTimes(2));
  vi.mocked(api.applyHarness).mockResolvedValue({ harnesses, errors: ['sync /h/x: permission denied'] });
  await waitFor(() => expect((within(dialog).getByRole('button', { name: 'Leave in place and turn off' }) as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(within(dialog).getByRole('button', { name: 'Leave in place and turn off' }));
  await waitFor(() => expect(api.applyHarness).toHaveBeenCalledTimes(2));
  expect(await within(dialog).findByText('sync /h/x: permission denied')).toBeTruthy();
  expect(changed).not.toHaveBeenCalled();
  fireEvent.click(within(dialog).getByRole('button', { name: 'Done' }));
  await waitFor(() => expect(changed).toHaveBeenCalled());
});

it('asks for a fresh preview when machine.toml changed before Take out', async () => {
  vi.mocked(api.takeOutHarness).mockRejectedValueOnce(Object.assign(new Error('machine.toml changed since the preview — preview again'), { status: 409 }));
  mount();
  fireEvent.click(await screen.findByRole('checkbox', { name: 'Sync into codex' }));
  const dialog = await screen.findByRole('dialog');
  await within(dialog).findByText('/h/.codex/skills/pdf');
  fireEvent.click(within(dialog).getByRole('button', { name: 'Take out and turn off' }));
  expect(await within(dialog).findByText(/preview again/)).toBeTruthy();
  // The stale preview is gone: nothing can be applied until it is taken again.
  expect((within(dialog).getByRole('button', { name: 'Apply' }) as HTMLButtonElement).disabled).toBe(true);
  fireEvent.click(within(dialog).getByRole('button', { name: 'Preview again' }));
  await waitFor(() => expect(api.previewHarness).toHaveBeenCalledTimes(2));
});

it('opens the daemon configuration even when settings cannot be parsed', async () => {
  vi.mocked(api.harnessSettings).mockRejectedValueOnce(new Error('Invalid TOML'));
  mount();
  await screen.findByText(/Invalid TOML/);
  fireEvent.click(screen.getByRole('button', { name: 'Open configuration' }));
  expect(api.openMachineConfig).toHaveBeenCalledWith('code');
});
