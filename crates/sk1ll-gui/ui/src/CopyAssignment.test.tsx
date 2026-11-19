// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { CopyAssignment } from './CopyAssignment';
import { api } from './api';

vi.mock('./api', () => ({
  api: { previewAssignmentCopy: vi.fn(), copyMachineAssignment: vi.fn() },
  errorMessage: String,
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
