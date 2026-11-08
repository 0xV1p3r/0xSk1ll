import { Fragment, useState, type ReactNode } from 'react';
import { ChevronRight } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { cn } from '@/lib/utils';
import { useResource } from './hooks';
import { api } from './api';
import { ago, shortSha, statusOrder, time } from './model';
import { Action, Details, ErrorNotice, Loading, Nothing, Notice, StatusBadge, When } from './shared';
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
function Stat({ label, value, hint }: { label: string; value: ReactNode; hint?: string }) {
  return <Card><CardHeader><CardDescription>{label}</CardDescription>
    <CardTitle className="text-2xl font-semibold tabular-nums">{value}</CardTitle>
    {hint && <CardDescription className="text-xs">{hint}</CardDescription>}</CardHeader></Card>;
}
export function Fleet({ revision, machineId, navigate, changed, live }: {
  revision: number; machineId: string; navigate: (screen: 'Draft' | 'Conflicts') => void; changed: () => void; live: boolean;
}) {
  const { data, error, loading, updatedAt } = useResource(api.fleet, revision);
  const [open, setOpen] = useState<string[]>([]);
  const toggle = (id: string) => setOpen(value => value.includes(id) ? value.filter(v => v !== id) : [...value, id]);
  const machines = [...data?.machines ?? []]
    .sort((a, b) => statusOrder.indexOf(a.status) - statusOrder.indexOf(b.status) || a.id.localeCompare(b.id));
  const attention = machines.filter(m => ['offline', 'blocked', 'conflict'].includes(m.status)).length;
  return <>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    {data && <>
      {(!!error || !live) && <Notice title="Showing last available Fleet data">Server updates are unavailable, so Machine reports may be out of date.
        Read from the Daemon {ago(updatedAt ?? null).toLowerCase()}.</Notice>}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Stat label="Machines" value={machines.length} />
        <Stat label="Connected" value={machines.filter(m => m.connected).length} />
        <Stat label="Need attention" value={attention} hint="Offline, Blocked, or in Conflict" />
        <Stat label="Fleet Baseline" value={<code className="font-mono text-xl">{shortSha(data.baseline)}</code>} />
      </div>
      <div className="flex items-center justify-between gap-4"><h2 className="text-base font-semibold">Machines</h2>
        <JoinCodeButton label="Add Machine" /></div>
      {machines.length > 0 ? <div className="overflow-hidden rounded-lg border"><Table aria-label="Machines" stickyFirstColumn>
        <TableHeader><TableRow className="bg-muted hover:bg-muted">
          {['Machine', 'Status', 'Baseline', 'Draft items', 'Conflicts', 'Last seen'].map(label =>
            <TableHead key={label} className={cn(['Draft items', 'Conflicts'].includes(label) && 'text-right')}>{label}</TableHead>)}
        </TableRow></TableHeader>
        <TableBody>{machines.map(machine => {
          const expanded = open.includes(machine.id);
          return <Fragment key={machine.id}>
            <TableRow data-state={expanded ? 'selected' : undefined} className="cursor-pointer" onClick={() => toggle(machine.id)}>
              <TableCell className="font-medium"><Button variant="ghost" size="icon-xs" aria-expanded={expanded} aria-controls={`machine-${machine.id}`}
                aria-label={`${expanded ? 'Hide' : 'Show'} details for ${machine.id}`} onClick={event => { event.stopPropagation(); toggle(machine.id); }}>
                <ChevronRight className={cn('transition-transform', expanded && 'rotate-90')} /></Button><span className="ml-2">{machine.id}</span>{machine.id === machineId && <Badge variant="secondary" className="ml-2">This Machine</Badge>}</TableCell>
              <TableCell><StatusBadge status={machine.status} /></TableCell>
              <TableCell><code className="font-mono text-xs">{shortSha(machine.report?.baseline)}</code></TableCell>
              <TableCell className="text-right tabular-nums">{machine.report?.drafts.length ?? '—'}</TableCell>
              <TableCell className="text-right tabular-nums">{machine.report?.conflicts.length ?? '—'}</TableCell>
              <TableCell className="text-muted-foreground" onClick={event => event.stopPropagation()}><When seconds={machine.last_seen} /></TableCell>
            </TableRow>
            {expanded && <TableRow id={`machine-${machine.id}`} className="bg-muted/30 hover:bg-muted/30">
              <TableCell colSpan={6} className="whitespace-normal"><div className="flex flex-col gap-3 py-2"><MachineDetails machine={machine} />
                <NextStep machine={machine} local={machine.id === machineId} navigate={navigate} changed={changed} /></div></TableCell>
            </TableRow>}
          </Fragment>;
        })}</TableBody>
      </Table></div> : <Nothing title="No Machines">Use Add Machine to create a join code for another Machine.</Nothing>}
    </>}
  </>;
}
