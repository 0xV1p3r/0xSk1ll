import { useCallback, useState, type ReactNode } from 'react';
import { ArrowRight, ChevronLeft, ChevronRight } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardAction, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api } from './api';
import { useResource } from './hooks';
import { Action, Confirm, Details, ErrorNotice, Loading, Nothing, Notice, When } from './shared';
import { ago, shortSha, time } from './model';
import type { ConflictView } from './types';

function Conflict({ entry, displaced, changed, adopted }: {
  entry: ConflictView; displaced: boolean; changed: () => void; adopted: (name: string) => void;
}) {
  const target = { harness: entry.harness, name: entry.name };
  return <Card size="sm">
    <CardHeader>
      <CardTitle className="flex flex-wrap items-center gap-2"><code className="font-mono">{entry.name}</code>
        <Badge variant="outline">{entry.harness}</Badge></CardTitle>
      <CardDescription className="font-mono text-xs break-all">{entry.dest}</CardDescription>
    </CardHeader>
    <CardContent className="flex flex-col gap-2">
      {entry.reason && <p>{entry.reason}</p>}
      {(entry.path || entry.displaced_at !== undefined) && <Details className="text-xs" rows={[
        ...entry.path ? [['Recovery path', <code className="font-mono break-all">{entry.path}</code>] as [string, ReactNode]] : [],
        ...entry.displaced_at !== undefined ? [['Recovered', time(entry.displaced_at)] as [string, ReactNode]] : [],
      ]} />}
      {!entry.adoptable && <p className="text-xs text-muted-foreground">This Harness combines Fragments in one file. Move its edits into the Draft by hand before Overwrite.</p>}
    </CardContent>
    <CardFooter className="gap-2 border-t">
      <Confirm label="Adopt" variant="default" confirmLabel="Adopt into Draft" ariaLabel={`Adopt ${entry.name} from ${entry.harness}`}
        disabled={!entry.adoptable || (displaced && !entry.id)}
        title={`Adopt ${entry.name} into the Draft?`}
        description={<>{displaced ? 'The recovered content' : `The ${entry.harness} Harness edit`} at <code className="font-mono break-all">{displaced ? entry.path ?? entry.dest : entry.dest}</code> replaces
          this item in this Machine's Draft. Any existing Draft changes to {entry.items.length ? entry.items.join(', ') : entry.name} are overwritten.</>}
        action={() => displaced ? api.adoptDisplaced(entry.id!) : api.adopt(target)} onDone={() => { adopted(entry.name); changed(); }} />
      <Confirm label={displaced ? 'Discard' : 'Overwrite'}
        ariaLabel={`${displaced ? 'Discard' : 'Overwrite'} ${entry.name} in ${entry.harness}`} title={displaced ? 'Discard this recovery entry?' : 'Overwrite this Harness Conflict?'}
        description={displaced ? 'Permanently remove the recovered content. The Draft is unchanged.'
          : 'Replace this Harness edit with the content in this Machine’s Draft and Baseline.'}
        action={() => displaced ? api.discardDisplaced(entry.id!) : api.overwrite(target)} onDone={changed}
        disabled={displaced && !entry.id} />
    </CardFooter>
  </Card>;
}
function Section({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return <section className="flex flex-col gap-3"><div><h2 className="text-base font-semibold">{title}</h2>
    <p className="text-sm text-muted-foreground">{description}</p></div>{children}</section>;
}
function InDraft({ name, reviewDraft }: { name?: string; reviewDraft: () => void }) {
  return name ? <Notice title={`${name} is in the Draft`}>Review it in the Draft, then Promote it to the Baseline.
    <div><Button variant="outline" size="sm" className="mt-2" onClick={reviewDraft}>Review Draft</Button></div></Notice> : null;
}
export function Conflicts({ revision, changed, reviewDraft }: { revision: number; changed: () => void; reviewDraft: () => void }) {
  const [adopted, setAdopted] = useState<string>();
  const conflicts = useResource(api.conflicts, revision);
  const displaced = useResource(api.recovered, revision);
  return <>
    <InDraft name={adopted} reviewDraft={reviewDraft} />
    <Section title="Harness Conflicts" description="Adopt copies a Harness edit into the Draft. Overwrite replaces it with the Draft.">
      <ErrorNotice error={conflicts.error} />{conflicts.loading && !conflicts.data && <Loading rows={2} />}
      {conflicts.data?.length === 0 && <Nothing title="No Conflicts">Every Harness matches this Machine's Draft.</Nothing>}
      {conflicts.data?.map((entry, i) => <Conflict key={`${entry.harness}/${entry.name}/${i}`} entry={entry} displaced={false} changed={changed} adopted={setAdopted} />)}
    </Section>
    <Section title="Displaced recovery entries" description="Adopt copies recovered content into the Draft. Discard removes the recovery entry.">
      <ErrorNotice error={displaced.error} />{displaced.loading && !displaced.data && <Loading rows={2} />}
      {displaced.data?.length === 0 && <Nothing title="Nothing to recover">No displaced content to recover.</Nothing>}
      {displaced.data?.map(entry => <Conflict key={entry.id} entry={entry} displaced changed={changed} adopted={setAdopted} />)}
    </Section>
  </>;
}
export function Imports({ revision, changed, reviewDraft }: { revision: number; changed: () => void; reviewDraft: () => void }) {
  const { data, error, loading } = useResource(api.imports, revision);
  const [updated, setUpdated] = useState<string>();
  return <>
    <InDraft name={updated} reviewDraft={reviewDraft} />
    <ErrorNotice error={error} />{loading && !data && <Loading rows={2} />}
    {data && <><p className="text-xs text-muted-foreground">Last checked {ago(data.checked_at)}</p>
      {data.errors.map(e => <ErrorNotice key={e.name} title={`Could not check ${e.name}`}
        error={`${e.message}. Check this import's repository URL and access credentials.`} />)}
      {!data.updates.length && !data.errors.length && <Nothing title="Up to date">No available import updates.</Nothing>}
      {!data.updates.length && !!data.errors.length && <p className="text-sm text-muted-foreground">No updates found among the imports that could be checked.</p>}
      {data.updates.map(update => <Card size="sm" key={update.name}>
        <CardHeader><CardTitle className="flex flex-wrap items-center gap-2"><code className="font-mono">{update.name}</code>
          <Badge variant="outline">{update.ref}</Badge></CardTitle>
          <CardDescription className="break-all">{update.source}</CardDescription>
          <CardAction><Action label="Update into Draft" variant="default" ariaLabel={`Update ${update.name} into Draft`} action={() => api.updateImport(update.name)} onDone={() => { setUpdated(update.name); changed(); }} /></CardAction>
        </CardHeader>
        <CardContent className="flex items-center gap-2 font-mono text-xs"><span className="text-muted-foreground">{shortSha(update.locked)}</span>
          <ArrowRight className="size-3 text-muted-foreground" aria-label="to" /><span>{shortSha(update.upstream)}</span></CardContent>
      </Card>)}</>}
  </>;
}
const toggleLabel = (value: boolean | null) => value === null ? 'Default' : value ? 'On' : 'Off';
export function Log({ revision, changed }: { revision: number; changed: () => void }) {
  const [before, setBefore] = useState<number>();
  const [pages, setPages] = useState<(number | undefined)[]>([]);
  const load = useCallback(() => api.log({ before, limit: 50 }), [before]);
  const { data, error, loading } = useResource(load, revision);
  return <>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    {data?.length === 0 ? <Nothing title="No log entries">No Assignment log entries on this page.</Nothing> : data && <div className="overflow-hidden rounded-lg border"><Table aria-label="Assignment log">
      <TableHeader><TableRow className="bg-muted/50 hover:bg-muted/50">{['Time', 'Assignment', 'Change', 'Cause', 'Author'].map(label =>
        <TableHead key={label}>{label}</TableHead>)}<TableHead><span className="sr-only">Actions</span></TableHead></TableRow></TableHeader>
      <TableBody>{data.map(entry => <TableRow key={entry.id}>
        <TableCell className="text-muted-foreground"><When seconds={entry.at} /></TableCell>
        <TableCell><code className="font-mono text-xs">{entry.machine} / {entry.harness}</code>
          <p className="text-xs text-muted-foreground">{entry.name === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name}`}</p></TableCell>
        <TableCell className="whitespace-nowrap">{toggleLabel(entry.before)} <span className="text-muted-foreground">→</span> {toggleLabel(entry.after)}</TableCell>
        <TableCell><Badge variant="secondary">{entry.cause.type.replaceAll('_', ' ')}{'entry' in entry.cause ? ` #${entry.cause.entry}` : ''}</Badge></TableCell>
        <TableCell className="text-muted-foreground">{entry.author}</TableCell>
        <TableCell className="text-right"><Action label="Undo" doneLabel="Undone" variant="ghost"
          ariaLabel={`Undo log entry ${entry.id}: ${entry.name === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name}`} on ${entry.machine} / ${entry.harness}`} action={() => api.undo(entry.id)} onDone={changed} /></TableCell>
      </TableRow>)}</TableBody></Table></div>}
    <div className="flex items-center justify-end gap-2">
      {before !== undefined && <Button variant="ghost" size="sm" onClick={() => { setBefore(undefined); setPages([]); }}>Latest</Button>}
      <Button variant="outline" size="sm" disabled={!pages.length || loading} onClick={() => {
        setBefore(pages.at(-1)); setPages(p => p.slice(0, -1));
      }}><ChevronLeft data-icon="inline-start" />Newer</Button>
      <Button variant="outline" size="sm" disabled={!data?.length || data.length < 50 || loading} onClick={() => {
        setPages(p => [...p, before]); setBefore(data?.at(-1)?.id);
      }}>Older<ChevronRight data-icon="inline-end" /></Button>
    </div>
  </>;
}
