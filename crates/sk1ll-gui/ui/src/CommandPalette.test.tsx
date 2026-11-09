// @vitest-environment jsdom
import { useState } from 'react';
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { CommandPalette } from './CommandPalette';
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
