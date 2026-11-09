import { Fragment, useState, type ReactNode } from 'react';
import { ChevronRight } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { toast } from './toast';
import { cn } from '@/lib/utils';
import { useResource } from './hooks';
import { api } from './api';
import { ago, shortSha, statusOrder, time } from './model';
import { Action, Confirm, Details, ErrorNotice, Loading, Nothing, Notice, StatusBadge, When } from './shared';
import { JoinCodeButton } from './Settings';
import type { MachineRow } from './types';

const list = (values: string[] | undefined, none = 'None') => values?.length ? values.join(', ') : none;
function NextStep({ machine, local, navigate, changed }: {
  machine: MachineRow; local: boolean; navigate: (screen: 'Draft' | 'Conflicts') => void; changed: () => void;
}) {
  const report = machine.report;
  if (!local) {
    const hint = { offline: 'Start its Daemon, or check its network path to the Server.', blocked: `Commit or Restore the clashing items on ${machine.id}, then Pull.`,
      conflict: `Resolve its Conflicts in the GUI on ${machine.id}.`, behind: `${machine.id} Pulls the new Baseline once its Draft allows it.` } as Record<string, string>;
    return hint[machine.status] ? <p className="text-xs text-muted-foreground">{hint[machine.status]}</p> : null;
  }
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
function Stat({ label, value, hint, onClick, active }: { label: string; value: ReactNode; hint?: string; onClick?: () => void; active?: boolean }) {
  const content = <Card><CardHeader><CardDescription>{label}</CardDescription>
    <CardTitle className="text-2xl font-semibold tabular-nums">{value}</CardTitle>
    {hint && <CardDescription className="text-xs">{hint}</CardDescription>}</CardHeader></Card>;
  return onClick ? <button type="button" aria-label={`Filter: ${label}`} aria-pressed={active} onClick={onClick} className="rounded-lg text-left focus-visible:outline-2 focus-visible:outline-ring">{content}</button> : content;
}
export function Fleet({ revision, machineId, navigate, changed, live }: {
  revision: number; machineId: string; navigate: (screen: 'Draft' | 'Conflicts') => void; changed: () => void; live: boolean;
}) {
  const { data, error, loading, updatedAt } = useResource(api.fleet, revision);
  const [filter, setFilter] = useState<'attention' | 'connected' | null>(null);
  const [open, setOpen] = useState<string[]>([]);
  const toggle = (id: string) => setOpen(value => value.includes(id) ? value.filter(v => v !== id) : [...value, id]);
  const machines = [...data?.machines ?? []]
    .sort((a, b) => statusOrder.indexOf(a.status) - statusOrder.indexOf(b.status) || a.id.localeCompare(b.id));
  const attention = machines.filter(m => ['offline', 'blocked', 'conflict'].includes(m.status)).length;
  const visible = machines.filter(machine => filter === 'attention' ? ['offline', 'blocked', 'conflict'].includes(machine.status) : filter === 'connected' ? machine.connected : true);
  const count = (machine: MachineRow, value: ReactNode) => machine.status === 'offline' || !machine.connected ? <Tooltip><TooltipTrigger render={<span tabIndex={0} className="text-muted-foreground opacity-60" />}>{value}</TooltipTrigger><TooltipContent>As of last report {ago(machine.last_seen)} ({time(machine.last_seen)})</TooltipContent></Tooltip> : value;
  return <>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    {data && <>
      {(!!error || !live) && <Notice title="Showing last available Fleet data">Server updates are unavailable, so Machine reports may be out of date.
        Read from the Daemon {ago(updatedAt ?? null).toLowerCase()}.</Notice>}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Stat label="Machines" value={machines.length} active={!filter} onClick={() => setFilter(null)} />
        <Stat label="Connected" active={filter === 'connected'} onClick={() => setFilter('connected')} value={machines.filter(m => m.connected).length} />
        <Stat label="Need attention" active={filter === 'attention'} onClick={() => setFilter('attention')} value={attention} hint="Offline, Blocked, or in Conflict" />
        <Stat label="Fleet Baseline" value={<code className="font-mono text-xl">{shortSha(data.baseline)}</code>} />
      </div>
      <div className="flex items-center justify-between gap-4"><h2 className="text-base font-semibold">Machines</h2>
        <JoinCodeButton label="Add Machine" /></div>
      {filter && <div><Button variant="secondary" size="sm" aria-label="Clear Machine filter" onClick={() => setFilter(null)}>{filter === 'attention' ? 'Need attention' : 'Connected'} ×</Button></div>}
      {visible.length > 0 ? <div className="overflow-hidden rounded-lg border"><Table aria-label="Machines" stickyFirstColumn>
        <TableHeader><TableRow className="bg-muted hover:bg-muted">
          {['Machine', 'Status', 'Baseline', 'Draft items', 'Conflicts', 'Last seen'].map(label =>
            <TableHead key={label} className={cn(['Draft items', 'Conflicts'].includes(label) && 'text-right')}>{label}</TableHead>)}
        </TableRow></TableHeader>
        <TableBody>{visible.map(machine => {
          const expanded = open.includes(machine.id);
          return <Fragment key={machine.id}>
            <TableRow data-state={expanded ? 'selected' : undefined} className="cursor-pointer" onClick={() => toggle(machine.id)}>
              <TableCell className="font-medium"><Button variant="ghost" size="icon-xs" aria-expanded={expanded} aria-controls={`machine-${machine.id}`}
                aria-label={`${expanded ? 'Hide' : 'Show'} details for ${machine.id}`} onClick={event => { event.stopPropagation(); toggle(machine.id); }}>
                <ChevronRight className={cn('transition-transform', expanded && 'rotate-90')} /></Button><span className="ml-2">{machine.id}</span>{machine.id === machineId && <Badge variant="secondary" className="ml-2">This Machine</Badge>}{machine.id !== machineId && <div className="mt-1 whitespace-normal"><NextStep machine={machine} local={false} navigate={navigate} changed={changed} /></div>}</TableCell>
              <TableCell><StatusBadge status={machine.status} /></TableCell>
              <TableCell><code className="font-mono text-xs">{shortSha(machine.report?.baseline)}</code></TableCell>
              <TableCell className="text-right tabular-nums">{count(machine, machine.report?.drafts.length ?? '—')}</TableCell>
              <TableCell className="text-right tabular-nums">{count(machine, machine.report?.conflicts.length ?? '—')}</TableCell>
              <TableCell className="text-muted-foreground" onClick={event => event.stopPropagation()}><When seconds={machine.last_seen} /></TableCell>
            </TableRow>
            {expanded && <TableRow id={`machine-${machine.id}`} className="bg-muted/30 hover:bg-muted/30">
              <TableCell colSpan={6} className="whitespace-normal"><div className="flex flex-col gap-3 py-2"><MachineDetails machine={machine} />
                {machine.id === machineId && <NextStep machine={machine} local navigate={navigate} changed={changed} />}
                <div><Confirm label="Remove Machine" ariaLabel={`Remove ${machine.id} from the Fleet`} title={`Remove ${machine.id} from the Fleet?`} typed={machine.id}
                  description={machine.id === machineId ? "This removes THIS Machine, revokes its credential, and removes its Assignment. The Daemon will stop managing its Harnesses. Content in the Baseline is kept." : "This revokes the Machine's credential and removes its Assignment. Content in the Baseline is kept."}
                  action={() => api.removeMachine(machine.id)} onDone={() => { toast({ title: 'Machine removed' }); changed(); }} /></div></div></TableCell>
            </TableRow>}
          </Fragment>;
        })}</TableBody>
      </Table></div> : <Nothing title={filter ? "No matching Machines" : "No Machines"}>{filter ? "Clear the filter to see all Machines." : "Use Add Machine to create a join code for another Machine."}</Nothing>}
    </>}
  </>;
}
