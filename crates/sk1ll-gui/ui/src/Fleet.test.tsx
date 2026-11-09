// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { TooltipProvider } from '@/components/ui/tooltip';
import { SidebarProvider } from '@/components/ui/sidebar';
import { Fleet } from './Fleet';
import { App, AppSidebar } from './App';
import { CommandPalette } from './CommandPalette';
import { api } from './api';
import type { StatusView } from './types';
vi.mock('./api', () => ({ api: { fleet: vi.fn(), status: vi.fn(), skills: vi.fn(), connectionInfo: vi.fn() }, errorMessage: String }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn().mockResolvedValue(() => {}) }));
beforeEach(() => vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() }))));
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
it('filters Machines by attention and connection, then clears the filter', async () => {
  vi.mocked(api.fleet).mockResolvedValue({ baseline: null, machines: [
    { id: 'online', connected: true, status: 'in_sync', report: null, last_seen: null, joined_at: 1 },
    { id: 'offline', connected: false, status: 'offline', report: null, last_seen: null, joined_at: 1 },
  ] });
  render(<TooltipProvider><Fleet revision={0} machineId="online" navigate={vi.fn()} changed={vi.fn()} live /></TooltipProvider>);
  await screen.findByText('online');
  fireEvent.click(screen.getByRole('button', { name: 'Filter: Need attention' }));
  expect(screen.queryByText('online')).toBeNull(); expect(screen.getByText('offline')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Filter: Connected' }));
  expect(screen.queryByText('offline')).toBeNull(); expect(screen.getByText('online')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Clear Machine filter' })); expect(screen.getByText('offline')).toBeTruthy();
});
it('shows action counts with their purpose and a neutral Draft count', () => {
  const status = { machine_id: 'test', report: { drafts: ['a', 'b'], conflicts: [{}] } } as StatusView;
  render(<TooltipProvider><SidebarProvider><AppSidebar screen="Library" setScreen={vi.fn()} status={status} unmanagedCount={1} /></SidebarProvider></TooltipProvider>);
  expect(screen.getByLabelText('1 Unmanaged skill to adopt').textContent).toBe('1');
  expect(screen.getByLabelText('1 Conflict to resolve').textContent).toBe('1');
  expect(screen.getByLabelText('2 Draft items').textContent).toBe('2');
});
it('opens the palette, filters commands, and navigates with the keyboard', async () => {
  const select = vi.fn();
  const view = render(<CommandPalette open={false} onOpenChange={vi.fn()} commands={[{ id: 'library', label: 'Library', select }]} />);
  expect(screen.queryByRole('dialog')).toBeNull();
  view.rerender(<CommandPalette open onOpenChange={vi.fn()} commands={[{ id: 'library', label: 'Library', select }]} />);
  const input = await screen.findByRole('combobox');
  fireEvent.change(input, { target: { value: 'lib' } }); fireEvent.keyDown(input, { key: 'ArrowDown' }); fireEvent.keyDown(input, { key: 'Enter' });
  expect(select).toHaveBeenCalledOnce();
});

it('opens with Ctrl+K and navigates to Library from App', async () => {
  vi.mocked(api.status).mockResolvedValue({ machine_id: 'test', headline: 'in_sync', connection: 'connected', report: { drafts: [], conflicts: [] } } as unknown as StatusView);
  vi.mocked(api.skills).mockResolvedValue([]); vi.mocked(api.connectionInfo).mockResolvedValue('test');
  render(<App />); await screen.findByRole('heading', { name: 'Fleet', level: 1 });
  fireEvent.keyDown(window, { key: 'k', ctrlKey: true });
  const input = await screen.findByRole('combobox');
  fireEvent.change(input, { target: { value: 'Library' } }); fireEvent.keyDown(input, { key: 'Enter' });
  expect(await screen.findByRole('heading', { name: 'Library', level: 1 })).toBeTruthy();
  expect(screen.queryByRole('dialog')).toBeNull();
});
