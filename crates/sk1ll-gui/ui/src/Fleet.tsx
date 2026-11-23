import { Fragment, useEffect, useRef, useState, type ReactNode } from 'react';
import { ArrowUpFromLine, BookOpen, Check, ChevronRight, FilePenLine, TriangleAlert } from 'lucide-react';
import { Toggle } from '@base-ui/react/toggle';
import { ToggleGroup } from '@base-ui/react/toggle-group';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { toast } from './toast';
import { cn } from '@/lib/utils';
import { useMutation, useResource } from './hooks';
import { api } from './api';
import { ago, shortSha, splitItem, statusLabels, statusOrder, time } from './model';
import { Action, Confirm, Details, ErrorNotice, Loading, Nothing, Notice, Panel, PanelToolbar, SectionTitle, StatusBadge, When } from './shared';
import { FleetAssignments } from './FleetAssignments';
import { JoinCodeButton, ownScope, scopeSummary } from './Settings';
import type { MachineReport, MachineRow, MachineStatus } from './types';

const list = (values: string[] | undefined, none = 'None') => values?.length ? values.join(', ') : none;
const statusColor: Record<MachineStatus, string> = { in_sync: 'bg-success', drafting: 'bg-drafting', behind: 'bg-info', conflict: 'bg-warning', blocked: 'bg-destructive', offline: 'bg-muted-foreground' };
const healthOrder: MachineStatus[] = ['in_sync', 'drafting', 'behind', 'conflict', 'blocked', 'offline'];
function guidance(machine: MachineRow, local: boolean) {
  if (!local) return { offline: 'Start its Daemon, or check its network path to the Server.', blocked: `On ${machine.id}, Keep mine on the clashing items (its version goes back on top of the new Baseline, over this change), or Restore them to take the Baseline.`,
    conflict: `Resolve its Conflicts in the GUI on ${machine.id}.`, behind: 'Pulls the new Baseline once its Draft allows it.',
    drafting: 'Draft changes are live in its Harnesses.', in_sync: 'Up to date with the Fleet Baseline.' }[machine.status];
  return { offline: 'Start the Daemon, or check its network path to the Server.', blocked: 'In Draft, Keep mine on the clashing items (your version goes back on top of the new Baseline, over the other change), or Restore them to take the Baseline.', conflict: 'Review Conflicts to resolve edits in your Harnesses.',
    behind: 'Pull the new Baseline once your Draft allows it.', drafting: 'Review your Draft before Commit.', in_sync: 'Up to date with the Fleet Baseline.' }[machine.status];
}
function NextStep({ machine, navigate, changed }: {
  machine: MachineRow; navigate: (screen: 'Draft' | 'Conflicts') => void; changed: () => void;
}) {
  const report = machine.report;
  return <div className="flex flex-wrap gap-2">
    {!!report?.conflicts.length && <Button size="sm" onClick={() => navigate('Conflicts')}>Review Conflicts</Button>}
    {(!!report?.drafts.length || report?.blocked) && <Button size="sm" variant="outline" onClick={() => navigate('Draft')}>Review Draft</Button>}
    {machine.status === 'behind' && <Action label="Pull Baseline" action={api.pull} onDone={changed} />}
  </div>;
}
function MachineDetails({ machine }: { machine: MachineRow }) {
  const report = machine.report;
  const scope = machine.scope ?? 'fleet';
  const scopeRow: [string, ReactNode] = ['Scope', <>{scope === 'fleet' ? 'Whole Fleet' : 'Only itself'} <span className="text-muted-foreground">· {scopeSummary[scope]}</span></>];
  if (!report) return <Details className="py-1" rows={[
    ['Connection', machine.connected ? 'Connected' : 'Disconnected'], ['Joined', <When seconds={machine.joined_at} />], scopeRow,
    ['Last seen', <When seconds={machine.last_seen} />], ['Harnesses', 'None reported'], ['Report', 'Not reported. This Machine has not sent a report to the Server yet.'],
  ]} />;
  return <Details className="py-1 md:grid-cols-[max-content_minmax(0,1fr)_max-content_minmax(0,1fr)]" rows={[
    ['Connection', machine.connected ? 'Connected' : 'Disconnected'],
    ['Joined', <When seconds={machine.joined_at} />],
    ['Harnesses', list(report?.harnesses, 'None reported')],
    ['Last seen', <When seconds={machine.last_seen} />],
    scopeRow,
    ...(report.drafts.length ? [['Draft', list(report.drafts)] as [string, ReactNode]] : []),
    ...(report.unpushed.length ? [['Unpushed', list(report.unpushed)] as [string, ReactNode]] : []),
    ...(report.conflicts.length ? [['Conflicts', list(report.conflicts.map(c => `${c.item} in ${c.harness}`))] as [string, ReactNode]] : []),
    ...(report.clashes.length ? [['Clashes', list(report.clashes)] as [string, ReactNode]] : []),
    ...(report.blocked ? [['Blocked', 'Yes'] as [string, ReactNode]] : []),
    ...(report.behind ? [['Behind', 'Yes'] as [string, ReactNode]] : []),
    ...(report.push_refused ? [['Push refused', report.push_refused] as [string, ReactNode]] : []),
  ]} />;
}
export function Fleet({ revision, machineId, initialMachine, serverUrl = null, localReport, unmanaged = [], discovery, navigate, changed, live }: {
  revision: number; machineId: string; initialMachine?: string; serverUrl?: string | null; localReport?: MachineReport; unmanaged?: { name: string }[]; discovery?: { loading: boolean; error: string }; navigate: (screen: 'Draft' | 'Conflicts' | 'Library' | 'Assignment matrix') => void; changed: () => void; live: boolean;
}) {
  const { data, error, loading, updatedAt } = useResource(api.fleet, revision);
  const [filter, setFilter] = useState<'attention' | 'connected' | null>(null);
  const [selectedMachine, setSelectedMachine] = useState(initialMachine);
  const [open, setOpen] = useState<string[]>(initialMachine ? [initialMachine] : []);
  const table = useRef<HTMLTableElement>(null);
  const [columns, setColumns] = useState(7);
  const toggle = (id: string) => setOpen(value => value.includes(id) ? value.filter(v => v !== id) : [...value, id]);
  const machines = [...data?.machines ?? []]
    .sort((a, b) => statusOrder.indexOf(a.status) - statusOrder.indexOf(b.status) || a.id.localeCompare(b.id));
  const attention = machines.filter(m => ['offline', 'blocked', 'conflict'].includes(m.status)).length;
  const connected = machines.filter(m => m.connected).length;
  const health = healthOrder.map(status => ({ status, count: machines.filter(machine => machine.status === status).length }));
  const local = machines.find(machine => machine.id === machineId);
  const report = localReport ?? local?.report;
  const scope = ownScope(data, machineId);
  const conflicts = report?.conflicts ?? [];
  const drafts = report?.drafts.length ?? 0, unpushed = report?.unpushed.length ?? 0;
  const queue = Number(conflicts.length > 0) + Number(drafts > 0) + Number(unpushed > 0) + Number(unmanaged.length > 0);
  const push = useMutation(changed);
  const pushNow = () => void push.run(api.push, value => { if (!value.pushed) throw new Error('Push did not reach the Server.'); toast({ title: 'Push complete' }); });
  // A selected Machine that was removed falls back to every Machine.
  useEffect(() => { if (data && selectedMachine && !data.machines.some(machine => machine.id === selectedMachine)) setSelectedMachine(undefined); }, [data, selectedMachine]);
  const visible = machines.filter(machine => selectedMachine ? machine.id === selectedMachine : filter === 'attention' ? ['offline', 'blocked', 'conflict'].includes(machine.status) : filter === 'connected' ? machine.connected : true);
  useEffect(() => {
    const node = table.current;
    if (!node) return;
    const update = () => setColumns([...node.querySelectorAll('thead th')].filter(head => getComputedStyle(head).display !== 'none').length);
    update();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => observer.disconnect();
  }, [visible.length]);
  const count = (machine: MachineRow, value: ReactNode) => machine.status === 'offline' || !machine.connected ? <Tooltip><TooltipTrigger render={<span tabIndex={0} className="text-muted-foreground opacity-60" />}>{value}</TooltipTrigger><TooltipContent>As of last report {ago(machine.last_seen)} ({time(machine.last_seen)})</TooltipContent></Tooltip> : value;
  return <>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    {data && <>
      {(!!error || !live) && <Notice title="Showing last available Fleet data">Server updates are unavailable, so Machine reports may be out of date.
        Read from the Daemon {ago(updatedAt ?? null).toLowerCase()}.</Notice>}
      <div className="@container min-w-0"><div className="grid min-w-0 items-start gap-3.5 @min-[1000px]:grid-cols-[minmax(0,1fr)_320px]">
        <div className="flex min-w-0 flex-col gap-3.5">
          <Panel label="Fleet health" className="gap-0 border py-0 ring-0">
            <PanelToolbar><SectionTitle id="fleet-health-title">Fleet health</SectionTitle>
              <div className="ml-auto flex items-center gap-2 text-xs"><span className="text-muted-foreground">baseline</span><code className="font-mono text-foreground">{shortSha(data.baseline)}</code></div></PanelToolbar>
            <div className="flex flex-col gap-2.5 p-3.5">
              <div role="img" aria-label={machines.length ? `Fleet health: ${health.map(group => `${group.count} ${statusLabels[group.status]}`).join(', ')}. ${machines.length} Machines total.` : 'Fleet health: No Machines.'} className="flex h-12 gap-1 overflow-hidden rounded-lg bg-muted">
                {health.filter(group => group.count > 0).map(group => <span key={group.status} aria-hidden className={statusColor[group.status]} style={{ flex: group.count }} />)}</div>
              <div className="grid grid-cols-2 gap-x-4 gap-y-3 text-xs text-muted-foreground sm:grid-cols-3 xl:grid-cols-6">{health.map(group => <span key={group.status} className={cn('flex items-center gap-2', !group.count && 'opacity-40')}><span aria-hidden className={cn('size-2 rounded-xs', statusColor[group.status])} /><b className="text-xl font-semibold text-foreground tabular-nums">{group.count}</b>{statusLabels[group.status]}</span>)}</div>
            </div>
          </Panel>
          <Panel label="Machines" className="@container/machines min-w-0 gap-0 border py-0 ring-0">
            <PanelToolbar><SectionTitle id="fleet-machines-title">Machines</SectionTitle>
              <ToggleGroup aria-label="Machine filter" value={selectedMachine ? [] : [filter ?? 'all']} onValueChange={value => { setSelectedMachine(undefined); if (value.length) setFilter(value[0] === 'all' ? null : value[0] as 'attention' | 'connected'); }} className="flex rounded-lg border bg-background p-0.5">
                {([{ value: 'all', label: 'All', count: machines.length }, { value: 'attention', label: 'Attention', count: attention }, { value: 'connected', label: 'Connected', count: connected }] as const).map(option =>
                  <Toggle key={option.value} value={option.value} aria-label={`Filter: ${option.label}`} className="flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring aria-pressed:bg-muted aria-pressed:text-foreground"><span>{option.label}</span><span className="text-[11px] text-muted-foreground tabular-nums">{option.count}</span></Toggle>)}
              </ToggleGroup></PanelToolbar>
            <div className="border-b px-3.5 py-2.5"><JoinCodeButton label="Add Machine" serverUrl={serverUrl} scope={scope} /></div>
            <div className="min-w-0 p-0">
      {visible.length > 0 ? <Table ref={table} aria-label="Machines" className="table-fixed">
        <TableHeader><TableRow className="hover:bg-transparent [&_th]:h-8 [&_th]:text-[11px] [&_th]:text-muted-foreground">
          <TableHead className="pl-4">Machine</TableHead><TableHead className="w-24">Status</TableHead>
          <TableHead className="hidden w-22 @min-[620px]/machines:table-cell">Baseline</TableHead>
          <TableHead className="hidden w-14 text-right @min-[520px]/machines:table-cell">Draft<span className="sr-only"> items</span></TableHead>
          <TableHead className="w-18 text-right">Conflicts</TableHead>
          <TableHead className="hidden w-28 @min-[740px]/machines:table-cell">Last seen</TableHead><TableHead className="w-10"><span className="sr-only">Details</span></TableHead>
        </TableRow></TableHeader>
        <TableBody>{visible.map(machine => {
          const expanded = open.includes(machine.id);
          return <Fragment key={machine.id}>
            <TableRow data-state={expanded ? 'selected' : undefined} className="h-14 cursor-pointer" onClick={() => toggle(machine.id)}>
              <TableCell className="relative pl-4"><span aria-hidden className={cn('absolute inset-y-0 left-0 w-1', statusColor[machine.status])} /><div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5"><span className="max-w-full truncate font-medium" title={machine.id}>{machine.id}</span>{machine.id === machineId && <Badge variant="info" className="shrink-0">This Machine</Badge>}{machine.scope === 'self' && <Badge variant="secondary" className="shrink-0" title={scopeSummary.self}>Self-only</Badge>}</div>
                <p className="mt-0.5 line-clamp-2 whitespace-normal text-xs text-muted-foreground" title={guidance(machine, machine.id === machineId)}>{guidance(machine, machine.id === machineId)}</p></TableCell>
              <TableCell><StatusBadge status={machine.status} /></TableCell>
              <TableCell className="hidden @min-[620px]/machines:table-cell"><code className={cn('font-mono text-xs text-muted-foreground', machine.report && machine.report.baseline !== data.baseline && 'text-warning-foreground')}>{shortSha(machine.report?.baseline)}</code></TableCell>
              <TableCell className={cn('hidden text-right text-xs tabular-nums @min-[520px]/machines:table-cell', machine.report?.drafts.length ? 'font-semibold text-foreground' : 'text-muted-foreground')}>{count(machine, machine.report?.drafts.length ?? '—')}</TableCell>
              <TableCell className={cn('text-right text-xs tabular-nums', machine.report?.conflicts.length ? 'font-semibold text-warning-foreground' : 'text-muted-foreground')}>{count(machine, machine.report?.conflicts.length ?? '—')}</TableCell>
              <TableCell className="hidden text-xs text-muted-foreground @min-[740px]/machines:table-cell" onClick={event => event.stopPropagation()}><When seconds={machine.last_seen} /></TableCell>
              <TableCell><Button variant="ghost" size="icon-xs" aria-expanded={expanded} aria-controls={`machine-${machine.id}`}
                aria-label={`${expanded ? 'Hide' : 'Show'} details for ${machine.id}`} onClick={event => { event.stopPropagation(); toggle(machine.id); }}>
                <ChevronRight className={cn('transition-transform', expanded && 'rotate-90')} /></Button></TableCell>
            </TableRow>
            {expanded && <TableRow id={`machine-${machine.id}`} className="bg-muted/30 hover:bg-muted/30">
              <TableCell colSpan={columns} className="relative whitespace-normal px-4"><span aria-hidden className={cn('absolute inset-y-0 left-0 w-1', statusColor[machine.status])} /><div className="flex min-w-0 flex-col gap-3 py-2"><MachineDetails machine={machine} />
                {machine.id === machineId && <NextStep machine={machine} navigate={navigate} changed={changed} />}
                {(scope !== 'self' || machine.id === machineId) && <div><Confirm label="Remove Machine" ariaLabel={`Remove ${machine.id} from the Fleet`} title={`Remove ${machine.id} from the Fleet?`} typed={machine.id}
                  description={machine.id === machineId ? "This removes THIS Machine, revokes its credential, and removes its Assignment. The Daemon will stop managing its Harnesses. Content in the Baseline is kept." : "This revokes the Machine's credential and removes its Assignment. Content in the Baseline is kept."}
                  action={() => api.removeMachine(machine.id)} onDone={() => { toast({ title: 'Machine removed' }); changed(); }} /></div>}</div></TableCell>
            </TableRow>}
          </Fragment>;
        })}</TableBody>
      </Table> : <Nothing className="rounded-none border-0" title={filter ? "No matching Machines" : "No Machines"} action={filter ? <Button variant="outline" size="sm" aria-label="Clear Machine filter" onClick={() => setFilter(null)}>Show all Machines</Button> : undefined}>{filter ? "Clear the filter to see all Machines." : "Use Add Machine to create a join code for another Machine."}</Nothing>}
            </div>
          </Panel>
          <FleetAssignments registration={local?.registration} key={machineId} revision={revision} machineId={machineId} harnesses={report?.harnesses ?? []} navigate={navigate} />
        </div>
        <Panel label="Up next" className="min-w-0 gap-0 border py-0 ring-0">
          <PanelToolbar><SectionTitle id="fleet-next-title">Up next</SectionTitle>{queue > 0 && <Badge variant="secondary" className="ml-auto tabular-nums">{queue}</Badge>}</PanelToolbar>
          <div className="p-0">{queue ? <ul className="divide-y">
            {!!conflicts.length && <li><button type="button" aria-label="Resolve conflict" onClick={() => navigate('Conflicts')} className="flex w-full items-center gap-2.5 px-3.5 py-3 text-left outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset">
              <span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-warning/13 text-warning-foreground"><TriangleAlert className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Resolve conflict</span><span className="block truncate text-xs text-muted-foreground">{splitItem(conflicts[0].item).name} in {conflicts[0].harness}{conflicts.length > 1 ? `, plus ${conflicts.length - 1} more` : ''}</span></span><ChevronRight aria-hidden className="ml-auto size-4 shrink-0 text-muted-foreground" /></button></li>}
            {!!unpushed && <li className="flex items-center gap-2.5 px-3.5 py-3"><span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-brand-soft text-brand-ink"><ArrowUpFromLine className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Push {unpushed} unpushed {unpushed === 1 ? 'item' : 'items'}</span><span className="block truncate text-xs text-muted-foreground">{splitItem(report!.unpushed[0]).name}{unpushed > 1 ? `, plus ${unpushed - 1} more` : ''}</span></span><Button size="sm" className="ml-auto" disabled={push.pending} onClick={pushNow}>{push.pending ? 'Pushing…' : 'Push'}</Button></li>}
            {!!drafts && <li><button type="button" aria-label="Review Draft" onClick={() => navigate('Draft')} className="flex w-full items-center gap-2.5 px-3.5 py-3 text-left outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset">
              <span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-brand-soft text-brand-ink"><FilePenLine className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Review Draft</span><span className="block text-xs text-muted-foreground">{drafts} Draft {drafts === 1 ? 'item' : 'items'}</span></span><ChevronRight aria-hidden className="ml-auto size-4 shrink-0 text-muted-foreground" /></button></li>}
            {!!unmanaged.length && <li><button type="button" aria-label="Adopt unmanaged skills" onClick={() => navigate('Library')} className="flex w-full items-center gap-2.5 px-3.5 py-3 text-left outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"><span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground"><BookOpen className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Adopt unmanaged skills</span><span className="block truncate text-xs text-muted-foreground">{unmanaged.length} unmanaged {unmanaged.length === 1 ? 'skill' : 'skills'} · {unmanaged[0].name}</span></span><ChevronRight aria-hidden className="ml-auto size-4 shrink-0 text-muted-foreground" /></button></li>}
          </ul> : <Empty className="py-6"><EmptyHeader><EmptyMedia variant="icon"><Check /></EmptyMedia><EmptyTitle>{discovery?.error ? 'Skill discovery failed' : discovery?.loading ? 'Checking for unmanaged skills…' : 'All caught up'}</EmptyTitle><EmptyDescription>{discovery?.error ? 'Unmanaged skills could not be listed, so this queue may be incomplete.' : 'No Draft items, unpushed items, or Conflicts reported for this Machine.'}</EmptyDescription></EmptyHeader></Empty>}{!!queue && discovery?.error && <p role="status" className="border-t px-3.5 py-2 text-xs text-warning-foreground">Unmanaged skills could not be listed.</p>}<ErrorNotice error={push.error} /></div>
        </Panel>
      </div></div>
    </>}
  </>;
}
