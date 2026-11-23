import type { Assignment, CachedAssignmentSnapshot as AssignmentSnapshot, HarnessAssignment, ItemId, ItemRef, ItemView, MachineReport, MachineRow, MachineStatus, ToggleChange } from './types';

export const statusOrder: MachineStatus[] = ['offline', 'blocked', 'conflict', 'behind', 'drafting', 'in_sync'];
export const statusLabels: Record<MachineStatus, string> = {
  offline: 'Offline', blocked: 'Blocked', conflict: 'Conflict', behind: 'Behind', drafting: 'Drafting', in_sync: 'In sync',
};
// Badge tones; keep in step with the status bar colours in Fleet.
export const statusTone: Record<MachineStatus, 'secondary' | 'destructive' | 'warning' | 'info' | 'success' | 'drafting'> = {
  offline: 'secondary', blocked: 'destructive', conflict: 'warning', behind: 'info', drafting: 'drafting', in_sync: 'success',
};
export const plural = (count: number, one: string, many = `${one}s`) => `${count} ${count === 1 ? one : many}`;
// Shared vocabulary so every screen names the same things the same way.
export const terms = { explicit: 'Explicit', inherited: 'Inherited', inheritedLong: 'Inherited from the Harness default', search: 'Search Skills and Rules' } as const;
export function headline(offline: boolean, report: MachineReport | null, baseline: string | null): MachineStatus {
  if (offline) return 'offline';
  if (report?.blocked) return 'blocked';
  if (report?.conflicts.length) return 'conflict';
  if (report?.behind || (baseline !== null && report?.baseline !== baseline)) return 'behind';
  return report?.drafts.length ? 'drafting' : 'in_sync';
}
export function emptyHarness(): HarnessAssignment {
  return { new_skills_default: true, new_rules_default: true, skills: {}, rules: {} };
}
export function splitItem(item: ItemRef): { kind: string; name: string } {
  const at = item.indexOf(':');
  return at < 0 ? { kind: item, name: item } : { kind: item.slice(0, at), name: item.slice(at + 1) };
}
// Mirrors the Daemon's warning: plain HTTP sends the Machine token in clear text, except to this computer.
export function insecureHttp(server: string): boolean {
  let url: URL;
  try { url = new URL(server.trim()); } catch { return false; }
  const host = url.hostname.replace(/^\[|\]$/g, '');
  return url.protocol === 'http:' && !(host.toLowerCase() === 'localhost' || host === '::1' || /^127\.\d+\.\d+\.\d+$/.test(host));
}
export function cellState(assignment: Assignment, harness: string, item: ItemRef, id: ItemId | null | undefined) {
  const h = assignment.harnesses[harness] ?? emptyHarness();
  const { kind } = splitItem(item);
  const items = kind === 'skill' ? h.skills : h.rules;
  const explicit = id != null && Object.hasOwn(items, id);
  return { explicit, enabled: explicit ? items[id!] : kind === 'skill' ? h.new_skills_default : h.new_rules_default };
}
export function applyToggle(assignment: Assignment, change: ToggleChange): Assignment {
  const next = structuredClone(assignment);
  const h = next.harnesses[change.harness] ??= emptyHarness();
  // Default changes are rendered only after the Server materializes existing items.
  if ((change.kind === 'skill' || change.kind === 'rule') && change.item_id !== undefined) {
    const map = change.kind === 'skill' ? h.skills : h.rules;
    if (change.enabled === null) delete map[change.item_id];
    else Object.defineProperty(map, change.item_id, { value: change.enabled, writable: true, enumerable: true, configurable: true });
  }
  return next;
}
export const shortSha = (sha: string | null | undefined) => sha?.slice(0, 8) ?? 'None';
export const time = (seconds: number | null) => seconds === null ? 'Never' : new Date(seconds * 1000).toLocaleString();

// Machine ids are data: `constructor` or `__proto__` must not read or write the object's prototype.
export const own = <T,>(record: Record<string, T>, key: string): T | undefined => Object.hasOwn(record, key) ? record[key] : undefined;
// Standalone has no registration.
export const registrationOf = (machine: Pick<MachineRow, 'registration'>) => machine.registration ?? null;
// A snapshot with when its request was sent and answered, on one counter shared by every load and write.
export type StampedSnapshot = AssignmentSnapshot & { sent?: number; received?: number };
let clock = 0;
export async function stamped<T extends AssignmentSnapshot>(request: () => Promise<T>): Promise<T & { sent: number; received: number }> {
  const sent = ++clock, value = await request();
  return { ...value, sent, received: ++clock };
}
// A cached snapshot stays only when its request overlapped the incoming one and it saw a later revision; a request
// sent after the cached one was answered is fresher whatever its revision (a re-registered Machine starts over).
const keeps = (cached: StampedSnapshot, incoming: StampedSnapshot) => cached.revision > incoming.revision
  && !(incoming.sent !== undefined && cached.received !== undefined && incoming.sent > cached.received);
export function mergeAssignments<S extends StampedSnapshot>(current: Record<string, S>, incoming: Record<string, S>): Record<string, S> {
  const next = new Map(Object.entries(incoming));
  for (const [machine, snapshot] of Object.entries(current)) {
    const fresh = next.get(machine);
    if (!fresh || keeps(snapshot, fresh)) next.set(machine, snapshot);
  }
  return Object.fromEntries(next);
}
export function itemStateLabel(item: Pick<ItemView, 'in_baseline' | 'unpushed'>): string | undefined {
  return item.unpushed ? 'Unpushed' : !item.in_baseline ? 'Draft only' : undefined;
}
const relative = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
export function ago(seconds: number | null, now = Date.now() / 1000): string {
  if (seconds === null) return 'Never';
  const delta = seconds - now;
  for (const [unit, size] of [['day', 86400], ['hour', 3600], ['minute', 60]] as const) {
    if (Math.abs(delta) >= size) return relative.format(Math.round(delta / size), unit);
  }
  return Math.abs(delta) < 10 ? 'Just now' : relative.format(Math.round(delta), 'second');
}
