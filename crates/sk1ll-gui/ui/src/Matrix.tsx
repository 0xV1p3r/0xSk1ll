import { useEffect, useRef, useState } from 'react';
import { MoreHorizontal, RotateCcw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { NativeSelect } from '@/components/ui/native-select';
import { Input } from '@/components/ui/input';
import { DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuCheckboxItem } from '@/components/ui/dropdown-menu';
import { Checkbox } from '@/components/ui/checkbox';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api } from './api';
import { cellState, emptyHarness, itemStateLabel, mergeAssignments, splitItem } from './model';
import { matrixCheckbox, useAssignmentWrites } from './assignments';
import { useResource } from './hooks';
import { cn } from '@/lib/utils';
import { CheckboxSwatch, ErrorNotice, KindBadge, Loading, Nothing, Pending } from './shared';
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
export function Matrix({ revision, status, target }: { revision: number; status: StatusView; target?: { item: string; harness: string } }) {
  const [search, setSearch] = useState('');
  const [kindFilter, setKindFilter] = useState('all');
  const [machines, setMachines] = useState<string[] | null>(() => {
    try {
      const stored: unknown = JSON.parse(localStorage.getItem('assignment-matrix-machines') ?? 'null');
      return Array.isArray(stored) && stored.every(id => typeof id === 'string') ? stored : null;
    } catch { return null; }
  });
  useEffect(() => {
    try { localStorage.setItem('assignment-matrix-machines', JSON.stringify(machines)); } catch { /* Storage may be unavailable. */ }
  }, [machines]);
  useEffect(() => {
    if (!target) return;
    setSearch(''); setKindFilter('all');
    setMachines(old => old === null || old.includes(status.machine_id) ? old : [...old, status.machine_id]);
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
  }, [target, resource.data, machines, search, kindFilter]);
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
  const groups = data.fleet.machines.filter(machine => machines === null || machines.includes(machine.id)).map(machine => ({ machine, harnesses: [...new Set([
    ...(machine.report?.harnesses ?? []), ...Object.keys(data.assignments[machine.id].assignment.harnesses),
  ])].sort() }));
  const columns = groups.flatMap(({ machine, harnesses }) => harnesses.map(harness => ({ machine: machine.id, harness })));
  const cell = (item: ItemView, machine: string, harness: string) => {
    const state = cellState(assignment(machine), harness, item.item);
    const defaults = assignment(machine).harnesses[harness] ?? emptyHarness();
    const differs = state.explicit && state.enabled !== defaults[splitItem(item.item).kind === 'skill' ? 'new_skills_default' : 'new_rules_default'];
    const localOnly = !item.in_baseline && machine !== status.machine_id;
    const { kind, name } = splitItem(item.item);
    const reason = localOnly ? `${state.explicit ? 'Explicit Assignment' : 'Inherited from the Harness default'}. ${itemStateLabel(item)}. This item can only be assigned on this Machine until it reaches the Baseline.`
      : state.explicit ? 'Explicit Assignment. Use the reset button to inherit the Harness default.'
        : 'Inherited from the Harness default. Toggle to set an explicit Assignment.';
    const focused = target?.item === item.item && target.harness === harness && machine === status.machine_id;
    return <TableCell key={`${machine}/${harness}`} data-diverges={differs || undefined} data-highlighted={focused && highlighted || undefined} className={cn('border-l', differs && 'bg-warning/8', focused && highlighted && 'bg-info/15 ring-2 ring-info ring-inset')}><div className="flex min-h-8 items-center justify-center gap-1">
      <Tooltip><TooltipTrigger render={<label className="inline-flex min-h-8 min-w-8 cursor-pointer items-center justify-center rounded-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring"
        tabIndex={localOnly ? 0 : undefined} role={localOnly ? 'group' : undefined}
        aria-label={localOnly ? `${item.item} in ${machine} / ${harness}, unavailable` : undefined} aria-description={localOnly ? reason : undefined} />}>
        <Checkbox ref={focused ? destination : undefined} className={matrixCheckbox(state.explicit, state.enabled)} checked={state.enabled} disabled={localOnly || pending[machine]}
          aria-label={`${item.item} in ${machine} / ${harness}, ${state.explicit ? 'explicit' : 'inherited'}`}
          aria-description={localOnly ? reason : undefined}
          onCheckedChange={enabled => void write(machine, [{ harness, kind: kind as 'skill' | 'rule', name, enabled }])} />
      </TooltipTrigger><TooltipContent className="max-w-64">{reason}</TooltipContent></Tooltip>
      <span className="inline-flex size-6 shrink-0 items-center justify-center">{state.explicit && !localOnly && <Tooltip><TooltipTrigger render={<Button size="icon-xs" variant="ghost" disabled={pending[machine]}
        aria-label={`Use default for ${item.item} in ${machine} / ${harness}`}
        onClick={() => void write(machine, [{ harness, kind: kind as 'skill' | 'rule', name, enabled: null }])} />}>
        <RotateCcw /></TooltipTrigger><TooltipContent>Inherit the Harness default</TooltipContent></Tooltip>}</span>
    </div></TableCell>;
  };
  const bulk = (selected: ItemView[], selectedColumns: typeof columns, enabled: boolean | null) => {
    for (const { machine } of groups) {
      const changes: ToggleChange[] = selectedColumns.filter(c => c.machine === machine.id).flatMap(c =>
        selected.filter(item => item.in_baseline || machine.id === status.machine_id).map(item => {
          const { kind, name } = splitItem(item.item);
          return { harness: c.harness, kind: kind as 'skill' | 'rule', name, enabled };
        }));
      void write(machine.id, changes);
    }
  };
  const menu = (label: string, actions: { label: string; run: () => void }[], disabled: boolean) => <DropdownMenu>
    <DropdownMenuTrigger render={<Button size="icon-xs" variant="ghost" aria-label={label} disabled={disabled} />}><MoreHorizontal /></DropdownMenuTrigger>
    <DropdownMenuContent className="w-max"><DropdownMenuGroup>{actions.map(action =>
      <DropdownMenuItem key={action.label} onClick={action.run}>{action.label}</DropdownMenuItem>)}</DropdownMenuGroup></DropdownMenuContent>
  </DropdownMenu>;
  const saving = Object.entries(pending).filter(([, value]) => value).map(([id]) => id);
  return <>
    <ErrorNotice error={resource.error} />{Object.entries(errors).filter(([, value]) => value).map(([id, error]) =>
      <ErrorNotice key={id} error={`${id}: ${error}`} />)}
    <div className="min-w-0 overflow-hidden rounded-xl border bg-card">
    <div className="flex flex-wrap items-center gap-2 border-b px-3.5 py-2.5">
      <Input aria-label="Search items" placeholder="Search Skills and Rules" value={search} onChange={event => setSearch(event.target.value)} className="h-8 w-52" />
      <NativeSelect aria-label="Item kind" value={kindFilter} onChange={event => setKindFilter(event.target.value)}>
        <option value="all">All</option><option value="skill">Skills</option><option value="rule">Rules</option>
      </NativeSelect>
      <DropdownMenu><DropdownMenuTrigger render={<Button variant="outline" size="sm" className="h-8 bg-card" />}>Machines ({groups.length})</DropdownMenuTrigger>
        <DropdownMenuContent><DropdownMenuGroup>{data.fleet.machines.map(machine => <DropdownMenuCheckboxItem key={machine.id}
          checked={machines === null || machines.includes(machine.id)} closeOnClick={false}
          onCheckedChange={checked => setMachines(old => {
            const current = old ?? data.fleet.machines.map(m => m.id);
            return checked ? [...current, machine.id] : current.filter(id => id !== machine.id);
          })}>{machine.id}</DropdownMenuCheckboxItem>)}</DropdownMenuGroup></DropdownMenuContent>
      </DropdownMenu>
      <div className="ml-auto flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
        <span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit={false} enabled /><CheckboxSwatch explicit={false} enabled={false} />Inherited from Harness default</span>
        <span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit enabled /><CheckboxSwatch explicit enabled={false} />Explicit Assignment</span>
      </div>
    </div>
    <div className="flex min-h-5 flex-wrap items-center gap-x-4 gap-y-1 border-b px-3.5 py-2 text-xs text-muted-foreground">
      <span id="default-hint" className="basis-full">Changing a default for new Skills or Rules applies to items added later.
        Existing items keep their current value as an explicit Assignment.</span>
      {saving.length > 0 && <span role="status" className="ml-auto inline-flex items-center gap-1.5 text-foreground">
        <Pending pending>Saving Assignment for {saving.join(', ')}…</Pending></span>}
    </div>
    {columns.length > 0 && <div className="overflow-hidden"><Table aria-label="Assignment matrix" stickyFirstColumn>
      <TableHeader><TableRow className="bg-muted hover:bg-muted">
        <TableHead rowSpan={2} scope="col" className="sticky left-0 z-10 min-w-48 bg-muted align-bottom md:min-w-64">Skill / Rule</TableHead>
        {groups.filter(g => g.harnesses.length).map(g => <TableHead key={g.machine.id} scope="colgroup" colSpan={g.harnesses.length} className="border-l">
          {g.machine.id}{g.machine.id === status.machine_id && <Badge variant="outline" className="ml-2 border-primary/20 bg-brand-soft text-brand-ink">This Machine</Badge>}</TableHead>)}
      </TableRow><TableRow className="bg-muted hover:bg-muted">
        {columns.map(c => <TableHead key={`${c.machine}/${c.harness}`} scope="col" className="h-8 min-w-28 border-l font-mono text-xs font-normal text-muted-foreground"><div className="flex items-center justify-between gap-1">{c.harness}{menu(`Actions for ${c.machine} / ${c.harness}`, [
          { label: 'Enable all visible items', run: () => bulk(items, [c], true) },
          { label: 'Disable all visible items', run: () => bulk(items, [c], false) },
        ], !!pending[c.machine] || !items.some(item => item.in_baseline || c.machine === status.machine_id))}</div></TableHead>)}
      </TableRow></TableHeader>
      <TableBody>
        <TableRow className="border-t-2 bg-card hover:bg-card"><TableHead colSpan={columns.length + 1} className="h-8 text-[11px] font-semibold tracking-wider text-muted-foreground uppercase">Defaults for new items</TableHead></TableRow>
        {(['skill', 'rule'] as const).map(kind => <TableRow key={kind} className="bg-muted/40 hover:bg-muted/40">
          <TableHead scope="row" className="sticky left-0 z-10 h-auto bg-muted py-3 whitespace-normal">
            <span className="font-medium">Default for new {kind === 'skill' ? 'Skills' : 'Rules'}</span></TableHead>
          {columns.map(c => <TableCell key={`${c.machine}/${c.harness}`} className="border-l"><label className="flex min-h-8 cursor-pointer items-center justify-center pr-7"><Checkbox className="size-[22px] rounded-[6px] not-data-checked:border-dashed"
            checked={(assignment(c.machine).harnesses[c.harness] ?? emptyHarness())[kind === 'skill' ? 'new_skills_default' : 'new_rules_default']}
            disabled={pending[c.machine]} aria-label={`Default for new ${kind}s in ${c.machine} / ${c.harness}`} aria-describedby="default-hint"
            onCheckedChange={enabled => void write(c.machine, [{ harness: c.harness,
              kind: kind === 'skill' ? 'new_skills_default' : 'new_rules_default', enabled }])} /></label></TableCell>)}
        </TableRow>)}
        {items.map(item => <TableRow key={item.item}>
          <TableHead scope="row" className="sticky left-0 z-10 h-14 min-w-60 max-w-80 bg-background py-2 pr-9 font-normal whitespace-normal">
            <div className="flex flex-wrap items-center gap-2"><span className="text-sm font-medium">{splitItem(item.item).name}</span><KindBadge kind={splitItem(item.item).kind} />
              {new Set(columns.map(c => cellState(assignment(c.machine), c.harness, item.item).enabled)).size > 1 &&
                <Tooltip><TooltipTrigger render={<Badge variant="secondary" tabIndex={0} />}>Mixed</TooltipTrigger><TooltipContent>Enabled in some visible Harnesses and disabled in others.</TooltipContent></Tooltip>}
              <span className="absolute top-1/2 right-2 -translate-y-1/2">{menu(`Actions for ${item.item}`, [
                { label: 'Enable in all visible Harnesses', run: () => bulk([item], columns, true) },
                { label: 'Disable in all visible Harnesses', run: () => bulk([item], columns, false) },
                { label: 'Reset all to default', run: () => bulk([item], columns, null) },
              ], !columns.length || columns.some(c => pending[c.machine]) || !columns.some(c => item.in_baseline || c.machine === status.machine_id))}</span>
              {itemStateLabel(item) && <Badge variant="info">{itemStateLabel(item)}</Badge>}</div>
            {item.description && <p className="mt-1 text-xs text-muted-foreground">{item.description}</p>}
          </TableHead>
          {columns.map(c => cell(item, c.machine, c.harness))}
        </TableRow>)}
      </TableBody>
    </Table></div>}
    {!items.length && <Nothing title={search || kindFilter !== 'all' ? 'No matching items' : 'No Skills or Rules'}>
      {search || kindFilter !== 'all' ? 'Change the search or kind filter to see more items.' : "Add a Skill or Rule to this Machine's Draft to get started."}</Nothing>}
    {!columns.length && <Nothing title={groups.length ? 'No Harnesses' : 'No Machines selected'}>
      {groups.length ? 'No Harnesses reported. Check the Machine configuration in Settings.' : 'Choose Machines above to show their Harnesses.'}</Nothing>}
    </div>
  </>;
}
