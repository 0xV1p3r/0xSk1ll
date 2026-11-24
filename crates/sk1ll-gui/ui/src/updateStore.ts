// Updates of the app itself, from the GitHub releases. The Rust side checks, verifies and installs
// (sk1ll-gui/src-tauri/src/updates.rs); this store shares what it found between the header and Settings.
import { useSyncExternalStore } from 'react';
import { listen } from '@tauri-apps/api/event';
import { api, errorMessage } from './api';
import type { AvailableUpdate, UpdateProgress } from './types';

export type AppUpdateState = {
  // This build can update itself; `null` until asked.
  supported: boolean | null;
  checking: boolean;
  // A check has finished; with `update` null, the app is up to date.
  checked: boolean;
  update: AvailableUpdate | null;
  // Set while an install runs.
  progress: UpdateProgress | null;
  error: string;
};

const autoKey = 'sk1ll-check-updates';
const skippedKey = 'sk1ll-skipped-updates';
const initial: AppUpdateState = { supported: null, checking: false, checked: false, update: null, progress: null, error: '' };
let state = initial;
const listeners = new Set<() => void>();
const set = (next: Partial<AppUpdateState>) => { state = { ...state, ...next }; listeners.forEach(listener => listener()); };
let startupDone = false;

export function checkAutomatically(): boolean {
  try { return localStorage.getItem(autoKey) !== 'off'; } catch { return true; }
}
export function setCheckAutomatically(on: boolean) {
  try { if (on) localStorage.removeItem(autoKey); else localStorage.setItem(autoKey, 'off'); } catch { /* not persisted */ }
  set({});
}
export function skippedVersions(): string[] {
  try { const stored: unknown = JSON.parse(localStorage.getItem(skippedKey) ?? '[]'); return Array.isArray(stored) ? stored.filter(v => typeof v === 'string') : []; } catch { return []; }
}
export function skipVersion(version: string) {
  try { localStorage.setItem(skippedKey, JSON.stringify([...skippedVersions(), version])); } catch { /* not persisted */ }
  set({});
}

async function supported() {
  if (state.supported === null) {
    let value = false;
    try { value = await api.updaterAvailable(); } catch { /* an older bridge, or no Tauri */ }
    set({ supported: value });
  }
  return state.supported;
}

export async function checkForUpdate() {
  if (state.checking || state.progress) return;
  if (!await supported()) return;
  set({ checking: true, error: '' });
  try { set({ update: await api.checkUpdate(), checking: false, checked: true }); }
  catch (e) { set({ error: errorMessage(e), checking: false, checked: true }); }
}

// Once per app run, unless turned off in Settings.
export function checkAtStartup() {
  if (startupDone) return;
  startupDone = true;
  void supported().then(ok => { if (ok && checkAutomatically()) void checkForUpdate(); });
}

// Installs the update on offer; the app relaunches when it succeeds.
export async function installUpdate() {
  const update = state.update;
  if (!update || state.progress) return;
  set({ progress: { stage: 'downloading', percent: null }, error: '' });
  let unlisten: (() => void) | undefined;
  try {
    unlisten = await listen<UpdateProgress>('app-update-progress', event => set({ progress: event.payload }));
    await api.installUpdate(update.version);
    set({ progress: null });
  } catch (e) {
    set({ progress: null, error: errorMessage(e) });
  } finally { unlisten?.(); }
}

export function useAppUpdate() {
  return useSyncExternalStore(listener => { listeners.add(listener); return () => listeners.delete(listener); }, () => state);
}

export function progressLabel(progress: UpdateProgress): string {
  switch (progress.stage) {
    case 'downloading': return progress.percent === null ? 'Downloading…' : `Downloading… ${progress.percent}%`;
    case 'installing': return 'Installing… Enter your password if asked.';
    case 'restarting_daemon': return 'Restarting the Daemon…';
    case 'relaunching': return 'Relaunching…';
  }
}

// Tests start from a clean store.
export function resetAppUpdate() { state = initial; startupDone = false; listeners.forEach(listener => listener()); }
