// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { Settings } from './Settings';
import { api } from './api';
import type { StatusView } from './types';
vi.mock('./api', () => ({ api: { harnesses: vi.fn(), editors: vi.fn(), setPreferredEditor: vi.fn() }, errorMessage: String }));
vi.mock('./toast', () => ({ toast: vi.fn() }));
afterEach(cleanup);
beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.harnesses).mockResolvedValue([]);
  vi.mocked(api.editors).mockResolvedValue({ preferred_editor: 'code', editors: [
    { id: 'code', name: 'VS Code', kind: 'gui' }, { id: 'vim', name: 'Vim', kind: 'terminal' },
  ] });
  vi.mocked(api.setPreferredEditor).mockResolvedValue();
});
const mount = (revoked = false) => render(<Settings status={{ server_url: 'https://example.com', connection: 'connected',
  revoked, machine_id: 'laptop', auth_rejected_since: null } as StatusView} socket="/daemon.sock" revision={0} changed={vi.fn()} />);
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
