// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { CopyAssignment } from './CopyAssignment';
import { api } from './api';

vi.mock('./api', () => ({
  api: { previewAssignmentCopy: vi.fn(), copyMachineAssignment: vi.fn(), assignment: vi.fn() },
  errorMessage: String,
  errorStatus: (e: { status?: number }) => e?.status,
}));

afterEach(cleanup);

it('previews a mapped merge and confirms with both previewed registrations and revisions', async () => {
  const target = { revision: 2, registration: 'target-reg' };
  const source = { revision: 4, registration: 'source-reg' };
  const update = { revision: 5, assignment: { harnesses: {} }, entries: [] };
  vi.mocked(api.previewAssignmentCopy).mockResolvedValue({ count: 1, source, target });
  vi.mocked(api.copyMachineAssignment).mockResolvedValue(update);
  const copied = vi.fn();
  render(<CopyAssignment target="work" expected={target} targetHarnesses={['codex']}
    sources={[{ id: 'home', harnesses: ['claude'] }]} close={vi.fn()} copied={copied} />);
  fireEvent.change(screen.getByLabelText('Source Machine'), { target: { value: 'home' } });
  fireEvent.change(screen.getByLabelText('Mode'), { target: { value: 'merge' } });
  const preview = screen.getByRole('button', { name: 'Preview changes' });
  expect((preview as HTMLButtonElement).disabled).toBe(true);
  fireEvent.change(screen.getByLabelText('Source Harness for codex'), { target: { value: 'claude' } });
  fireEvent.click(preview);
  const body = { source: 'home', mode: 'merge', harnesses: [{ from: 'claude', to: 'codex' }], expected: target };
  await screen.findByText('1 change to item toggles and defaults');
  expect(api.previewAssignmentCopy).toHaveBeenCalledWith('work', body);
  expect(api.copyMachineAssignment).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Copy Assignment' }));
  await waitFor(() => expect(copied).toHaveBeenCalledWith(update));
  expect(api.copyMachineAssignment).toHaveBeenCalledWith('work', { ...body, source_expected: source });
});

it('reloads the target and requires a new preview in the same dialog after a conflict', async () => {
  const source = { revision: 1, registration: 'source' };
  const target = { revision: 1, registration: 'target' };
  vi.mocked(api.previewAssignmentCopy).mockClear().mockResolvedValue({ count: 1, source, target });
  vi.mocked(api.copyMachineAssignment).mockRejectedValueOnce(Object.assign(new Error('Assignment changed'), { status: 409 }));
  vi.mocked(api.assignment).mockResolvedValue({ revision: 2, registration: 'target', assignment: {
    harnesses: { codex: { skills: {}, rules: {}, new_skills_default: false, new_rules_default: false } },
  } });
  render(<CopyAssignment target="work" expected={target} targetHarnesses={['claude']}
    sources={[{ id: 'home', harnesses: ['claude', 'codex'] }]} close={vi.fn()} copied={vi.fn()} />);
  fireEvent.change(screen.getByLabelText('Source Machine'), { target: { value: 'home' } });
  fireEvent.click(screen.getByRole('button', { name: 'Preview changes' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Copy Assignment' }));
  await screen.findByLabelText('Source Harness for codex');
  expect(screen.queryByLabelText('Source Harness for claude')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Preview changes' }));
  expect(api.previewAssignmentCopy).toHaveBeenLastCalledWith('work', expect.objectContaining({
    expected: { ...target, revision: 2 }, harnesses: [{ from: 'codex', to: 'codex' }],
  }));
});
