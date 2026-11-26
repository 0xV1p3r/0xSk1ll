import type { Assignment, StatusView, CachedAssignmentSnapshot as AssignmentSnapshot, HarnessAssignment, ItemId, ItemRef, ItemView, MachineReport, MachineRow, MachineStatus, PullResult, ToggleChange } from './types';

export const statusOrder: MachineStatus[] = ['offline', 'blocked', 'conflict', 'behind', 'drafting', 'in_sync'];
export const statusLabels: Record<MachineStatus, string> = {
  offline: 'Offline', blocked: 'Blocked', conflict: 'Conflict', behind: 'Behind', drafting: 'Drafting', in_sync: 'In sync',
};
// Badge tones; keep in step with the status bar colours in Fleet.
export const statusTone: Record<MachineStatus, 'secondary' | 'destructive' | 'warning' | 'info' | 'success' | 'drafting'> = {
  offline: 'secondary', blocked: 'destructive', conflict: 'warning', behind: 'info', drafting: 'drafting', in_sync: 'success',
};
export const plural = (count: number, one: string, many = `${one}s`) => `${count} ${count === 1 ? one : many}`;
export function pullToast(result: PullResult) {
  if (result.state !== 'blocked') return { title: result.state === 'up_to_date' ? 'Baseline is up to date' : 'Pull complete' };
  const description = result.items.length ? `The new Baseline also changes ${itemNames(result.items)} in this Machine's Draft.`
    : result.pending ? 'An interrupted pull still needs to finish.' : "The new Baseline clashes with this Machine's Draft.";
  return { title: 'Pull blocked', description };
}
// Shared vocabulary so every screen names the same things the same way.
export const terms = { explicit: 'Explicit', inherited: 'Inherited', inheritedLong: 'Inherited from the Harness default', search: 'Search Skills and Rules' } as const;
export function headline(offline: boolean, report: MachineReport | null, baseline: string | null): MachineStatus {
  if (offline) return 'offline';
  if (report?.blocked) return 'blocked';
  if (report?.conflicts.length) return 'conflict';
  if (report?.behind || (baseline !== null && report?.baseline !== baseline)) return 'behind';
  return report?.drafts.length ? 'drafting' : 'in_sync';
}
// A Harness's entry, or what it has without one (as the Daemon's `Assignment::harness`).
export function harnessOf(assignment: Assignment, harness: string): HarnessAssignment {
  return own(assignment.harnesses, harness) ?? uniformHarness(assignment.new_harnesses_default);
}
export function uniformHarness(enabled: boolean): HarnessAssignment {
  return { new_skills_default: enabled, new_rules_default: enabled, skills: {}, rules: {} };
}
export function splitItem(item: ItemRef): { kind: string; name: string } {
  const at = item.indexOf(':');
  return at < 0 ? { kind: item, name: item } : { kind: item.slice(0, at), name: item.slice(at + 1) };
}
// Item IDs as people read them: `skill:pdf` → `pdf`. Commands keep the full ID.
export const itemNames = (items: ItemRef[]) => items.map(item => splitItem(item).name).join(', ');
// Mirrors the Daemon's warning: plain HTTP sends the Machine token in clear text, except to this computer.
export function insecureHttp(server: string): boolean {
  let url: URL;
  try { url = new URL(server.trim()); } catch { return false; }
  const host = url.hostname.replace(/^\[|\]$/g, '');
  return url.protocol === 'http:' && !(host.toLowerCase() === 'localhost' || host === '::1' || /^127\.\d+\.\d+\.\d+$/.test(host));
}
export function cellState(assignment: Assignment, harness: string, item: ItemRef, id: ItemId | null | undefined) {
  const h = harnessOf(assignment, harness);
  const { kind } = splitItem(item);
  const items = kind === 'skill' ? h.skills : h.rules;
  const explicit = id != null && Object.hasOwn(items, id);
  return { explicit, enabled: explicit ? items[id!] : kind === 'skill' ? h.new_skills_default : h.new_rules_default };
}
export function applyToggle(assignment: Assignment, change: ToggleChange): Assignment {
  const next = structuredClone(assignment);
  const h = next.harnesses[change.harness] ??= uniformHarness(next.new_harnesses_default);
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

type Release = { core: number[]; pre: string[] | null };
function parseRelease(value: string | null | undefined): Release | null {
  // `0.2.0`, `0.2.0-rc.1`, or the Daemon's `0.2.0 (abc1234567)`.
  const match = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\s|$)/.exec(value?.trim() ?? '');
  return match ? { core: match.slice(1, 4).map(Number), pre: match[4]?.split('.') ?? null } : null;
}
const numeric = (part: string) => /^\d+$/.test(part);
// Orders two releases (negative: `a` is older); a pre-release comes before its release. `null`: not both releases.
export function releaseOrder(a: string | null | undefined, b: string | null | undefined): number | null {
  const x = parseRelease(a), y = parseRelease(b);
  if (!x || !y) return null;
  for (let i = 0; i < 3; i++) if (x.core[i] !== y.core[i]) return x.core[i] - y.core[i];
  if (!x.pre || !y.pre) return x.pre ? -1 : y.pre ? 1 : 0;
  for (let i = 0; i < Math.max(x.pre.length, y.pre.length); i++) {
    const p = x.pre[i], q = y.pre[i];
    if (p === undefined || q === undefined) return p === undefined ? -1 : 1;
    if (p === q) continue;
    if (numeric(p) && numeric(q)) return Number(p) - Number(q);
    if (numeric(p) !== numeric(q)) return numeric(p) ? -1 : 1;
    return p < q ? -1 : 1;
  }
  return 0;
}
// The release part of a version, without the Daemon's `(commit)`.
export const releaseOf = (value: string | null | undefined) => /^\S+/.exec(value?.trim() ?? '')?.[0] ?? '';
export type VersionNotice =
  | { kind: 'server-incompatible' | 'app-incompatible'; server: string | null }
  | { kind: 'incompatible' }
  | { kind: 'server-older'; server: string; machine: string }
  | { kind: 'daemon-older'; daemon: string; app: string };
// What the header warns about versions: a Server this Machine can't talk to (and which side is older),
// a compatible but older Server, and a Daemon older than this app (an update not yet restarted into).
export function versionNotices(status: Pick<StatusView, 'connection' | 'version' | 'protocol' | 'server'>, app: string): VersionNotice[] {
  const notices: VersionNotice[] = [];
  const daemon = releaseOf(status.version);
  const server = status.server?.version ?? null;
  const theirs = status.server?.protocol ?? null;
  if (status.connection === 'incompatible') {
    if (theirs === null || status.protocol === undefined) notices.push({ kind: 'incompatible' });
    else notices.push({ kind: theirs < status.protocol ? 'server-incompatible' : 'app-incompatible', server });
  } else if (server && (releaseOrder(server, daemon) ?? 0) < 0) {
    notices.push({ kind: 'server-older', server, machine: daemon });
  }
  if ((releaseOrder(daemon, app) ?? 0) < 0) notices.push({ kind: 'daemon-older', daemon, app });
  return notices;
}
