// Editing a Library file in place: unsaved edits are guarded, and a save never silently overwrites a disk edit.
import { expect, it, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import { invoke } from '@tauri-apps/api/core';
import { dialog, heading, launch, main, notify } from './harness';

it('edits a file in place: guards unsaved edits, saves against what it read, and never silently overwrites a disk edit', async () => {
  const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false);
  // Only the five-second status poll notices that the Daemon went away.
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const { daemon, user } = launch('fleet', { hash: '#/library' });
  await heading('Library');
  await user.click(await screen.findByRole('row', { name: 'View Personal code-review' }));
  const detail = await dialog('code-review');
  await user.click(await detail.findByRole('button', { name: 'Edit' }));
  const text = detail.getByRole('textbox', { name: 'SKILL.md' });
  await user.clear(text); await user.type(text, '# First edit');
  await user.click(detail.getByRole('button', { name: 'example.md' }));
  expect(confirm).toHaveBeenCalled();
  expect(detail.getByRole('textbox', { name: 'SKILL.md' })).toBe(text);

  // The Daemon goes away and comes back: the unsaved edit stays.
  daemon.running = false;
  await act(() => vi.advanceTimersByTimeAsync(5000));
  expect(await main().findByText(/Daemon disconnected. Your unsaved changes are still here/)).toBeTruthy();
  expect((detail.getByRole('textbox', { name: 'SKILL.md' }) as HTMLTextAreaElement).value).toBe('# First edit');
  daemon.running = true;
  await notify('daemon-connection', { reachable: true, message: null, retry_in_ms: 0 });
  await waitFor(() => expect(main().queryByText(/Daemon disconnected/)).toBeNull());

  await user.click(text);
  await user.keyboard('{Control>}s{/Control}');
  const review = await detail.findByRole('button', { name: 'Review in Draft' });
  expect(daemon.sent('write_skill_file')).toEqual([{ request: { item: 'skill:code-review', path: 'SKILL.md', content: '# First edit', expected: 'mock-file' } }]);
  expect(document.activeElement).toBe(detail.getByRole('heading', { name: 'SKILL.md' }));

  // Someone else saves the file while this edit is open, twice.
  const write = (content: string, expected: string) => invoke('write_skill_file', { request: { item: 'skill:code-review', path: 'SKILL.md', content, expected } });
  await user.click(detail.getByRole('button', { name: 'Edit' }));
  await user.type(detail.getByRole('textbox', { name: 'SKILL.md' }), '\nMine');
  await write('# Disk edit', 'file-1');
  await user.click(detail.getByRole('button', { name: 'Save' }));
  expect(await detail.findByText('File changed on disk')).toBeTruthy();
  expect(await detail.findByText(/# Disk edit/)).toBeTruthy();
  await write('# Later edit', 'file-2');
  await user.click(detail.getByRole('button', { name: 'Overwrite' }));
  expect(await detail.findByText(/# Later edit/)).toBeTruthy();
  // The App's own saves (the other writes are the outside edits above).
  const saves = daemon.sent('write_skill_file').map(args => args.request as { content: string; expected: string }).filter(request => request.content.endsWith('Mine'));
  expect(saves.map(request => request.expected)).toEqual(['file-1', 'file-2']);
  await user.click(detail.getByRole('button', { name: 'Reload' }));
  expect((detail.getByRole('textbox', { name: 'SKILL.md' }) as HTMLTextAreaElement).value).toBe('# Later edit');
  expect(detail.queryByText('File changed on disk')).toBeNull();
  await user.click(detail.getByRole('button', { name: 'Cancel' }));
  await user.click(review);
  await heading('Draft');
});
