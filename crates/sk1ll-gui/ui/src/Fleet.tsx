import { Fragment } from 'react';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { useResource } from './hooks';
import { api } from './api';
import { shortSha, statusOrder, time } from './model';
import { ErrorNotice, Loading, Nothing, StatusBadge } from './shared';

export function Fleet({ revision }: { revision: number }) {
  const { data, error, loading } = useResource(api.fleet, revision);
  return <><p className="description">Machines sharing this Baseline. Reports refresh live and every 15 seconds.</p>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    {data && <><p className="meta">Fleet Baseline <code>{shortSha(data.baseline)}</code></p>
      <Table><TableHeader><TableRow>{['Machine', 'Status', 'Baseline', 'Draft items', 'Conflicts', 'Last seen'].map(label =>
        <TableHead key={label}>{label}</TableHead>)}</TableRow></TableHeader><TableBody>
        {[...data.machines].sort((a, b) => statusOrder.indexOf(a.status) - statusOrder.indexOf(b.status) || a.id.localeCompare(b.id))
          .map(machine => <Fragment key={machine.id}><TableRow>
            <TableCell><details><summary>{machine.id}</summary><dl className="report">
              <dt>Connection</dt><dd>{machine.connected ? 'Connected' : 'Disconnected'}</dd>
              <dt>Joined</dt><dd>{time(machine.joined_at)}</dd>
              <dt>Harnesses</dt><dd>{machine.report?.harnesses.join(', ') || 'None reported'}</dd>
              <dt>Draft</dt><dd>{machine.report?.drafts.join(', ') || 'None'}</dd>
              <dt>Unpushed</dt><dd>{machine.report?.unpushed.join(', ') || 'None'}</dd>
              <dt>Conflicts</dt><dd>{machine.report?.conflicts.map(c => `${c.item} in ${c.harness}`).join(', ') || 'None'}</dd>
              <dt>Clashes</dt><dd>{machine.report?.clashes.join(', ') || 'None'}</dd>
              <dt>Blocked</dt><dd>{machine.report?.blocked ? 'Yes' : 'No'}</dd>
              <dt>Behind</dt><dd>{machine.report?.behind ? 'Yes' : 'No'}</dd>
            </dl></details></TableCell><TableCell><StatusBadge status={machine.status} /></TableCell>
            <TableCell><code>{shortSha(machine.report?.baseline)}</code></TableCell>
            <TableCell>{machine.report?.drafts.length ?? 0}</TableCell>
            <TableCell>{machine.report?.conflicts.length ?? 0}</TableCell>
            <TableCell>{time(machine.last_seen)}</TableCell>
          </TableRow></Fragment>)}
      </TableBody></Table>{!data.machines.length && <Nothing>Join a Machine in Settings.</Nothing>}</>}
  </>;
}
