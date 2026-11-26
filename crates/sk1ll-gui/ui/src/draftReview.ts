import { api } from './api';
import { splitItem } from './model';
import type { DraftEntry } from './types';

export interface DraftSnapshotEntry extends DraftEntry { diff: string }
export interface DraftReview {
  items: string[];
  snapshot: DraftSnapshotEntry[];
  watchAll: boolean;
  changes: string[];
}
// Capture both byte fingerprints and readable diffs for each review.
export async function loadDraft(): Promise<DraftSnapshotEntry[]> {
  const entries = await api.draft();
  return Promise.all(entries.map(async entry => ({ ...entry, diff: (await api.diff(entry.item)).diff })));
}
export function captureReview(snapshot: DraftSnapshotEntry[], items: string[], watchAll = false): DraftReview {
  return { items: [...items], snapshot, watchAll, changes: [] };
}
export function reconcileReview(review: DraftReview, snapshot: DraftSnapshotEntry[]): DraftReview {
  const previous = new Map(review.snapshot.map(entry => [entry.item, entry]));
  const current = new Map(snapshot.map(entry => [entry.item, entry]));
  const watched = review.watchAll ? new Set([...previous.keys(), ...current.keys()]) : new Set(review.items);
  const changes = [...review.changes];
  for (const item of watched) {
    const before = previous.get(item), after = current.get(item);
    const signature = (entry: DraftSnapshotEntry | undefined) => entry && JSON.stringify([
      entry.change, [...entry.paths].sort(), entry.fingerprint, entry.diff,
    ]);
    if (signature(before) === signature(after)) continue;
    const reason = !after ? 'no longer in Draft' : !before ? 'added to Draft' : 'Draft content changed';
    const change = `${splitItem(item).name}: ${reason}`;
    if (!changes.includes(change)) changes.push(change);
  }
  return { ...review, snapshot, changes };
}
export function confirmReview(review: DraftReview): DraftReview {
  const existing = new Set(review.snapshot.map(entry => entry.item));
  return { ...review, items: review.items.filter(item => existing.has(item)), changes: [] };
}
