import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { emit } from '@tauri-apps/api/event';
import { App } from '../App';
import { installMock, type MockDaemon, type MockOptions } from '../dev/mock';

type Launch = MockOptions & { hash?: string; setup?: (daemon: MockDaemon) => void };
// Starts the App on `hash` against a fresh mock Daemon running `scenario` (see `installMock`), with no latency.
// `setup` changes the Daemon's state before the App first asks for it.
export function launch(scenario = 'fleet', { hash = '', setup, ...options }: Launch = {}) {
  const daemon = installMock(scenario, { latency: 0, ...options });
  setup?.(daemon);
  window.history.replaceState(null, '', `/${hash}`);
  const user = userEvent.setup();
  const view = render(<App />);
  return { daemon, user, view };
}

// The Daemon's change notification, as the Tauri bridge forwards it.
export const notify = (event: 'status' | 'draft' | 'fleet' | 'assignment' | 'imports' | 'daemon-connection', payload?: unknown) =>
  act(() => emit(event, payload));
export const heading = (name: string) => screen.findByRole('heading', { name, level: 1 });
export const nav = () => within(screen.getByRole('navigation', { name: 'Main navigation' }));
export const dialog = async (name?: string | RegExp) => within(await screen.findByRole('dialog', name ? { name } : {}));
export const checked = (element: HTMLElement) => element.getAttribute('aria-checked') === 'true';
export const isDisabled = (element: HTMLElement) => (element as HTMLButtonElement).disabled || element.hasAttribute('data-disabled');
// A toast's own buttons stay aria-hidden until it is hovered or focused.
export const toastButton = (name: string) => within(screen.getByRole('region', { name: 'Notifications' })).getAllByText(name, { selector: 'button' }).at(-1)!;
// The current screen, without the sidebar, header and toasts.
export const main = () => within(document.getElementById('main')!);
