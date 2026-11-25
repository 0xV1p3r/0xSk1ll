// Every flow renders the real App against the dev mock Daemon (`src/dev/mock.ts`) in jsdom.
import { afterEach, vi } from 'vitest';
import { cleanup } from '@testing-library/react';
import { clearMocks } from '@tauri-apps/api/mocks';
import { resetAppUpdate } from '../updateStore';

// jsdom has no media queries: every window is a wide, light-themed desktop one.
window.matchMedia = query => ({ matches: false, media: query, onchange: null, addEventListener() {}, removeEventListener() {},
  addListener() {}, removeListener() {}, dispatchEvent: () => false });
afterEach(() => {
  cleanup(); vi.useRealTimers(); clearMocks(); resetAppUpdate(); localStorage.clear(); vi.restoreAllMocks();
  window.history.replaceState(null, '', '/');
});
