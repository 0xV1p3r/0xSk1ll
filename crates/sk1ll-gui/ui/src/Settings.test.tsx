// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Settings } from './Settings';
import { api } from './api';
import type { StatusView } from './types';
vi.mock('./api', () => ({ api: { fleet: vi.fn(), join: vi.fn(), harnesses: vi.fn(), editors: vi.fn(), setPreferredEditor: vi.fn() }, errorMessage: String }));
vi.mock('./toast', () => ({ toast: vi.fn() }));
afterEach(cleanup);
beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [{ id: 'desktop', status: 'in_sync', connected: true, joined_at: 1, last_seen: 1, report: null }] });
  vi.mocked(api.harnesses).mockResolvedValue([]);
  vi.mocked(api.editors).mockResolvedValue({ preferred_editor: 'code', editors: [
    { id: 'code', name: 'VS Code', kind: 'gui' }, { id: 'vim', name: 'Vim', kind: 'terminal' },
  ] });
  vi.mocked(api.setPreferredEditor).mockResolvedValue();
});
const mount = (revoked = false) => render(<Settings status={{ server_url: 'https://example.com', connection: 'connected',
  revoked, machine_id: 'laptop', assignment_revision: 4, pending_discards: 0, auth_rejected_since: null } as StatusView} socket="/daemon.sock" revision={0} changed={vi.fn()} />);
it('collapses rejoin when joined', async () => {
  mount(); expect(screen.queryByLabelText('Server URL')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Rejoin this Machine…' }));
  expect(await screen.findByLabelText('Server URL')).toBeTruthy();
});
it('expands rejoin when revoked', () => { mount(true); expect(screen.getByLabelText('Server URL')).toBeTruthy(); });
it('saves editor changes', async () => {
  mount(); const select = await screen.findByLabelText('Preferred editor');
  fireEvent.change(select, { target: { value: 'vim' } });
  await waitFor(() => expect(api.setPreferredEditor).toHaveBeenCalledWith('vim'));
});

it('orders sections and hides inactive diagnostic rows', async () => {
  mount(); await screen.findByLabelText('Preferred editor');
  expect(screen.getAllByRole('heading', { level: 2 }).map(node => node.textContent)).toEqual(['This machine', 'Fleet membership', 'Editor', 'Diagnostics']);
  expect(screen.queryByText('Pending Restore reports')).toBeNull(); expect(screen.queryByText('Auth rejected since')).toBeNull();
  expect(screen.getByText('/daemon.sock')).toBeTruthy(); expect(screen.getByText('Assignment revision')).toBeTruthy();
});
it('reveals the join code and confirms the effective default Machine id', async () => {
  mount(true);
  const id = screen.getByLabelText('Machine id') as HTMLInputElement;
  expect(id.value).toBe(''); expect(id.placeholder).toBe('laptop');
  const code = screen.getByLabelText('Join code') as HTMLInputElement;
  expect(code.type).toBe('password'); fireEvent.click(screen.getByRole('button', { name: 'Show join code' })); expect(code.type).toBe('text');
  fireEvent.click(screen.getByRole('button', { name: 'Hide join code' })); expect(code.type).toBe('password');
  fireEvent.change(code, { target: { value: 'secret' } });
  const select = await screen.findByLabelText('Copy Assignment from Machine');
  await screen.findByRole('option', { name: 'desktop' }); fireEvent.change(select, { target: { value: 'desktop' } });
  fireEvent.submit(code.closest('form')!);
  const dialog = await screen.findByRole('dialog'); expect(dialog.textContent).toContain('laptop'); expect(dialog.textContent).toContain('Copied from desktop');
});
it('joins with the admin token instead of a join code', async () => {
  vi.mocked(api.join).mockResolvedValue({ machine_id: 'laptop', repo: '/skills', setup: 'initialized', harnesses: [], revision: 1 });
  mount(true);
  fireEvent.change(screen.getByLabelText('Join with'), { target: { value: 'admin' } });
  fireEvent.change(screen.getByLabelText('Server URL'), { target: { value: 'https://skills.example.com' } });
  const token = screen.getByLabelText('Admin token') as HTMLInputElement;
  expect(screen.getByRole('button', { name: 'Show admin token' })).toBeTruthy();
  fireEvent.change(token, { target: { value: 'adm' } });
  fireEvent.submit(token.closest('form')!);
  const dialog = await screen.findByRole('dialog'); expect(dialog.textContent).toContain('Admin token');
  fireEvent.click(within(dialog).getByRole('button', { name: 'Join / rejoin' }));
  await waitFor(() => expect(api.join).toHaveBeenCalledWith({ server_url: 'https://skills.example.com', code: '', admin_token: 'adm', machine_id: undefined, copy_from: undefined }));
});
it('falls back to a text field when Fleet loading fails', async () => {
  vi.mocked(api.fleet).mockRejectedValue(new Error('Unavailable')); mount(true);
  await waitFor(() => expect(screen.getByLabelText('Copy Assignment from Machine').tagName).toBe('INPUT'));
});
it('shows an editor empty state and allows refreshing', async () => {
  vi.mocked(api.editors).mockResolvedValue({ preferred_editor: '', editors: [] }); mount();
  expect(await screen.findByText('No editors found')).toBeTruthy(); expect(screen.queryByLabelText('Preferred editor')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Refresh editors' })); await waitFor(() => expect(api.editors).toHaveBeenCalledWith(true));
});
it('uses readable Harness values and preserves path formatting', async () => {
  vi.mocked(api.harnesses).mockResolvedValue([{ name: 'codex', origin: 'machine.toml', harness: { skills: null, instructions: '/rules', instructions_format: 'block', mode: 'symlink' } }]); mount();
  expect((await screen.findByText('Not configured')).tagName).toBe('SPAN'); expect(screen.getByText('/rules').tagName).toBe('CODE');
  expect(screen.getByText('Symbolic link')).toBeTruthy(); expect(screen.getByText('Machine configuration')).toBeTruthy();
});

it('keeps active diagnostic details with a keyboard-reachable timestamp', async () => {
  render(<Settings status={{ server_url: '', machine_id: 'laptop', connection: 'auth_rejected', pending_discards: 2, auth_rejected_since: 1, assignment_revision: 4, last_error: 'Credentials expired' } as StatusView} socket="/daemon.sock" revision={0} changed={vi.fn()} />);
  await screen.findByLabelText('Preferred editor');
  expect(screen.getByText('Pending Restore reports')).toBeTruthy(); expect(screen.getByText('Auth rejected since')).toBeTruthy();
  expect(screen.getByText('Credentials expired')).toBeTruthy(); expect(document.querySelector('time')?.getAttribute('tabindex')).toBe('0');
});
