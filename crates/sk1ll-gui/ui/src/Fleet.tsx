import { Fragment, useEffect, useRef, useState, type ReactNode } from 'react';
import { ArrowUpFromLine, BookOpen, Check, ChevronRight, FilePenLine, TriangleAlert } from 'lucide-react';
import { Toggle } from '@base-ui/react/toggle';
import { ToggleGroup } from '@base-ui/react/toggle-group';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { toast } from './toast';
import { cn } from '@/lib/utils';
import { useMutation, useResource } from './hooks';
import { api } from './api';
import { ago, shortSha, splitItem, statusLabels, statusOrder, time } from './model';
import { Action, Confirm, Details, ErrorNotice, Loading, Nothing, Notice, StatusBadge, When } from './shared';
import { FleetAssignments } from './FleetAssignments';
import { JoinCodeButton } from './Settings';
import type { MachineReport, MachineRow, MachineStatus } from './types';

const list = (values: string[] | undefined, none = 'None') => values?.length ? values.join(', ') : none;
const statusColor: Record<MachineStatus, string> = { in_sync: 'bg-success', drafting: 'bg-brand-ink', behind: 'bg-info', conflict: 'bg-warning', blocked: 'bg-destructive', offline: 'bg-muted-foreground' };
const healthOrder: MachineStatus[] = ['in_sync', 'drafting', 'behind', 'conflict', 'blocked', 'offline'];
function guidance(machine: MachineRow, local: boolean) {
  if (!local) return { offline: 'Start its Daemon, or check its network path to the Server.', blocked: `Commit or Restore the clashing items on ${machine.id}, then Pull.`,
    conflict: `Resolve its Conflicts in the GUI on ${machine.id}.`, behind: `${machine.id} Pulls the new Baseline once its Draft allows it.`,
    drafting: 'Draft changes are live in its Harnesses.', in_sync: 'Up to date with the Fleet Baseline.' }[machine.status];
  return { offline: 'Start the Daemon, or check its network path to the Server.', blocked: 'Commit or Restore clashing items, then Pull.', conflict: 'Review Conflicts to resolve edits in your Harnesses.',
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
  if (!report) return <Details className="py-1" rows={[
    ['Connection', machine.connected ? 'Connected' : 'Disconnected'], ['Joined', time(machine.joined_at)],
    ['Last seen', time(machine.last_seen)], ['Report', 'Not reported. This Machine has not sent a report to the Server yet.'],
  ]} />;
  return <Details className="py-1 md:grid-cols-[max-content_minmax(0,1fr)_max-content_minmax(0,1fr)]" rows={[
    ['Connection', machine.connected ? 'Connected' : 'Disconnected'],
    ['Joined', time(machine.joined_at)],
    ['Harnesses', list(report?.harnesses, 'None reported')],
    ['Last seen', time(machine.last_seen)],
    ['Draft', list(report?.drafts)],
    ['Unpushed', list(report?.unpushed)],
    ['Conflicts', list(report?.conflicts.map(c => `${c.item} in ${c.harness}`))],
    ['Clashes', list(report?.clashes)],
    ['Blocked', report?.blocked ? 'Yes' : 'No'],
    ['Behind', report?.behind ? 'Yes' : 'No'],
  ]} />;
}
export function Fleet({ revision, machineId, localReport, unmanaged = [], discovery, navigate, changed, live }: {
  revision: number; machineId: string; localReport?: MachineReport; unmanaged?: { name: string }[]; discovery?: { loading: boolean; error: string }; navigate: (screen: 'Draft' | 'Conflicts' | 'Library' | 'Assignment matrix') => void; changed: () => void; live: boolean;
}) {
  const { data, error, loading, updatedAt } = useResource(api.fleet, revision);
  const [filter, setFilter] = useState<'attention' | 'connected' | null>(null);
  const [open, setOpen] = useState<string[]>([]);
  const table = useRef<HTMLTableElement>(null);
  const [columns, setColumns] = useState(7);
  const toggle = (id: string) => setOpen(value => value.includes(id) ? value.filter(v => v !== id) : [...value, id]);
  const machines = [...data?.machines ?? []]
    .sort((a, b) => statusOrder.indexOf(a.status) - statusOrder.indexOf(b.status) || a.id.localeCompare(b.id));
  const attention = machines.filter(m => ['offline', 'blocked', 'conflict'].includes(m.status)).length;
  const connected = machines.filter(m => m.connected).length;
  const health = healthOrder.map(status => ({ status, count: machines.filter(machine => machine.status === status).length }));
  const report = localReport ?? machines.find(machine => machine.id === machineId)?.report;
  const conflicts = report?.conflicts ?? [];
  const drafts = report?.drafts.length ?? 0, unpushed = report?.unpushed.length ?? 0;
  const queue = Number(conflicts.length > 0) + Number(drafts > 0) + Number(unpushed > 0) + Number(unmanaged.length > 0);
  const push = useMutation(changed);
  const pushNow = () => void push.run(api.push, value => { if (!value.pushed) throw new Error('Push did not reach the Server.'); toast({ title: 'Push complete' }); });
  const visible = machines.filter(machine => filter === 'attention' ? ['offline', 'blocked', 'conflict'].includes(machine.status) : filter === 'connected' ? machine.connected : true);
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
          <Card role="region" aria-labelledby="fleet-health-title" className="gap-0 border py-0 ring-0">
            <CardHeader className="flex flex-row flex-wrap items-center gap-2.5 border-b px-3.5 pt-2.5 [--card-spacing:--spacing(2.5)]"><CardTitle><h2 id="fleet-health-title" className="text-xs font-semibold tracking-wider text-muted-foreground uppercase">Fleet health</h2></CardTitle>
              <div className="ml-auto flex items-center gap-2 font-mono text-xs"><span className="text-muted-foreground">baseline</span><code className="text-brand-ink">{shortSha(data.baseline)}</code></div></CardHeader>
            <CardContent className="flex flex-col gap-2.5 p-3.5">
              <div role="img" aria-label={machines.length ? `Fleet health: ${health.map(group => `${group.count} ${statusLabels[group.status]}`).join(', ')}. ${machines.length} Machines total.` : 'Fleet health: No Machines.'} className="flex h-2 gap-0.5 overflow-hidden rounded-full bg-muted">
                {health.filter(group => group.count > 0).map(group => <span key={group.status} aria-hidden className={statusColor[group.status]} style={{ flex: group.count }} />)}</div>
              <div className="flex flex-wrap gap-x-3.5 gap-y-2 text-xs text-muted-foreground">{health.map(group => <span key={group.status} className="inline-flex items-center gap-1.5"><span aria-hidden className={cn('size-2 rounded-xs', statusColor[group.status])} />{statusLabels[group.status]} <b className="font-mono font-medium text-foreground tabular-nums">{group.count}</b></span>)}</div>
            </CardContent>
          </Card>
          <Card role="region" aria-labelledby="fleet-machines-title" className="@container/machines min-w-0 gap-0 border py-0 ring-0">
            <CardHeader className="flex flex-row flex-wrap items-center gap-2.5 border-b px-3.5 pt-2.5 [--card-spacing:--spacing(2.5)]"><CardTitle><h2 id="fleet-machines-title" className="text-xs font-semibold tracking-wider text-muted-foreground uppercase">Machines</h2></CardTitle>
              <ToggleGroup aria-label="Machine filter" value={[filter ?? 'all']} onValueChange={value => { if (value.length) setFilter(value[0] === 'all' ? null : value[0] as 'attention' | 'connected'); }} className="flex rounded-lg border bg-background p-0.5">
                {([{ value: 'all', label: 'All', count: machines.length }, { value: 'attention', label: 'Attention', count: attention }, { value: 'connected', label: 'Connected', count: connected }] as const).map(option =>
                  <Toggle key={option.value} value={option.value} aria-label={`Filter: ${option.label}`} className="flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring aria-pressed:bg-muted aria-pressed:text-foreground"><span>{option.label}</span><span className="font-mono text-[11px] text-muted-foreground tabular-nums">{option.count}</span></Toggle>)}
              </ToggleGroup><div className="ml-auto"><JoinCodeButton label="Add Machine" /></div></CardHeader>
            <CardContent className="min-w-0 p-0">
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
              <TableCell className="relative pl-4"><span aria-hidden className={cn('absolute inset-y-0 left-0 w-1', statusColor[machine.status])} /><div className="flex min-w-0 items-center gap-2"><span className="truncate font-medium" title={machine.id}>{machine.id}</span>{machine.id === machineId && <Badge variant="info" className="shrink-0">This Machine</Badge>}</div>
                <p className="mt-0.5 truncate text-xs text-muted-foreground" title={guidance(machine, machine.id === machineId)}>{guidance(machine, machine.id === machineId)}</p></TableCell>
              <TableCell><StatusBadge status={machine.status} /></TableCell>
              <TableCell className="hidden @min-[620px]/machines:table-cell"><code className={cn('font-mono text-xs text-muted-foreground', machine.report && machine.report.baseline !== data.baseline && 'text-warning-foreground')}>{shortSha(machine.report?.baseline)}</code></TableCell>
              <TableCell className={cn('hidden text-right font-mono text-xs tabular-nums @min-[520px]/machines:table-cell', machine.report?.drafts.length ? 'font-semibold text-brand-ink' : 'text-muted-foreground')}>{count(machine, machine.report?.drafts.length ?? '—')}</TableCell>
              <TableCell className={cn('text-right font-mono text-xs tabular-nums', machine.report?.conflicts.length ? 'font-semibold text-warning-foreground' : 'text-muted-foreground')}>{count(machine, machine.report?.conflicts.length ?? '—')}</TableCell>
              <TableCell className="hidden text-xs text-muted-foreground @min-[740px]/machines:table-cell" onClick={event => event.stopPropagation()}><When seconds={machine.last_seen} /></TableCell>
              <TableCell><Button variant="ghost" size="icon-xs" aria-expanded={expanded} aria-controls={`machine-${machine.id}`}
                aria-label={`${expanded ? 'Hide' : 'Show'} details for ${machine.id}`} onClick={event => { event.stopPropagation(); toggle(machine.id); }}>
                <ChevronRight className={cn('transition-transform', expanded && 'rotate-90')} /></Button></TableCell>
            </TableRow>
            {expanded && <TableRow id={`machine-${machine.id}`} className="bg-muted/30 hover:bg-muted/30">
              <TableCell colSpan={columns} className="whitespace-normal px-4"><div className="flex min-w-0 flex-col gap-3 py-2"><p className="text-xs text-muted-foreground">{guidance(machine, machine.id === machineId)}</p><MachineDetails machine={machine} />
                {machine.id === machineId && <NextStep machine={machine} navigate={navigate} changed={changed} />}
                <div><Confirm label="Remove Machine" ariaLabel={`Remove ${machine.id} from the Fleet`} title={`Remove ${machine.id} from the Fleet?`} typed={machine.id}
                  description={machine.id === machineId ? "This removes THIS Machine, revokes its credential, and removes its Assignment. The Daemon will stop managing its Harnesses. Content in the Baseline is kept." : "This revokes the Machine's credential and removes its Assignment. Content in the Baseline is kept."}
                  action={() => api.removeMachine(machine.id)} onDone={() => { toast({ title: 'Machine removed' }); changed(); }} /></div></div></TableCell>
            </TableRow>}
          </Fragment>;
        })}</TableBody>
      </Table> : <Nothing className="rounded-none border-0" title={filter ? "No matching Machines" : "No Machines"} action={filter ? <Button variant="outline" size="sm" aria-label="Clear Machine filter" onClick={() => setFilter(null)}>Show all Machines</Button> : undefined}>{filter ? "Clear the filter to see all Machines." : "Use Add Machine to create a join code for another Machine."}</Nothing>}
            </CardContent>
          </Card>
          <FleetAssignments key={machineId} revision={revision} machineId={machineId} harnesses={report?.harnesses ?? []} navigate={navigate} />
        </div>
        <Card role="region" aria-labelledby="fleet-next-title" className="min-w-0 gap-0 border py-0 ring-0">
          <CardHeader className="flex flex-row items-center gap-2.5 border-b px-3.5 pt-2.5 [--card-spacing:--spacing(2.5)]"><CardTitle><h2 id="fleet-next-title" className="text-xs font-semibold tracking-wider text-muted-foreground uppercase">Up next</h2></CardTitle><Badge variant="secondary" className="ml-auto font-mono tabular-nums">{queue}</Badge></CardHeader>
          <CardContent className="p-0">{queue ? <ul className="divide-y">
            {!!conflicts.length && <li><button type="button" aria-label="Resolve conflict" onClick={() => navigate('Conflicts')} className="flex w-full items-center gap-2.5 px-3.5 py-3 text-left outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset">
              <span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-warning/13 text-warning-foreground"><TriangleAlert className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Resolve conflict</span><span className="block truncate text-xs text-muted-foreground">{splitItem(conflicts[0].item).name} in {conflicts[0].harness}{conflicts.length > 1 ? `, plus ${conflicts.length - 1} more` : ''}</span></span><ChevronRight aria-hidden className="ml-auto size-4 shrink-0 text-muted-foreground" /></button></li>}
            {!!unpushed && <li className="flex items-center gap-2.5 px-3.5 py-3"><span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-brand-soft text-brand-ink"><ArrowUpFromLine className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Push {unpushed} unpushed {unpushed === 1 ? 'item' : 'items'}</span><span className="block truncate text-xs text-muted-foreground">{splitItem(report!.unpushed[0]).name}</span></span><Button size="sm" className="ml-auto" disabled={push.pending} onClick={pushNow}>{push.pending ? 'Pushing…' : 'Push'}</Button></li>}
            {!!drafts && <li><button type="button" aria-label="Review Draft" onClick={() => navigate('Draft')} className="flex w-full items-center gap-2.5 px-3.5 py-3 text-left outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset">
              <span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-brand-soft text-brand-ink"><FilePenLine className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Review Draft</span><span className="block text-xs text-muted-foreground">{drafts} Draft {drafts === 1 ? 'item' : 'items'}</span></span><ChevronRight aria-hidden className="ml-auto size-4 shrink-0 text-muted-foreground" /></button></li>}
            {!!unmanaged.length && <li><button type="button" aria-label="Adopt unmanaged skills" onClick={() => navigate('Library')} className="flex w-full items-center gap-2.5 px-3.5 py-3 text-left outline-none hover:bg-muted/50 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"><span aria-hidden className="grid size-7 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground"><BookOpen className="size-4" /></span><span className="min-w-0"><span className="block text-sm font-medium">Adopt unmanaged skills</span><span className="block truncate text-xs text-muted-foreground">{unmanaged.length} unmanaged {unmanaged.length === 1 ? 'skill' : 'skills'} · {unmanaged[0].name}</span></span><ChevronRight aria-hidden className="ml-auto size-4 shrink-0 text-muted-foreground" /></button></li>}
          </ul> : <Empty className="py-6"><EmptyHeader><EmptyMedia variant="icon"><Check /></EmptyMedia><EmptyTitle>{discovery?.error ? 'Skill discovery failed' : discovery?.loading ? 'Checking for unmanaged skills…' : 'All caught up'}</EmptyTitle><EmptyDescription>{discovery?.error ? 'Unmanaged skills could not be listed, so this queue may be incomplete.' : 'No Draft items, unpushed items, or Conflicts reported for this Machine.'}</EmptyDescription></EmptyHeader></Empty>}{!!queue && discovery?.error && <p role="status" className="border-t px-3.5 py-2 text-xs text-warning-foreground">Unmanaged skills could not be listed.</p>}<ErrorNotice error={push.error} /></CardContent>
        </Card>
      </div></div>
    </>}
  </>;
}
