import { useCallback, useState } from 'react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api } from './api';
import { useResource } from './hooks';
import { Action, Confirm, ErrorNotice, Loading, Nothing, Notice } from './shared';
import { shortSha, time } from './model';
import type { ConflictView } from './types';

function Conflict({ entry, displaced, changed }: { entry: ConflictView; displaced: boolean; changed: () => void }) {
  const target = { harness: entry.harness, name: entry.name };
  return <section className="list-entry"><div className="row"><code>{entry.name}</code><Badge variant="outline">{entry.harness}</Badge></div>
    <p className="meta">{entry.dest}</p>{entry.reason && <p>{entry.reason}</p>}
    {entry.path && <p className="meta">Recovery path {entry.path}</p>}
    {entry.displaced_at !== undefined && <p className="meta">Recovered {time(entry.displaced_at)}</p>}
    <div className="toolbar"><Action label="Adopt" disabled={!entry.adoptable || (displaced && !entry.id)}
      action={() => displaced ? api.adoptDisplaced(entry.id!) : api.adopt(target)} onDone={changed} />
      <Confirm label={displaced ? 'Discard' : 'Overwrite'} title={displaced ? 'Discard this recovery entry?' : 'Overwrite this Harness Conflict?'}
        description={displaced ? 'Permanently remove the recovered content. The Draft is unchanged.'
          : 'Replace this Harness edit with the content in this Machine’s Draft and Baseline.'}
        action={() => displaced ? api.discardDisplaced(entry.id!) : api.overwrite(target)} onDone={changed}
        disabled={displaced && !entry.id} /></div>
    {!entry.adoptable && <p className="meta">This Harness combines Fragments in one file. Move its edits into the Draft by hand before Overwrite.</p>}
  </section>;
}
export function Conflicts({ revision, changed }: { revision: number; changed: () => void }) {
  const conflicts = useResource(api.conflicts, revision);
  const displaced = useResource(api.recovered, revision);
  return <><p className="description">A Conflict is a Harness edit that differs from this Machine's Draft.</p>
    <Notice title="Adopt or Overwrite">Adopt copies a Harness edit into the Draft. Overwrite replaces it with the Draft.</Notice>
    <ErrorNotice error={conflicts.error} />{conflicts.loading && !conflicts.data && <Loading />}
    {conflicts.data?.length === 0 && <Nothing>No Harness Conflicts.</Nothing>}
    {conflicts.data?.map((entry, i) => <Conflict key={`${entry.harness}/${entry.name}/${i}`} entry={entry} displaced={false} changed={changed} />)}
    <h2>Displaced recovery entries</h2><p className="description">Adopt copies recovered content into the Draft. Discard removes the recovery entry.</p>
    <ErrorNotice error={displaced.error} />{displaced.loading && !displaced.data && <Loading />}
    {displaced.data?.length === 0 && <Nothing>No displaced content to recover.</Nothing>}
    {displaced.data?.map(entry => <Conflict key={entry.id} entry={entry} displaced changed={changed} />)}
  </>;
}
export function Imports({ revision, changed }: { revision: number; changed: () => void }) {
  const { data, error, loading } = useResource(api.imports, revision);
  return <><p className="description">Available upstream updates. Review each update in the Draft before Promote.</p>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    {data && <><p className="meta">Last checked {time(data.checked_at)}</p>
      {data.errors.map(e => <ErrorNotice key={e.name} error={`${e.name}: ${e.message}`} />)}
      {!data.updates.length && <Nothing>No available import updates.</Nothing>}
      {data.updates.map(update => <section className="list-entry" key={update.name}><div className="row"><code>{update.name}</code>
        <Badge variant="outline">{update.ref}</Badge></div><p>{update.source}</p>
        <p className="meta">{shortSha(update.locked)} → {shortSha(update.upstream)}</p>
        <Action label="Update into Draft" action={() => api.updateImport(update.name)} onDone={changed} />
      </section>)}</>}
  </>;
}
const toggleLabel = (value: boolean | null) => value === null ? 'Default' : value ? 'On' : 'Off';
export function Log({ revision, changed }: { revision: number; changed: () => void }) {
  const [before, setBefore] = useState<number>();
  const [pages, setPages] = useState<(number | undefined)[]>([]);
  const load = useCallback(() => api.log({ before, limit: 50 }), [before]);
  const { data, error, loading } = useResource(load, revision);
  return <><p className="description">Assignment log, newest first. Undo restores an entry's previous value and adds a new log entry.</p>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    <Table><TableHeader><TableRow>{['Time', 'Assignment', 'Change', 'Cause', 'Author', ''].map((label, i) => <TableHead key={i}>{label}</TableHead>)}</TableRow></TableHeader>
      <TableBody>{data?.map(entry => <TableRow key={entry.id}><TableCell>{time(entry.at)}</TableCell>
        <TableCell><code>{entry.machine} / {entry.harness}</code><p className="meta">{entry.name ?? entry.kind.replaceAll('_', ' ')}</p></TableCell>
        <TableCell>{toggleLabel(entry.before)} → {toggleLabel(entry.after)}</TableCell>
        <TableCell><Badge variant="secondary">{entry.cause.type.replaceAll('_', ' ')}{'entry' in entry.cause ? ` #${entry.cause.entry}` : ''}</Badge></TableCell>
        <TableCell>{entry.author}</TableCell><TableCell><Action label="Undo" action={() => api.undo(entry.id)} onDone={changed} /></TableCell>
      </TableRow>)}</TableBody></Table>
    {data?.length === 0 && <Nothing>No Assignment log entries on this page.</Nothing>}
    <div className="toolbar"><Button variant="outline" disabled={!pages.length || loading} onClick={() => {
      setBefore(pages.at(-1)); setPages(p => p.slice(0, -1));
    }}>Newer</Button><Button variant="outline" disabled={!data?.length || data.length < 50 || loading} onClick={() => {
      setPages(p => [...p, before]); setBefore(data?.at(-1)?.id);
    }}>Older</Button>{before !== undefined && <Button variant="ghost" onClick={() => { setBefore(undefined); setPages([]); }}>Latest</Button>}</div>
  </>;
}
