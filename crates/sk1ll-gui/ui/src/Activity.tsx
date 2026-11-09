import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from '@/components/ui/card';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Input } from '@/components/ui/input';
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select';
import { toast } from './toast';
import { api } from './api';
import { AddImport } from './AddImport';
import { useResource } from './hooks';
import { Action, Confirm, Details, ErrorNotice, Loading, Nothing, Notice, When } from './shared';
import { ago, shortSha, time } from './model';
import type { ConflictView } from './types';

function ConflictDiff({ entry, displaced }: { entry: ConflictView; displaced: boolean }) {
  const load = useCallback(() => api.conflictDiff(displaced ? { id: entry.id! } : { harness: entry.harness, name: entry.name }), [entry.id, entry.harness, entry.name, displaced]);
  const { data, error, loading } = useResource(load, 0);
  return <><p className="text-xs text-muted-foreground">− {displaced ? 'Recovered content' : 'Harness edit'} / + Draft</p>
    <ErrorNotice error={error} />{loading && <Loading rows={2} />}
    {data && <pre className="diff overflow-auto text-xs" aria-label="Conflict diff">{data.diff ? data.diff.split('\n').map((line, i) => <div key={i} data-kind={line.startsWith('@@') ? 'hunk' : line.startsWith('+') ? 'add' : line.startsWith('-') ? 'delete' : undefined}>{line || ' '}</div>) : 'No differences.'}</pre>}</>;
}
function Conflict({ entry, displaced, changed, adopted, expanded }: {
  entry: ConflictView; displaced: boolean; expanded: boolean; changed: () => void; adopted: (name: string) => void;
}) {
  const [open, setOpen] = useState(expanded);
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
      <details open={open} onToggle={e => setOpen(e.currentTarget.open)}><summary className="cursor-pointer text-sm">Show diff</summary>{open && <ConflictDiff entry={entry} displaced={displaced} />}</details>
      {!entry.adoptable && <p className="text-xs text-muted-foreground">This Harness combines Rules in one file. Move its edits into the Draft by hand before Overwrite.</p>}
    </CardContent>
    <CardFooter className="gap-2 border-t">
      <Confirm label="Adopt" variant="default" confirmLabel="Adopt into Draft" ariaLabel={`Adopt ${entry.name} from ${entry.harness}`}
        disabled={!entry.adoptable || (displaced && !entry.id)}
        title={`Adopt ${entry.name} into the Draft?`}
        description={<>{displaced ? 'The recovered content' : `The ${entry.harness} Harness edit`} at <code className="font-mono break-all">{displaced ? entry.path ?? entry.dest : entry.dest}</code> replaces
          this item in this Machine's Draft. Any existing Draft changes to {entry.items.length ? entry.items.join(', ') : entry.name} are overwritten.</>}
        action={() => displaced ? api.adoptDisplaced(entry.id!) : api.adopt(target)} onDone={() => { adopted(entry.name); toast({ title: `${entry.name} adopted into Draft` }); changed(); }} />
      <Confirm label={displaced ? 'Discard' : 'Overwrite'}
        ariaLabel={`${displaced ? 'Discard' : 'Overwrite'} ${entry.name} in ${entry.harness}`} title={`${displaced ? 'Discard' : 'Overwrite'} ${entry.name} in ${entry.harness}?`}
        description={displaced ? <>Permanently remove recovered content for {entry.name} from {entry.harness} at <code className="font-mono break-all">{entry.path ?? entry.dest}</code>. The Draft is unchanged.</>
          : <>Replace {entry.name} in {entry.harness} at <code className="font-mono break-all">{entry.dest}</code> with the content in this Machine's Draft and Baseline.</>}
        action={() => displaced ? api.discardDisplaced(entry.id!) : api.overwrite(target)} onDone={() => { toast({ title: `${entry.name} ${displaced ? 'discarded' : 'overwritten'}` }); changed(); }}
        disabled={displaced && !entry.id} />
    </CardFooter>
  </Card>;
}
function Section({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return <section className="flex flex-col gap-3"><div><h2 className="text-base font-semibold">{title}</h2>
    <p className="text-sm text-muted-foreground">{description}</p></div>{children}</section>;
}
function InDraft({ name, reviewDraft }: { name?: string; reviewDraft: () => void }) {
  return name ? <Notice title={`${name} is in the Draft`}>Review it in the Draft, then Commit it to the Baseline.
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
      {conflicts.data?.map((entry, i) => <Conflict key={`${entry.harness}/${entry.name}/${i}`} entry={entry} expanded={(conflicts.data?.length ?? 0) + (displaced.data?.length ?? 0) === 1} displaced={false} changed={changed} adopted={setAdopted} />)}
    </Section>
    <Section title="Displaced recovery entries" description="Adopt copies recovered content into the Draft. Discard removes the recovery entry.">
      <ErrorNotice error={displaced.error} />{displaced.loading && !displaced.data && <Loading rows={2} />}
      {displaced.data?.length === 0 && <Nothing title="Nothing to recover">No displaced content to recover.</Nothing>}
      {displaced.data?.map(entry => <Conflict key={entry.id} entry={entry} expanded={(conflicts.data?.length ?? 0) + (displaced.data?.length ?? 0) === 1} displaced changed={changed} adopted={setAdopted} />)}
    </Section>
  </>;
}
export function Imports({ revision, changed, reviewDraft, openAdd = false }: { revision: number; changed: () => void; reviewDraft: () => void; openAdd?: boolean }) {
  const { data, setData, error, loading } = useResource(api.imports, revision);
  const [updated, setUpdated] = useState<string>();
  const rows = data?.imports ?? data?.updates.map(update => ({ ...update, path: '' })) ?? [];
  return <>
    <div className="flex flex-wrap items-center gap-3">{data && <p className="text-xs text-muted-foreground">Last checked {ago(data.checked_at)}</p>}
      <Action label="Check now" action={async () => setData(await api.checkImports())} />
      <div className="ml-auto"><AddImport initiallyOpen={openAdd} changed={changed} reviewDraft={reviewDraft} /></div></div>
    <InDraft name={updated} reviewDraft={reviewDraft} />
    <ErrorNotice error={error} />{loading && !data && <Loading rows={2} />}
    {data && (!rows.length ? <Nothing title="No Imports yet">Add an Import to bring Skills from a git source into the Draft.</Nothing> : <Table aria-label="Imports">
      <TableHeader><TableRow>{['Name', 'Source', 'Ref', 'Path', 'Locked commit', 'Status', 'Actions'].map(label => <TableHead key={label}>{label}</TableHead>)}</TableRow></TableHeader>
      <TableBody>{rows.map(entry => {
        const update = data.updates.find(u => u.name === entry.name), failure = data.errors.find(e => e.name === entry.name);
        return <TableRow key={entry.name}><TableCell>{entry.name}</TableCell><TableCell className="max-w-64 break-all whitespace-normal">{entry.source}</TableCell><TableCell>{entry.ref}</TableCell><TableCell>{entry.path || '—'}</TableCell><TableCell><code title={entry.locked ?? undefined}>{shortSha(entry.locked)}</code></TableCell>
          <TableCell>{failure ? <span title={failure.message}>Check failed: {failure.message}</span> : update ? `Update available ${shortSha(update.locked)}→${shortSha(update.upstream)}` : 'Up to date'}</TableCell>
          <TableCell><div className="flex gap-2">{update && <Action label="Update into Draft" ariaLabel={`Update ${entry.name} into Draft`} action={() => api.updateImport(entry.name)} toastOnDone={`${entry.name} updated into Draft`} onDone={() => { setUpdated(entry.name); changed(); }} />}
            <Confirm label="Remove" ariaLabel={`Remove ${entry.name}`} title={`Remove ${entry.name}?`} description="Remove this Import into the Draft. Review and Commit the removal to update the Baseline."
              action={() => api.removeImport(entry.name)} onDone={() => { setUpdated(`${entry.name} removal`); setData(previous => previous && { ...previous, imports: rows.filter(row => row.name !== entry.name), updates: previous.updates.filter(row => row.name !== entry.name), errors: previous.errors.filter(row => row.name !== entry.name) }); toast({ title: `${entry.name} removal is in the Draft` }); changed(); }} /></div></TableCell></TableRow>;
      })}</TableBody></Table>)}
  </>;
}

const toggleLabel = (value: boolean | null) => value === null ? 'Default' : value ? 'On' : 'Off';
export function Log({ revision, changed }: { revision: number; changed: () => void }) {
  const log = useRef<HTMLDivElement>(null);
  const restoreFocus = useRef(false);
  const [machine, setMachine] = useState(''), [harness, setHarness] = useState(''), [author, setAuthor] = useState(''), [item, setItem] = useState('');
  const [before, setBefore] = useState<number>();
  const [pages, setPages] = useState<(number | undefined)[]>([]);
  const load = useCallback(() => api.log({ before, limit: 50 }), [before]);
  const { data, error, loading } = useResource(load, revision);
  useEffect(() => {
    if (restoreFocus.current && document.activeElement === document.body) {
      (log.current?.querySelector<HTMLElement>('[role="region"]') ?? log.current)?.focus();
      restoreFocus.current = false;
    }
  }, [data]);
  return <>
    <ErrorNotice error={error} />{loading && !data && <Loading />}
    <div className="flex flex-wrap items-center gap-2">{(['machine', 'harness', 'author'] as const).map(key => <NativeSelect key={key} aria-label={`Filter by ${key}`} value={{ machine, harness, author }[key]} onChange={e => ({ machine: setMachine, harness: setHarness, author: setAuthor }[key])(e.target.value)}>
      <NativeSelectOption value="">All {key === 'machine' ? 'Machines' : key === 'harness' ? 'Harnesses' : 'authors'}</NativeSelectOption>{[...new Set(data?.map(entry => entry[key]) ?? [])].sort().map(value => <NativeSelectOption key={value} value={value}>{value}</NativeSelectOption>)}</NativeSelect>)}
      <Input aria-label="Search items" placeholder="Search items…" value={item} onChange={e => setItem(e.target.value)} /></div>
    <div ref={log} tabIndex={-1} role={!data?.length ? 'region' : undefined} aria-label={!data?.length ? 'Assignment log' : undefined} className="outline-none">{data?.length === 0 ? <Nothing title="No log entries">No Assignment log entries on this page.</Nothing> : data && <div className="overflow-hidden rounded-lg border"><Table aria-label="Assignment log">
      <TableHeader><TableRow className="bg-muted/50 hover:bg-muted/50">{['Time', 'Assignment', 'Change', 'Cause', 'Author'].map(label =>
        <TableHead key={label}>{label}</TableHead>)}<TableHead><span className="sr-only">Actions</span></TableHead></TableRow></TableHeader>
      <TableBody>{data.filter(entry => (!machine || entry.machine === machine) && (!harness || entry.harness === harness) && (!author || entry.author === author) && `${entry.kind}:${entry.name ?? ''}`.toLowerCase().includes(item.toLowerCase())).map(entry => <TableRow className="group" key={entry.id}>
        <TableCell className="text-muted-foreground"><When seconds={entry.at} /></TableCell>
        <TableCell><code className="font-mono text-xs">{entry.machine} / {entry.harness}</code>
          <p className="text-xs text-muted-foreground">{entry.name === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name}`}</p></TableCell>
        <TableCell className="whitespace-nowrap">{toggleLabel(entry.before)} <span className="text-muted-foreground">→</span> {toggleLabel(entry.after)}</TableCell>
        <TableCell>{entry.cause.type !== 'change' && <Badge variant="secondary">{entry.cause.type.replaceAll('_', ' ')}{'entry' in entry.cause ? ` #${entry.cause.entry}` : ''}</Badge>}</TableCell>
        <TableCell className="text-muted-foreground">{entry.author}</TableCell>
        <TableCell data-log-id={entry.id} className="[@media(hover:hover)]:opacity-0 group-hover:opacity-100 focus-within:opacity-100 w-24 min-w-24 max-w-24 whitespace-normal xl:w-36 xl:min-w-36 xl:max-w-36"><Action announceDone={false} className="h-10 w-full whitespace-normal xl:whitespace-nowrap" label={`Restore ${toggleLabel(entry.before)}`} doneLabel="Restored" variant="ghost"
          ariaLabel={`Restore ${toggleLabel(entry.before)} for log entry ${entry.id}: ${entry.name === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name}`} on ${entry.machine} / ${entry.harness}`} action={() => { restoreFocus.current = !!log.current?.contains(document.activeElement); return api.undo(entry.id); }} onDone={() => {
            toast({ title: `Restored Assignment ${entry.name === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name}`} on ${entry.machine} / ${entry.harness} to ${toggleLabel(entry.before)}.` }); changed();
          }} /></TableCell>
      </TableRow>)}</TableBody></Table></div>}</div>
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
