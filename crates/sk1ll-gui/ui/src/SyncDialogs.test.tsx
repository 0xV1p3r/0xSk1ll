// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { api } from './api';
import { InstructionsDialog, SyncDialog } from './SyncDialogs';
import { toast } from './toast';

vi.mock('./api', () => ({ api: { renderInstructions: vi.fn(), previewSync: vi.fn(), syncNow: vi.fn() }, errorMessage: String }));
vi.mock('./toast', () => ({ toast: vi.fn() }));
afterEach(() => { cleanup(); vi.resetAllMocks(); });

it('shows the proposed file and switches the diff when a file tab is selected', async () => {
  vi.mocked(api.renderInstructions).mockResolvedValue([
    { path: '/rules/style.md', format: 'rules', content: 'New style', next_content: 'New style',
      differs: true, diff: '@@ -1 +1 @@\n-Old style\n+New style' },
    { path: '/rules/security.md', format: 'rules', content: 'Keep secrets private', next_content: 'Keep secrets private',
      differs: false, diff: '' },
  ]);
  render(<InstructionsDialog harness="claude" onClose={vi.fn()} />);
  expect(await screen.findByText('New style')).toBeTruthy();
  expect(screen.getByText('+New style')).toBeTruthy();
  expect(api.renderInstructions).toHaveBeenCalledWith('claude');
  fireEvent.click(screen.getByRole('tab', { name: 'security.md' }));
  expect(screen.getByText('Keep secrets private')).toBeTruthy();
  expect(screen.getByText('Matches the file on disk.')).toBeTruthy();
  expect(screen.queryByText('+New style')).toBeNull();
});

it('lists the dry-run plan and applies only after confirmation', async () => {
  vi.mocked(api.previewSync).mockResolvedValue({ fingerprint: 'reviewed', conflicts: [], actions: [
    { harness: 'codex', kind: 'create', item: 'skill', name: 'pdf', dest: '/skills/pdf', reason: null },
    { harness: 'codex', kind: 'conflict', item: 'rule', name: 'style', dest: '/rules/style.md', reason: 'Edited by hand' },
  ] });
  vi.mocked(api.syncNow).mockResolvedValue({ conflicts: [] });
  const changed = vi.fn(), close = vi.fn();
  render(<SyncDialog changed={changed} onClose={close} />);
  expect(await screen.findByText('/skills/pdf')).toBeTruthy();
  expect(screen.getByText('Review in Conflicts.')).toBeTruthy();
  expect(screen.getAllByText('1')).toHaveLength(2);
  expect(api.syncNow).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(changed).toHaveBeenCalledOnce();
  expect(api.syncNow).toHaveBeenCalledExactlyOnceWith('reviewed');
  expect(toast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Sync complete' }));
});

it('requires a second confirmation when the daemon returns a changed plan', async () => {
  const action = { harness: 'codex', kind: 'create', item: 'skill', name: 'pdf', dest: '/skills/pdf', reason: null } as const;
  vi.mocked(api.previewSync).mockResolvedValue({ fingerprint: 'first', conflicts: [], actions: [action] });
  vi.mocked(api.syncNow).mockResolvedValueOnce({ fingerprint: 'new', conflicts: [], actions: [{ ...action, dest: '/new/pdf' }] })
    .mockResolvedValueOnce({ conflicts: [] });
  const close = vi.fn();
  render(<SyncDialog changed={vi.fn()} onClose={close} />);
  await screen.findByText('/skills/pdf');
  fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
  await screen.findByText('/new/pdf');
  expect(close).not.toHaveBeenCalled();
  expect(api.syncNow).toHaveBeenCalledExactlyOnceWith('first');
  fireEvent.click(screen.getByRole('button', { name: 'Apply' }));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(api.syncNow).toHaveBeenLastCalledWith('new');
});
