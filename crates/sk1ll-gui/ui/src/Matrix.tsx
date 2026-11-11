import { useEffect, useRef, useState } from 'react';
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
import { api } from './api';
import { cellState, emptyHarness, itemStateLabel, mergeAssignments, splitItem, terms } from './model';
import { matrixCheckbox, useAssignmentWrites } from './assignments';
import { useResource } from './hooks';
import { toast } from './toast';
import { cn } from '@/lib/utils';
import { CheckboxSwatch, ErrorNotice, KindBadge, Loading, Nothing, Panel, PanelToolbar, Pending } from './shared';
import type { CachedAssignmentSnapshot as AssignmentSnapshot, ItemView, StatusView, ToggleChange } from './types';

const loadMatrix = async () => {
  const [fleet, items] = await Promise.all([api.fleet(), api.items()]);
  const assignments: Record<string, AssignmentSnapshot> = Object.fromEntries(await Promise.all(fleet.machines.map(async machine =>
    [machine.id, await api.assignment(machine.id)] as const)));
  return { fleet, items, assignments };
};
type MatrixData = Awaited<ReturnType<typeof loadMatrix>>;
const mergeMatrix = (previous: MatrixData | undefined, next: MatrixData): MatrixData => ({
  ...next, assignments: mergeAssignments(previous?.assignments ?? {}, next.assignments),
});
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
export function Matrix({ revision, status, target }: { revision: number; status: StatusView; target?: { item: string; harness: string } }) {
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
  const resource = useResource(loadMatrix, revision, mergeMatrix);
  const { pending, errors, write, assignment } = useAssignmentWrites(resource);
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
  const selected = data.fleet.machines.filter(machine => machines === null || machines.includes(machine.id));
  const groups = selected.map(machine => ({ machine, harnesses: [...new Set([
    ...(machine.report?.harnesses ?? []), ...Object.keys(data.assignments[machine.id].assignment.harnesses),
  ])].sort(), collapsed: collapsed.includes(machine.id), offline: !machine.connected || machine.status === 'offline' })).filter(g => g.harnesses.length);
  const offline = new Set(groups.filter(g => g.offline).map(g => g.machine.id));
  const columns = groups.flatMap(({ machine, harnesses, collapsed }) => collapsed ? [] : harnesses.map(harness => ({ machine: machine.id, harness })));
  // A collapsed Machine keeps one summary column so it can be expanded again.
  const segments = <T,>(column: (c: (typeof columns)[number]) => T, summary: (g: (typeof groups)[number]) => T) =>
    groups.flatMap(g => g.collapsed ? [summary(g)] : g.harnesses.map(harness => column({ machine: g.machine.id, harness })));
  const cell = (item: ItemView, machine: string, harness: string) => {
    const state = cellState(assignment(machine), harness, item.item);
    const defaults = assignment(machine).harnesses[harness] ?? emptyHarness();
    const differs = state.explicit && state.enabled !== defaults[splitItem(item.item).kind === 'skill' ? 'new_skills_default' : 'new_rules_default'];
    const localOnly = !item.in_baseline && machine !== status.machine_id;
    const { kind, name } = splitItem(item.item);
    const later = offline.has(machine) ? ` Applies when ${machine} reconnects.` : '';
    const reason = localOnly ? `${state.explicit ? `${terms.explicit} Assignment` : terms.inheritedLong}. ${itemStateLabel(item)}. This item can only be assigned on this Machine until it reaches the Baseline.`
      : state.explicit ? `${terms.explicit} Assignment, ${differs ? 'overriding' : 'same as'} the Harness default. Use the reset button to inherit the default.${later}`
        : `${terms.inheritedLong}. Toggle to set an explicit Assignment.${later}`;
    const note = [differs && 'Overrides the Harness default.', later.trim()].filter(Boolean).join(' ');
    const focused = target?.item === item.item && target.harness === harness && machine === status.machine_id;
    return <TableCell key={`${machine}/${harness}`} data-highlighted={focused && highlighted || undefined} className={cn('border-l', focused && highlighted && 'bg-info/15 ring-2 ring-info ring-inset')}><div className="flex min-h-8 items-center justify-center gap-1">
      <Tooltip><TooltipTrigger render={<label className="inline-flex min-h-8 min-w-8 cursor-pointer items-center justify-center rounded-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring"
        tabIndex={localOnly ? 0 : undefined} role={localOnly ? 'group' : undefined}
        aria-label={localOnly ? `${item.item} in ${machine} / ${harness}, unavailable` : undefined} aria-description={localOnly ? reason : undefined} />}>
        <Checkbox ref={focused ? destination : undefined} className={matrixCheckbox(state.explicit, state.enabled)} checked={state.enabled} disabled={localOnly || pending[machine]}
          aria-label={`${item.item} in ${machine} / ${harness}, ${state.explicit ? 'explicit' : 'inherited'}`}
          aria-description={localOnly ? reason : note || undefined}
          onCheckedChange={enabled => void write(machine, [{ harness, kind: kind as 'skill' | 'rule', name, enabled }])} />
      </TooltipTrigger><TooltipContent className="max-w-64">{reason}</TooltipContent></Tooltip>
      <span className="inline-flex size-6 shrink-0 items-center justify-center">{state.explicit && !localOnly && <Tooltip><TooltipTrigger render={<Button size="icon-xs" variant="ghost" disabled={pending[machine]}
        aria-label={`Use default for ${item.item} in ${machine} / ${harness}`}
        onClick={() => void write(machine, [{ harness, kind: kind as 'skill' | 'rule', name, enabled: null }])} />}>
        <RotateCcw /></TooltipTrigger><TooltipContent>Inherit the Harness default</TooltipContent></Tooltip>}</span>
    </div></TableCell>;
  };
  // Writes once per Machine, then announces what the Server accepted.
  const bulk = async (selected: ItemView[], selectedColumns: typeof columns, enabled: boolean | null, title: (changes: number) => string) => {
    const accepted = await Promise.all(groups.map(async ({ machine }) => {
      const changes: ToggleChange[] = selectedColumns.filter(c => c.machine === machine.id).flatMap(c =>
        selected.filter(item => item.in_baseline || machine.id === status.machine_id).map(item => {
          const { kind, name } = splitItem(item.item);
          return { harness: c.harness, kind: kind as 'skill' | 'rule', name, enabled };
        }));
      const previous = changes.map(change => {
        const state = cellState(assignment(machine.id), change.harness, `${change.kind}:${change.name}`);
        return { ...change, enabled: state.explicit ? state.enabled : null };
      });
      return await write(machine.id, changes) ? { machine: machine.id, changes: changes.length, previous } : null;
    }));
    const done = accepted.filter(result => result !== null), later = done.filter(result => offline.has(result.machine)).map(result => result.machine);
    if (done.length) toast({ title: title(done.reduce((sum, result) => sum + result.changes, 0)),
      action: { label: 'Undo', run: () => { void (async () => {
        const results = await Promise.all(done.map(async result => ({ ...result, restored: await write(result.machine, result.previous) })));
        const restored = results.filter(result => result.restored).reduce((sum, result) => sum + result.changes, 0);
        toast({ title: `Restored ${count(restored, 'Assignment', 'Assignments')}`, tone: results.every(result => result.restored) ? 'default' : 'destructive',
          description: results.some(result => !result.restored) ? 'Some Assignments could not be restored. Check the Machine errors and retry.' : undefined });
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
  const saving = Object.entries(pending).filter(([, value]) => value).map(([id]) => id);
  return <>
    <ErrorNotice error={resource.error} />{Object.entries(errors).filter(([, value]) => value).map(([id, error]) =>
      <ErrorNotice key={id} error={`${id}: ${error}`} />)}
    <Panel>
    <PanelToolbar>
      <Input aria-label="Search items" placeholder={terms.search} value={search} onChange={event => setSearch(event.target.value)} className="h-8 w-52" />
      <NativeSelect aria-label="Item kind" value={kindFilter} onChange={event => setKindFilter(event.target.value)}>
        <option value="all">All</option><option value="skill">Skills</option><option value="rule">Rules</option>
      </NativeSelect>
      <DropdownMenu><DropdownMenuTrigger render={<Button variant="outline" size="sm" className="h-8 bg-card" />}>Machines ({selected.length})</DropdownMenuTrigger>
        <DropdownMenuContent><DropdownMenuGroup>
          <DropdownMenuItem onClick={() => setMachines(null)}>All Machines</DropdownMenuItem>
          <DropdownMenuItem onClick={() => setMachines([status.machine_id])}>Only this Machine</DropdownMenuItem>
        </DropdownMenuGroup><DropdownMenuSeparator /><DropdownMenuGroup>{data.fleet.machines.map(machine => <DropdownMenuCheckboxItem key={machine.id}
          checked={machines === null || machines.includes(machine.id)} closeOnClick={false}
          onCheckedChange={checked => setMachines(old => {
            const current = old ?? data.fleet.machines.map(m => m.id);
            return checked ? [...current, machine.id] : current.filter(id => id !== machine.id);
          })}>{machine.id}</DropdownMenuCheckboxItem>)}</DropdownMenuGroup></DropdownMenuContent>
      </DropdownMenu>
      <span role="status" className="inline-flex items-center gap-1.5 text-xs">{saving.length > 0 && <Pending pending>Saving Assignment for {saving.join(', ')}…</Pending>}</span>
      <div className="ml-auto flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
        <span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit enabled /><CheckboxSwatch explicit enabled={false} />{terms.explicit}</span>
        <span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit={false} enabled /><CheckboxSwatch explicit={false} enabled={false} />{terms.inheritedLong}</span>
      </div>
    </PanelToolbar>
    <span id="default-hint" className="sr-only">{defaultHint}</span>
    {groups.length > 0 && <div className="overflow-hidden [&_[data-slot=table-container]]:max-h-[max(24rem,calc(100svh-9rem))] [&_[data-slot=table-container]]:scroll-pt-[4.5rem]"><Table aria-label="Assignment matrix" stickyFirstColumn>
      <TableHeader><TableRow className="bg-muted hover:bg-muted">
        <TableHead rowSpan={2} scope="col" className="sticky top-0 left-0 z-30! min-w-48 bg-muted align-bottom shadow-[inset_0_-1px_0_var(--color-border)] md:min-w-64">Skill / Rule</TableHead>
        {groups.map(g => <TableHead key={g.machine.id} scope="colgroup" colSpan={g.collapsed ? 1 : g.harnesses.length}
          aria-label={[g.machine.id, g.machine.id === status.machine_id && 'This Machine', g.offline && 'Offline'].filter(Boolean).join(', ')} className={cn(headEdge, 'top-0 pl-1')}><div className="flex items-center gap-1.5">
          <Button size="icon-xs" variant="ghost" className="text-muted-foreground aria-expanded:bg-transparent aria-expanded:text-muted-foreground aria-expanded:hover:bg-muted" aria-expanded={!g.collapsed} aria-label={`${g.machine.id} Harnesses`}
            onClick={() => setCollapsed(old => g.collapsed ? old.filter(id => id !== g.machine.id) : [...old, g.machine.id])}>
            <ChevronRight className={cn('transition-transform', !g.collapsed && 'rotate-90')} /></Button>
          {g.machine.id}{g.machine.id === status.machine_id && <Badge variant="outline" className="border-primary/20 bg-brand-soft text-brand-ink">This Machine</Badge>}
          {g.offline && <span className="inline-flex items-center gap-1.5 text-xs font-normal text-muted-foreground"><span aria-hidden className="size-2 rounded-full border-[1.5px] border-muted-foreground" />Offline</span>}
        </div></TableHead>)}
      </TableRow><TableRow className="bg-muted hover:bg-muted">
        {segments(c => <TableHead key={`${c.machine}/${c.harness}`} scope="col" className={cn(headEdge, 'top-10 h-8 min-w-28 font-mono text-xs font-normal text-muted-foreground')}><div className="flex items-center justify-between gap-1">{c.harness}{menu(`Actions for ${c.machine} / ${c.harness}`, [
          { label: 'Enable all visible items', run: () => columnBulk(c, true) },
          { label: 'Disable all visible items', run: () => columnBulk(c, false) },
        ], !!pending[c.machine] || !items.some(item => item.in_baseline || c.machine === status.machine_id))}</div></TableHead>,
        g => <TableHead key={`${g.machine.id}/collapsed`} scope="col" className={cn(headEdge, 'top-10 h-8 text-xs font-normal text-muted-foreground')}>{count(g.harnesses.length, 'Harness', 'Harnesses')}</TableHead>)}
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
              disabled={pending[c.machine]} aria-label={`Default for new ${kind}s in ${c.machine} / ${c.harness}`} aria-describedby="default-hint"
              onCheckedChange={enabled => void write(c.machine, [{ harness: c.harness,
                kind: kind === 'skill' ? 'new_skills_default' : 'new_rules_default', enabled }])}>
              <Switch.Thumb className="size-3.5 rounded-full bg-card shadow-sm transition-transform data-checked:translate-x-3.5" /></Switch.Root></label></TableCell>,
          g => <TableCell key={`${g.machine.id}/collapsed`} className="border-l" />)}
        </TableRow>)}
        {items.map(item => <TableRow key={item.item}>
          <TableHead scope="row" className="sticky left-0 z-10 h-14 min-w-60 max-w-80 bg-background py-2 pr-9 font-normal whitespace-normal">
            <div className="flex flex-wrap items-center gap-2"><span className="text-sm font-medium">{splitItem(item.item).name}</span><KindBadge kind={splitItem(item.item).kind} />
              {new Set(columns.map(c => cellState(assignment(c.machine), c.harness, item.item).enabled)).size > 1 &&
                <Tooltip><TooltipTrigger render={<Badge variant="secondary" tabIndex={0} />}>Mixed</TooltipTrigger><TooltipContent>Enabled in some visible Harnesses and disabled in others.</TooltipContent></Tooltip>}
              <span className="absolute top-1/2 right-2 -translate-y-1/2">{menu(`Actions for ${item.item}`, [
                { label: 'Enable in all visible Harnesses', run: () => rowBulk(item, true) },
                { label: 'Disable in all visible Harnesses', run: () => rowBulk(item, false) },
                { label: 'Reset all to default', run: () => rowBulk(item, null) },
              ], !columns.length || columns.some(c => pending[c.machine]) || !columns.some(c => item.in_baseline || c.machine === status.machine_id))}</span>
              {itemStateLabel(item) && <Badge variant="info">{itemStateLabel(item)}</Badge>}</div>
            {item.description && <p className="mt-1 text-xs text-muted-foreground">{item.description}</p>}
          </TableHead>
          {segments(c => cell(item, c.machine, c.harness), g => {
            const on = g.harnesses.filter(harness => cellState(assignment(g.machine.id), harness, item.item).enabled).length;
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
