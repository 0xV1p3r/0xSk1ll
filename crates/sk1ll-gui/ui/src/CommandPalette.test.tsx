// @vitest-environment jsdom
import { useState } from 'react';
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { CommandPalette } from './CommandPalette';
import { screenFromHash } from './App';
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
function Fixture() {
  const [open, setOpen] = useState(false);
  return <><button onClick={() => setOpen(true)}>Open palette</button><h1 tabIndex={-1}>Destination</h1>
    <CommandPalette open={open} onOpenChange={setOpen} commands={[{ id: 'go', label: 'Navigate', select: () => { document.querySelector('h1')?.focus(); } }]} /></>;
}
it.each(['Escape', 'Close'])('restores previous focus on %s dismissal', async method => {
  vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() })));
  render(<Fixture />);
  const trigger = screen.getByRole('button', { name: 'Open palette' });
  trigger.focus(); fireEvent.click(trigger);
  const input = await screen.findByRole('combobox');
  await waitFor(() => expect(document.activeElement).toBe(input));
  if (method === 'Escape') fireEvent.keyDown(input, { key: 'Escape' });
  else fireEvent.click(screen.getByRole('button', { name: 'Close' }));
  await waitFor(() => expect(document.activeElement).toBe(trigger));
});
it('lets navigation focus the destination', async () => {
  render(<Fixture />);
  const trigger = screen.getByRole('button', { name: 'Open palette' });
  trigger.focus(); fireEvent.click(trigger);
  fireEvent.click(await screen.findByRole('option', { name: 'Navigate' }));
  await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Destination' })));
});
it('matches every query word in any order and selects a machine with the keyboard', async () => {
  const select = vi.fn();
  render(<CommandPalette open onOpenChange={() => {}} commands={[
    { id: 'machine', label: 'Machine: studio linux', select },
    { id: 'other-machine', label: 'Machine: studio mac', select: vi.fn() },
    { id: 'action', label: 'Action: Pull Baseline', select: vi.fn() },
  ]} />);
  const input = await screen.findByRole('combobox');
  fireEvent.change(input, { target: { value: ' linux   machine studio ' } });
  expect(screen.getAllByRole('option')).toHaveLength(1);
  expect(input.getAttribute('aria-activedescendant')).toBe('machine');
  fireEvent.keyDown(input, { key: 'Enter' });
  expect(select).toHaveBeenCalledOnce();
});
it('finds actions and announces an empty result when a query word is missing', async () => {
  const select = vi.fn();
  render(<CommandPalette open onOpenChange={() => {}} commands={[
    { id: 'pull', label: 'Action: Pull Baseline', select },
    { id: 'push', label: 'Action: Push', select: vi.fn() },
  ]} />);
  const input = await screen.findByRole('combobox');
  fireEvent.change(input, { target: { value: 'baseline action' } });
  fireEvent.click(screen.getByRole('option', { name: 'Action: Pull Baseline' }));
  expect(select).toHaveBeenCalledOnce();
  fireEvent.change(input, { target: { value: 'baseline missing' } });
  expect(screen.queryAllByRole('option')).toHaveLength(0);
  expect(screen.getByRole('status').textContent).toBe('No matching commands');
  expect(input.hasAttribute('aria-activedescendant')).toBe(false);
});

it.each([['#/assignment-matrix', 'Assignment matrix'], ['#/library', 'Library'], ['#/draft', 'Draft'], ['#/unknown', 'Fleet'], ['', 'Fleet']])('resolves screen hash %s to %s', (hash, expected) => {
  expect(screenFromHash(hash)).toBe(expected);
});
it('returns focus to the previous element after a background action', async () => {
  vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() })));
  const run = vi.fn();
  function Background() {
    const [open, setOpen] = useState(false);
    return <><button onClick={() => setOpen(true)}>Open palette</button>
      <CommandPalette open={open} onOpenChange={setOpen} commands={[{ id: 'pull', label: 'Action: Pull', background: true, select: run }]} /></>;
  }
  render(<Background />);
  const trigger = screen.getByRole('button', { name: 'Open palette' });
  trigger.focus(); fireEvent.click(trigger);
  const input = await screen.findByRole('combobox');
  fireEvent.keyDown(input, { key: 'Enter' });
  expect(run).toHaveBeenCalled();
  await waitFor(() => expect(document.activeElement).toBe(trigger));
});
