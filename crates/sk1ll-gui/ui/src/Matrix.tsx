import { useCallback, useEffect, useRef, useState } from 'react';
import { ChevronRight, Info, MoreHorizontal, RotateCcw } from 'lucide-react';
import { Switch } from '@base-ui/react/switch';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { NativeSelect } from '@/components/ui/native-select';
import { Input } from '@/components/ui/input';
import { DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuCheckboxItem, DropdownMenuSeparator } from '@/components/ui/dropdown-menu';
import { Checkbox } from '@/components/ui/checkbox';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { CopyAssignment } from './CopyAssignment';
import { api, errorMessage } from './api';
import { cellState, emptyHarness, itemStateLabel, mergeAssignments, own, registrationOf, splitItem, stamped, terms } from './model';
import type { StampedSnapshot } from './model';
import { matrixCheckbox, useAssignmentWrites } from './assignments';
import { useResource } from './hooks';
import { toast } from './toast';
import { cn } from '@/lib/utils';
import { CheckboxSwatch, Confirm, ErrorNotice, KindBadge, Loading, Nothing, Panel, PanelToolbar, Pending } from './shared';
import type { ItemView, MachineRow, StatusView, ToggleChange } from './types';

const loadMatrix = async (status: StatusView) => {
  const [fleet, localItems] = await Promise.all([status.mode === 'standalone' ? Promise.resolve({ baseline: status.report.baseline, machines: [{ id: status.machine_id, status: status.headline, connected: true, joined_at: 0, last_seen: null, scope: undefined, report: status.report, registration: null }] }) : api.fleet(), api.items()]);
  const byId = new Map<string, ItemView>();
  for (const item of localItems) {
    if (item.id == null) continue;
    const previous = byId.get(item.id);
    if (!previous || item.draft !== 'deleted') byId.set(item.id, { ...item, in_baseline: item.in_baseline || !!previous?.in_baseline });
    else if (item.in_baseline) byId.set(item.id, { ...previous, in_baseline: true });
  }
  for (const machine of fleet.machines) {
    for (const { item, id } of machine.report?.items ?? []) {
      if (!byId.has(id)) byId.set(id, { item, id, in_baseline: false, draft: null, unpushed: machine.report!.unpushed.includes(item), description: null });
    }
  }
  const items = [...byId.values()];
  // Each Machine loads on its own: one that fails (removed meanwhile, say) leaves the others usable.
  const results = await Promise.allSettled(fleet.machines.map(machine => loadAssignment(machine)));
  const ids = fleet.machines.map(machine => machine.id);
  const assignments: Record<string, StampedSnapshot> = Object.fromEntries(results.flatMap((result, i) => result.status === 'fulfilled' ? [[ids[i], result.value]] : []));
  const failed: Record<string, string> = Object.fromEntries(results.flatMap((result, i) => result.status === 'rejected' ? [[ids[i], errorMessage(result.reason)]] : []));
  return { fleet, items, assignments, failed };
};
type MatrixData = Awaited<ReturnType<typeof loadMatrix>>;
// Tagged with the registration it was requested for. A reply for another one (the Machine joined again meanwhile) is refused.
async function loadAssignment(machine: Pick<MachineRow, 'id' | 'registration' | 'joined_at'>) {
  const snapshot = await stamped(() => api.assignment(machine.id));
  if (machine.registration && snapshot.registration && snapshot.registration !== machine.registration) throw new Error(`${machine.id} joined the Fleet again meanwhile; refresh to load its Assignment`);
  return { ...snapshot, registration: registrationOf(machine) };
}
const registrationIn = (data: MatrixData | undefined, machine: string) => {
  const row = data?.fleet.machines.find(m => m.id === machine);
  return row && registrationOf(row);
};
const without = (record: Record<string, string>, key: string) => Object.fromEntries(Object.entries(record).filter(([id]) => id !== key));
// A Machine that left the Fleet, or joined again (a new registration starts its revisions over), keeps no cached snapshot.
const mergeMatrix = (previous: MatrixData | undefined, next: MatrixData): MatrixData => {
  const joined = new Map(next.fleet.machines.map(machine => [machine.id, registrationOf(machine)]));
  const cached = Object.fromEntries(Object.entries(previous?.assignments ?? {}).filter(([id, snapshot]) => joined.has(id) && snapshot.registration === joined.get(id)));
  return { ...next, assignments: mergeAssignments(cached, next.assignments) };
};
const sameSnapshot = (a: StampedSnapshot | undefined, b: StampedSnapshot) => !!a && a.revision === b.revision && JSON.stringify(a.assignment) === JSON.stringify(b.assignment);
const readIds = (key: string) => {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(key) ?? 'null');
    return Array.isArray(stored) && stored.every(id => typeof id === 'string') ? stored as string[] : null;
  } catch { return null; }
};
const saveIds = (key: string, ids: string[] | null) => {
  try { localStorage.setItem(key, JSON.stringify(ids)); } catch { /* Storage may be unavailable. */ }
};
const count = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
const verb = (enabled: boolean | null) => enabled === null ? 'Reset' : enabled ? 'Enabled' : 'Disabled';
const defaultHint = 'Changing a default for new Skills or Rules applies to items added later. Existing items keep their current value as an explicit Assignment.';
// Sticky header cells leave the collapsed table borders behind, so they draw their own edges.
const headEdge = 'sticky z-20 bg-muted shadow-[inset_1px_0_0_var(--color-border),inset_0_-1px_0_var(--color-border)]';
type MatrixProps = { revision: number; status: StatusView; openLog?: () => void; target?: { item: string; harness: string } };
export function Matrix(props: MatrixProps) {
  const { mode, server_url, machine_id } = props.status;
  // Revisions and optimistic writes belong to one assignment authority.
  return <AuthorityMatrix key={JSON.stringify([mode, server_url, machine_id])} {...props} />;
}
function AuthorityMatrix({ revision, status, target, openLog }: MatrixProps) {
  const [search, setSearch] = useState('');
  const [kindFilter, setKindFilter] = useState('all');
  const [machines, setMachines] = useState<string[] | null>(() => readIds('assignment-matrix-machines'));
  const [collapsed, setCollapsed] = useState<string[]>(() => readIds('assignment-matrix-collapsed') ?? []);
  useEffect(() => saveIds('assignment-matrix-machines', machines), [machines]);
  useEffect(() => saveIds('assignment-matrix-collapsed', collapsed), [collapsed]);
  useEffect(() => {
    if (!target) return;
    setSearch(''); setKindFilter('all');
    setMachines(old => old === null || old.includes(status.machine_id) ? old : [...old, status.machine_id]);
    setCollapsed(old => old.includes(status.machine_id) ? old.filter(id => id !== status.machine_id) : old);
  }, [target, status.machine_id]);
  const destination = useRef<HTMLButtonElement>(null);
  const visited = useRef<typeof target>(undefined);
  const [highlighted, setHighlighted] = useState(false);
  const standalone = status.mode === 'standalone';
  const latestStatus = useRef(status);
  useEffect(() => { latestStatus.current = status; }, [status]);
  const { mode, server_url, machine_id } = status;
  const load = useCallback(() => loadMatrix({ ...latestStatus.current, mode, server_url, machine_id }), [mode, server_url, machine_id]);
  const active = useRef(true);
  useEffect(() => {
    active.current = true;
    return () => { active.current = false; };
  }, []);
  const resource = useResource(load, revision, mergeMatrix);
  const { pending, errors, write, assignment } = useAssignmentWrites(resource);
  const latest = useRef(resource.data);
  useEffect(() => { latest.current = resource.data; }, [resource.data]);
  const [copyTarget, setCopyTarget] = useState<{ id: string; snapshot: StampedSnapshot }>();
  const [retrying, setRetrying] = useState<ReadonlySet<string>>(new Set());
  const retry = async (machine: string) => {
    setRetrying(old => new Set(old).add(machine));
    // A Machine that left the Fleet or joined again meanwhile stays out.
    const registration = registrationIn(latest.current, machine);
    const member = (previous: MatrixData) => registration !== undefined && registrationIn(previous, machine) === registration;
    try {
      const row = latest.current?.fleet.machines.find(m => m.id === machine);
      if (!row) return;
      const snapshot = await loadAssignment(row);
      if (active.current) resource.setData(previous => previous && member(previous) ? { ...previous,
        assignments: mergeAssignments(previous.assignments, { [machine]: snapshot }), failed: without(previous.failed, machine) } : previous);
    } catch (error) {
      if (active.current) resource.setData(previous => previous && member(previous) ? { ...previous, failed: { ...previous.failed, [machine]: errorMessage(error) } } : previous);
    } finally {
      if (active.current) setRetrying(old => { const next = new Set(old); next.delete(machine); return next; });
    }
  };
  useEffect(() => {
    if (!target || visited.current === target || !destination.current) return;
    visited.current = target;
    destination.current.scrollIntoView?.({ block: 'center', inline: 'nearest' });
    destination.current.focus();
    setHighlighted(true);
  }, [target, resource.data, machines, collapsed, search, kindFilter]);
  useEffect(() => {
    if (!highlighted) return;
    const timer = setTimeout(() => setHighlighted(false), 2500);
    return () => clearTimeout(timer);
  }, [highlighted]);
  const data = resource.data;
  if (!data) return <><ErrorNotice error={resource.error} />{resource.loading && <Loading />}</>;
  const items = data.items.filter(i => ['skill', 'rule'].includes(splitItem(i.item).kind)
    && (kindFilter === 'all' || splitItem(i.item).kind === kindFilter)
    && `${splitItem(i.item).name} ${i.description ?? ''}`.toLowerCase().includes(search.toLowerCase()));
  const selected = data.fleet.machines.filter(machine => standalone || machines === null || machines.includes(machine.id));
  // A Machine whose Assignment failed to load has no columns, only its error; nothing reads or writes its Assignment.
  const groups = selected.map(machine => ({ machine, loaded: !!own(data.assignments, machine.id), harnesses: [...new Set([
    ...(machine.report?.harnesses ?? []), ...Object.keys(own(data.assignments, machine.id)?.assignment.harnesses ?? {}),
  ])].sort(), collapsed: collapsed.includes(machine.id), offline: !machine.connected || machine.status === 'offline' })).filter(g => g.harnesses.length || !g.loaded);
  const offline = new Set(groups.filter(g => g.offline).map(g => g.machine.id));
  const columns = groups.flatMap(({ machine, harnesses, collapsed, loaded }) => collapsed || !loaded ? [] : harnesses.map(harness => ({ machine: machine.id, harness })));
  // A collapsed or unloaded Machine keeps one summary column so it can be expanded or retried.
  const segments = <T,>(column: (c: (typeof columns)[number]) => T, summary: (g: (typeof groups)[number]) => T) =>
    groups.flatMap(g => g.collapsed || !g.loaded ? [summary(g)] : g.harnesses.map(harness => column({ machine: g.machine.id, harness })));
  const expectedFor = (machine: string) => {
    const registration = data.fleet.machines.find(row => row.id === machine)?.registration;
    return { revision: own(data.assignments, machine)!.revision, ...(registration && { registration }) };
  };
  const reportedName = (item: ItemView, machine: string) =>
    data.fleet.machines.find(row => row.id === machine)?.report?.items?.find(entry => entry.id === item.id)?.item;
  const available = (item: ItemView, machine: string) =>
    item.in_baseline || !!reportedName(item, machine) || machine === status.machine_id;
  const cell = (item: ItemView, machine: string, harness: string) => {
    const state = cellState(assignment(machine), harness, item.item, item.id);
    const defaults = assignment(machine).harnesses[harness] ?? emptyHarness();
    const differs = state.explicit && state.enabled !== defaults[splitItem(item.item).kind === 'skill' ? 'new_skills_default' : 'new_rules_default'];
    const localOnly = !available(item, machine);
    const { kind, name } = splitItem(reportedName(item, machine) ?? item.item);
    const later = offline.has(machine) ? ` Applies when ${machine} reconnects.` : '';
    const reason = localOnly ? `${state.explicit ? `${terms.explicit} Assignment` : terms.inheritedLong}. ${itemStateLabel(item)}. This Machine has no report of the item yet.`
      : state.explicit ? `${terms.explicit} Assignment, ${differs ? 'overriding' : 'same as'} the Harness default. Use the reset button to inherit the default.${later}`
        : `${terms.inheritedLong}. Toggle to set an explicit Assignment.${later}`;
    const note = [differs && 'Overrides the Harness default.', later.trim()].filter(Boolean).join(' ');
    const focused = target?.item === item.item && target.harness === harness && machine === status.machine_id;
    return <TableCell key={`${machine}/${harness}`} data-highlighted={focused && highlighted || undefined} className={cn('border-l', focused && highlighted && 'bg-info/15 ring-2 ring-info ring-inset')}><div className="flex min-h-8 items-center justify-center gap-1">
      <Tooltip><TooltipTrigger render={<label className="inline-flex min-h-8 min-w-8 cursor-pointer items-center justify-center rounded-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring"
        tabIndex={localOnly ? 0 : undefined} role={localOnly ? 'group' : undefined}
        aria-label={localOnly ? `${item.item} in ${machine} / ${harness}, unavailable` : undefined} aria-description={localOnly ? reason : undefined} />}>
        <Checkbox ref={focused ? destination : undefined} className={matrixCheckbox(state.explicit, state.enabled)} checked={state.enabled} disabled={localOnly || !!pending.get(machine)}
          aria-label={`${item.item} in ${machine} / ${harness}, ${state.explicit ? 'explicit' : 'inherited'}`}
          aria-description={localOnly ? reason : note || undefined}
          onCheckedChange={enabled => void write(machine, [{ harness, kind: kind as 'skill' | 'rule', item_id: item.id!, name, enabled }], { expected: expectedFor(machine) })} />
      </TooltipTrigger><TooltipContent className="max-w-64">{reason}</TooltipContent></Tooltip>
      <span className="inline-flex size-6 shrink-0 items-center justify-center">{state.explicit && !localOnly && <Tooltip><TooltipTrigger render={<Button size="icon-xs" variant="ghost" disabled={!!pending.get(machine)}
        aria-label={`Use default for ${item.item} in ${machine} / ${harness}`}
        onClick={() => void write(machine, [{ harness, kind: kind as 'skill' | 'rule', item_id: item.id!, name, enabled: null }], { expected: expectedFor(machine) })} />}>
        <RotateCcw /></TooltipTrigger><TooltipContent>Inherit the Harness default</TooltipContent></Tooltip>}</span>
    </div></TableCell>;
  };
  // Writes once per Machine, then announces what the Server accepted.
  const bulk = async (selected: ItemView[], selectedColumns: typeof columns, enabled: boolean | null, title: (changes: number) => string) => {
    const accepted = await Promise.all(groups.filter(g => g.loaded).map(async ({ machine }) => {
      const changes: ToggleChange[] = selectedColumns.filter(c => c.machine === machine.id).flatMap(c =>
        selected.filter(item => available(item, machine.id)).map(item => {
          const { kind, name } = splitItem(reportedName(item, machine.id) ?? item.item);
          return { harness: c.harness, kind: kind as 'skill' | 'rule', item_id: item.id!, name, enabled };
        }));
      // The values shown, at the revision shown: the write applies only to that revision, so they are what Undo restores.
      const shown = own(data.assignments, machine.id)!;
      const previous = changes.map(change => {
        const state = cellState(shown.assignment, change.harness, `${change.kind}:${change.name}`, change.item_id);
        return { ...change, enabled: state.explicit ? state.enabled : null };
      });
      const expected = expectedFor(machine.id);
      const snapshot = await write(machine.id, changes, { expected });
      return snapshot ? { machine: machine.id, registration: registrationOf(machine), changes: changes.length, previous, snapshot } : null;
    }));
    if (!active.current) return;
    const done = accepted.filter(result => result !== null), later = done.filter(result => offline.has(result.machine)).map(result => result.machine);
    if (done.length) toast({ title: title(done.reduce((sum, result) => sum + result.changes, 0)),
      action: { label: 'Undo', run: () => {
        // Checked now: only a Machine still in the Fleet, under the same registration, whose Assignment is still what this update left.
        const now = active.current ? latest.current : undefined;
        const current = now ? done.filter(result => registrationIn(now, result.machine) === result.registration
          && sameSnapshot(own(now.assignments, result.machine), result.snapshot)) : [];
        if (!current.length) {
          toast({ title: 'Undo is no longer available', description: 'The Machine or Fleet changed since this bulk update.' });
          return;
        }
        const changed = done.length - current.length;
        void (async () => {
        // The authority checks again as it writes: a newer Assignment, or a new registration, refuses the Undo.
        const results = await Promise.all(current.map(async result => {
          let conflict = false;
          const row = now!.fleet.machines.find(m => m.id === result.machine);
          const expected = { revision: result.snapshot.revision, ...row?.registration && { registration: row.registration } };
          const restored = !!await write(result.machine, result.previous, { expected, onConflict: () => { conflict = true; } });
          return { ...result, restored, conflict };
        }));
        if (!active.current) return;
        const conflicts = results.filter(result => result.conflict).length;
        if (conflicts === results.length) {
          toast({ title: 'Undo is no longer available', description: 'The Assignment changed since this bulk update. The matrix shows the current one.' });
          return;
        }
        const restored = results.filter(result => result.restored).reduce((sum, result) => sum + result.changes, 0);
        const failed = results.some(result => !result.restored && !result.conflict);
        const kept = changed + conflicts;
        toast({ title: `Restored ${count(restored, 'Assignment', 'Assignments')}`, tone: !failed && !kept ? 'default' : 'destructive',
          description: [failed && 'Some Assignments could not be restored. Check the Machine errors and retry.',
            kept && `${count(kept, 'Machine', 'Machines')} changed since this bulk update and kept the current Assignment.`].filter(Boolean).join(' ') || undefined });
      })(); } },
      description: later.length ? `Applies when ${later.join(', ')} ${later.length === 1 ? 'reconnects' : 'reconnect'}.` : undefined });
  };
  const menu = (label: string, actions: { label: string; run: () => void }[], disabled: boolean) => <DropdownMenu>
    <DropdownMenuTrigger render={<Button size="icon-xs" variant="ghost" aria-label={label} disabled={disabled} />}><MoreHorizontal /></DropdownMenuTrigger>
    <DropdownMenuContent className="w-max"><DropdownMenuGroup>{actions.map(action =>
      <DropdownMenuItem key={action.label} onClick={action.run}>{action.label}</DropdownMenuItem>)}</DropdownMenuGroup></DropdownMenuContent>
  </DropdownMenu>;
  const columnBulk = (c: (typeof columns)[number], enabled: boolean) => void bulk(items, [c], enabled, n => `${verb(enabled)} ${count(n, 'item', 'items')} in ${c.machine} / ${c.harness}`);
  const rowBulk = (item: ItemView, enabled: boolean | null) => void bulk([item], columns, enabled,
    n => `${verb(enabled)} ${splitItem(item.item).name}${enabled === null ? ' to the default' : ''} in ${count(n, 'Harness', 'Harnesses')}`);
  const saving = [...pending.keys()];
  return <>
    <ErrorNotice error={resource.error} />{[...errors].map(([id, error]) =>
      <ErrorNotice key={id} error={`${id}: ${error}`} />)}
    {/* A failed refresh keeps the last snapshot usable; a first load that failed shows in the Machine's column. */}
    {Object.entries(data.failed).filter(([id]) => own(data.assignments, id)).map(([id, error]) =>
      <ErrorNotice key={`refresh-${id}`} title="Could not refresh Assignment" error={`${id}: ${error}`} />)}
    <Panel>
    {copyTarget && <CopyAssignment target={copyTarget.id}
      expected={{ revision: copyTarget.snapshot.revision, registration: copyTarget.snapshot.registration }}
      targetHarnesses={Object.keys(copyTarget.snapshot.assignment.harnesses).sort()}
      sources={data.fleet.machines.flatMap(machine => {
        const snapshot = own(data.assignments, machine.id);
        return machine.id !== copyTarget.id && snapshot ? [{ id: machine.id, harnesses: Object.keys(snapshot.assignment.harnesses).sort() }] : [];
      })}
      close={() => setCopyTarget(undefined)}
      refreshed={resource.refresh}
      copied={update => {
        setCopyTarget(undefined);
        void retry(copyTarget.id);
        toast({ title: `Copied Assignment to ${copyTarget.id}`,
          description: `${count(update.entries.length, 'change', 'changes')}. Undo individual changes in the Assignment log.`,
          action: openLog && { label: 'View log', run: openLog } });
      }} />}

    <PanelToolbar>
      <Input aria-label="Search items" placeholder={terms.search} value={search} onChange={event => setSearch(event.target.value)} className="h-8 w-52" />
      <NativeSelect aria-label="Item kind" value={kindFilter} onChange={event => setKindFilter(event.target.value)}>
        <option value="all">All</option><option value="skill">Skills</option><option value="rule">Rules</option>
      </NativeSelect>
      {!standalone && <DropdownMenu><DropdownMenuTrigger render={<Button variant="outline" size="sm" className="h-8 bg-card" />}>Machines ({selected.length})</DropdownMenuTrigger>
        <DropdownMenuContent><DropdownMenuGroup>
          <DropdownMenuItem onClick={() => setMachines(null)}>All Machines</DropdownMenuItem>
          <DropdownMenuItem onClick={() => setMachines([status.machine_id])}>Only this Machine</DropdownMenuItem>
        </DropdownMenuGroup><DropdownMenuSeparator /><DropdownMenuGroup>{data.fleet.machines.map(machine => <DropdownMenuCheckboxItem key={machine.id}
          checked={standalone || machines === null || machines.includes(machine.id)} closeOnClick={false}
          onCheckedChange={checked => setMachines(old => {
            const current = old ?? data.fleet.machines.map(m => m.id);
            return checked ? [...current, machine.id] : current.filter(id => id !== machine.id);
          })}>{machine.id}</DropdownMenuCheckboxItem>)}</DropdownMenuGroup></DropdownMenuContent>
      </DropdownMenu>}
      <span role="status" className="inline-flex items-center gap-1.5 text-xs">{saving.length > 0 && <Pending pending>Saving Assignment for {saving.join(', ')}…</Pending>}</span>
      <div className="ml-auto flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
        <span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit enabled /><CheckboxSwatch explicit enabled={false} />{terms.explicit}</span>
        <span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit={false} enabled /><CheckboxSwatch explicit={false} enabled={false} />{terms.inheritedLong}</span>
      </div>
    </PanelToolbar>
    {!standalone && <OrphanToggles revision={revision} machine={data.fleet.machines.find(row => row.id === status.machine_id)} changed={() => void resource.refresh()} />}
    <span id="default-hint" className="sr-only">{defaultHint}</span>
    {groups.length > 0 && <div className="overflow-hidden [&_[data-slot=table-container]]:max-h-[max(24rem,calc(100svh-9rem))] [&_[data-slot=table-container]]:scroll-pt-[4.5rem]"><Table aria-label="Assignment matrix" stickyFirstColumn>
      <TableHeader><TableRow className="bg-muted hover:bg-muted">
        <TableHead rowSpan={2} scope="col" className="sticky top-0 left-0 z-30! min-w-48 bg-muted align-bottom shadow-[inset_0_-1px_0_var(--color-border)] md:min-w-64">Skill / Rule</TableHead>
        {groups.map(g => <TableHead key={g.machine.id} scope="colgroup" colSpan={g.collapsed || !g.loaded ? 1 : g.harnesses.length}
          aria-label={[g.machine.id, g.machine.id === status.machine_id && 'This Machine', g.offline && 'Offline'].filter(Boolean).join(', ')} className={cn(headEdge, 'top-0 pl-1')}><div className="flex items-center gap-1.5">
          {g.loaded ? <Button size="icon-xs" variant="ghost" className="text-muted-foreground aria-expanded:bg-transparent aria-expanded:text-muted-foreground aria-expanded:hover:bg-muted" aria-expanded={!g.collapsed} aria-label={`${g.machine.id} Harnesses`}
            onClick={() => setCollapsed(old => g.collapsed ? old.filter(id => id !== g.machine.id) : [...old, g.machine.id])}>
            <ChevronRight className={cn('transition-transform', !g.collapsed && 'rotate-90')} /></Button> : <span aria-hidden className="size-6" />}
          {g.machine.id}{g.machine.id === status.machine_id && <Badge variant="outline" className="border-primary/20 bg-brand-soft text-brand-ink">This Machine</Badge>}
          {!standalone && g.loaded && data.fleet.machines.length > 1 &&
            (g.machine.id === status.machine_id || data.fleet.machines.find(machine => machine.id === status.machine_id)?.scope !== 'self') &&
            menu(`Actions for ${g.machine.id}`, [{ label: 'Copy Assignment from…', run: () => {
              const snapshot = own(data.assignments, g.machine.id);
              if (snapshot) setCopyTarget({ id: g.machine.id, snapshot });
            } }], !!pending.get(g.machine.id))}
          {g.offline && <span className="inline-flex items-center gap-1.5 text-xs font-normal text-muted-foreground"><span aria-hidden className="size-2 rounded-full border-[1.5px] border-muted-foreground" />Offline</span>}
        </div></TableHead>)}
      </TableRow><TableRow className="bg-muted hover:bg-muted">
        {segments(c => <TableHead key={`${c.machine}/${c.harness}`} scope="col" className={cn(headEdge, 'top-10 h-8 min-w-28 font-mono text-xs font-normal text-muted-foreground')}><div className="flex items-center justify-between gap-1">{c.harness}{menu(`Actions for ${c.machine} / ${c.harness}`, [
          { label: 'Enable all visible items', run: () => columnBulk(c, true) },
          { label: 'Disable all visible items', run: () => columnBulk(c, false) },
        ], !!pending.get(c.machine) || !items.some(item => available(item, c.machine)))}</div></TableHead>,
        g => g.loaded ? <TableHead key={`${g.machine.id}/collapsed`} scope="col" className={cn(headEdge, 'top-10 h-8 text-xs font-normal text-muted-foreground')}>{count(g.harnesses.length, 'Harness', 'Harnesses')}</TableHead>
          : <TableHead key={`${g.machine.id}/unavailable`} scope="col" className={cn(headEdge, 'top-10 h-auto min-w-56 py-1.5 text-xs font-normal whitespace-normal')}>
            <div role="alert" className="flex items-start justify-between gap-2"><span className="min-w-0 break-words text-destructive-foreground">Assignment not loaded: {own(data.failed, g.machine.id)}</span>
              <Button size="xs" variant="outline" className="shrink-0" disabled={retrying.has(g.machine.id)} aria-label={`Retry loading the Assignment of ${g.machine.id}`}
                onClick={() => void retry(g.machine.id)}><Pending pending={retrying.has(g.machine.id)}>Retry</Pending></Button></div></TableHead>)}
      </TableRow></TableHeader>
      <TableBody>
        {(['skill', 'rule'] as const).map(kind => <TableRow key={kind} className={cn('bg-muted/40 hover:bg-muted/40', kind === 'rule' && 'border-b-2')}>
          <TableHead scope="row" className="sticky left-0 z-10 h-auto bg-muted py-2 font-normal whitespace-normal"><div className="flex items-center gap-1">
            <span className="text-sm">Default for new {kind === 'skill' ? 'Skills' : 'Rules'}</span>
            <Tooltip><TooltipTrigger render={<Button size="icon-xs" variant="ghost" className="text-muted-foreground" aria-label={`About the default for new ${kind}s`} />}><Info /></TooltipTrigger>
              <TooltipContent className="max-w-64">{defaultHint}</TooltipContent></Tooltip></div></TableHead>
          {segments(c => <TableCell key={`${c.machine}/${c.harness}`} className="border-l"><label className="flex min-h-8 cursor-pointer items-center justify-center pr-7">
            <Switch.Root className="inline-flex h-[18px] w-8 shrink-0 cursor-pointer items-center rounded-full bg-control-border/45 p-0.5 transition-colors outline-none focus-visible:ring-3 focus-visible:ring-ring data-checked:bg-foreground/75 data-disabled:cursor-not-allowed data-disabled:opacity-50"
              checked={(assignment(c.machine).harnesses[c.harness] ?? emptyHarness())[kind === 'skill' ? 'new_skills_default' : 'new_rules_default']}
              disabled={!!pending.get(c.machine)} aria-label={`Default for new ${kind}s in ${c.machine} / ${c.harness}`} aria-describedby="default-hint"
              onCheckedChange={enabled => void write(c.machine, [{ harness: c.harness,
                kind: kind === 'skill' ? 'new_skills_default' : 'new_rules_default', enabled }], { expected: expectedFor(c.machine) })}>
              <Switch.Thumb className="size-3.5 rounded-full bg-card shadow-sm transition-transform data-checked:translate-x-3.5" /></Switch.Root></label></TableCell>,
          g => <TableCell key={`${g.machine.id}/${g.loaded ? 'collapsed' : 'unavailable'}`} className="border-l" />)}
        </TableRow>)}
        {items.map(item => <TableRow key={item.id!}>
          <TableHead scope="row" className="sticky left-0 z-10 h-14 min-w-60 max-w-80 bg-background py-2 pr-9 font-normal whitespace-normal">
            <div className="flex flex-wrap items-center gap-2"><span className="text-sm font-medium" title={item.id ?? undefined}>{splitItem(item.item).name}</span><KindBadge kind={splitItem(item.item).kind} />
              {new Set(columns.map(c => cellState(assignment(c.machine), c.harness, item.item, item.id).enabled)).size > 1 &&
                <Tooltip><TooltipTrigger render={<Badge variant="secondary" tabIndex={0} />}>Mixed</TooltipTrigger><TooltipContent>Enabled in some visible Harnesses and disabled in others.</TooltipContent></Tooltip>}
              <span className="absolute top-1/2 right-2 -translate-y-1/2">{menu(`Actions for ${item.item}`, [
                { label: 'Enable in all visible Harnesses', run: () => rowBulk(item, true) },
                { label: 'Disable in all visible Harnesses', run: () => rowBulk(item, false) },
                { label: 'Reset all to default', run: () => rowBulk(item, null) },
              ], !columns.length || columns.some(c => pending.get(c.machine)) || !columns.some(c => available(item, c.machine)))}</span>
              {itemStateLabel(item) && <Badge variant="info">{itemStateLabel(item)}</Badge>}</div>
            {item.description && <p className="mt-1 text-xs text-muted-foreground">{item.description}</p>}
          </TableHead>
          {segments(c => cell(item, c.machine, c.harness), g => {
            if (!g.loaded) return <TableCell key={`${g.machine.id}/unavailable`} className="border-l" />;
            const on = g.harnesses.filter(harness => cellState(assignment(g.machine.id), harness, item.item, item.id).enabled).length;
            return <TableCell key={`${g.machine.id}/collapsed`} className="border-l text-center text-xs text-muted-foreground tabular-nums">
              <span aria-hidden>{on}/{g.harnesses.length}</span><span className="sr-only">Enabled in {on} of {count(g.harnesses.length, 'Harness', 'Harnesses')}</span></TableCell>;
          })}
        </TableRow>)}
      </TableBody>
    </Table></div>}
    {!items.length && <Nothing title={search || kindFilter !== 'all' ? 'No matching items' : 'No Skills or Rules'}>
      {search || kindFilter !== 'all' ? 'Change the search or kind filter to see more items.' : "Add a Skill or Rule to this Machine's Draft to get started."}</Nothing>}
    {!groups.length && <Nothing title={selected.length ? 'No Harnesses' : 'No Machines selected'}>
      {selected.length ? 'No Harnesses reported. Check the Machine configuration in Settings.' : 'Choose Machines above to show their Harnesses.'}</Nothing>}
    </Panel>
  </>;
}


function OrphanToggles({ revision, machine, changed }: { revision: number; machine?: MachineRow; changed: () => void }) {
  const resource = useResource(api.orphans, revision);
  const count = resource.data?.orphans.length ?? 0;
  if (count === 0) return null;
  const self = machine?.scope === 'self';
  const own = resource.data?.orphans.filter(orphan => orphan.machine === machine?.id).length ?? 0;
  return <PanelToolbar>
    <span className="text-sm">{count} {count === 1 ? 'toggle' : 'toggles'} for items that no longer exist</span>
    {(!self || own > 0) && <Confirm label="Clear" title="Clear orphan toggles?"
      description={self ? 'Clear orphan toggles for this Machine.' : 'Clear orphan toggles across the Fleet.'}
      confirmLabel="Clear" triggerVariant="outline" action={() => api.pruneOrphans()}
      onDone={() => { void resource.refresh(); changed(); }} />}
  </PanelToolbar>;
}
