import type { Assignment, CachedAssignmentSnapshot as AssignmentSnapshot, HarnessAssignment, ItemRef, ItemView, MachineReport, MachineStatus, ToggleChange } from './types';

export const statusOrder: MachineStatus[] = ['offline', 'blocked', 'conflict', 'behind', 'drafting', 'in_sync'];
export const statusLabels: Record<MachineStatus, string> = {
  offline: 'Offline', blocked: 'Blocked', conflict: 'Conflict', behind: 'Behind', drafting: 'Drafting', in_sync: 'In sync',
};
export const statusTone: Record<MachineStatus, 'secondary' | 'destructive' | 'warning' | 'info' | 'success'> = {
  offline: 'secondary', blocked: 'destructive', conflict: 'warning', behind: 'info', drafting: 'secondary', in_sync: 'success',
};
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
export function cellState(assignment: Assignment, harness: string, item: ItemRef) {
  const h = assignment.harnesses[harness] ?? emptyHarness();
  const { kind, name } = splitItem(item);
  const items = kind === 'skill' ? h.skills : h.rules;
  const explicit = Object.hasOwn(items, name);
  return { explicit, enabled: explicit ? items[name] : kind === 'skill' ? h.new_skills_default : h.new_rules_default };
}
export function applyToggle(assignment: Assignment, change: ToggleChange): Assignment {
  const next = structuredClone(assignment);
  const h = next.harnesses[change.harness] ??= emptyHarness();
  // Default changes are rendered only after the Server materializes existing items.
  if ((change.kind === 'skill' || change.kind === 'rule') && change.name !== undefined) {
    const map = change.kind === 'skill' ? h.skills : h.rules;
    if (change.enabled === null) delete map[change.name];
    else map[change.name] = change.enabled;
  }
  return next;
}
export const shortSha = (sha: string | null | undefined) => sha?.slice(0, 8) ?? 'None';
export const time = (seconds: number | null) => seconds === null ? 'Never' : new Date(seconds * 1000).toLocaleString();

export function mergeAssignments(current: Record<string, AssignmentSnapshot>, incoming: Record<string, AssignmentSnapshot>) {
  const next = { ...incoming };
  for (const [machine, snapshot] of Object.entries(current)) {
    if (!next[machine] || next[machine].revision < snapshot.revision) next[machine] = snapshot;
  }
  return next;
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
