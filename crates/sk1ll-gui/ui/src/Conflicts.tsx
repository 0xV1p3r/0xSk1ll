import { useCallback, useState, type ReactNode } from 'react';
import { ChevronRight } from 'lucide-react';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from '@/components/ui/card';
import { toast } from './toast';
import { api } from './api';
import { useResource } from './hooks';
import { Confirm, Details, DiffLines, ErrorNotice, Loading, Nothing, Notice, SectionTitle, When } from './shared';
import type { ConflictView } from './types';

function ConflictDiff({ entry, displaced }: { entry: ConflictView; displaced: boolean }) {
  const load = useCallback(() => api.conflictDiff(displaced ? { id: entry.id! } : { harness: entry.harness, name: entry.name }), [entry.id, entry.harness, entry.name, displaced]);
  const { data, error, loading } = useResource(load, 0);
  return <><p className="text-xs text-muted-foreground">− {displaced ? 'Recovered content' : 'Harness edit'} / + Draft</p>
    <ErrorNotice error={error} />{loading && <Loading rows={2} />}
    {data?.target && <p className="text-xs break-all">Draft destination <code className="font-mono">{data.target}</code></p>}
    {data && <pre className="diff max-h-[32rem] overflow-auto rounded-lg border border-border bg-muted/40 p-3 font-mono text-xs" aria-label="Conflict diff">{data.diff ? <DiffLines diff={data.diff} /> : 'No differences.'}</pre>}</>;
}
function Conflict({ entry, displaced, changed, adopted, expanded }: {
  entry: ConflictView; displaced: boolean; expanded: boolean; changed: () => void; adopted: (name: string) => void;
}) {
  const [open, setOpen] = useState(expanded);
  const target = { harness: entry.harness, name: entry.name };
  // Recovered content needs the user only while it holds a change they didn't choose to overwrite;
  // Adopt is offered only where it can tell where it goes.
  const attention = !displaced || (!!entry.changed && !entry.overwritten);
  const canAdopt = !displaced || entry.adoptable;
  return <Card size="default" className="relative overflow-hidden pt-0">
    <span aria-hidden className={`absolute inset-y-3 left-0 w-1 rounded-r-full ${attention ? 'bg-warning' : 'bg-info'}`} />
    <CardHeader className="border-b border-border bg-muted/40 py-3">
      <CardTitle className="flex flex-wrap items-center gap-2"><span className="font-medium">{entry.name}</span>
        <Badge variant="outline">{entry.harness}</Badge>
        {displaced && <Badge variant={entry.overwritten ? 'info' : entry.changed ? 'warning' : 'secondary'}>{entry.overwritten ? 'Overwritten' : entry.changed ? 'Changed' : 'Unchanged'}</Badge>}</CardTitle>
      <CardDescription className="font-mono text-xs break-all">{entry.dest}</CardDescription>
      {!displaced && typeof entry.detected_at === 'number' && <p className="text-xs text-muted-foreground">Edited <When seconds={entry.detected_at} /></p>}
    </CardHeader>
    <CardContent className="flex flex-col gap-2">
      {entry.reason && <p>{entry.reason}</p>}
      {displaced && <p className="text-xs text-muted-foreground">{entry.overwritten
        ? 'Overwritten: your edit is kept here. Adopt it to bring it back into the Draft, or Discard it once you no longer need it.'
        : entry.changed
          ? 'It holds a change no other copy may have. Adopt it, or Discard it once you no longer need it.'
          : 'Unchanged since sk1ll set it aside. A routine copy is removed on its own after a while.'}</p>}
      {(entry.path || entry.displaced_at !== undefined) && <Details className="text-xs" rows={[
        ...entry.path ? [['Recovery path', <code className="font-mono break-all">{entry.path}</code>] as [string, ReactNode]] : [],
        ...entry.displaced_at !== undefined ? [['Recovered', <When seconds={entry.displaced_at} />] as [string, ReactNode]] : [],
      ]} />}
      <Collapsible open={open} onOpenChange={setOpen}><CollapsibleTrigger render={<Button variant="ghost" className="group/diff" />}><ChevronRight className="transition-transform group-data-panel-open/diff:rotate-90" />{open ? 'Hide diff' : 'Show diff'}</CollapsibleTrigger><CollapsibleContent>{open && <ConflictDiff entry={entry} displaced={displaced} />}</CollapsibleContent></Collapsible>
      {!entry.adoptable && <p className="text-xs text-muted-foreground">{displaced
        ? 'sk1ll can\'t tell which item this came from, so it can\'t be adopted. Copy what you need from the recovery path by hand.'
        : 'This Harness combines Rules in one file. Move its edits into the Draft by hand before Overwrite.'}</p>}
    </CardContent>
    <CardFooter className="flex-wrap gap-2 border-t [&_button]:h-8">
      {canAdopt && <Confirm label="Adopt" variant="default" triggerVariant="outline" confirmLabel="Adopt into Draft" ariaLabel={`Adopt ${entry.name} from ${entry.harness}`}
        disabled={!entry.adoptable || (displaced && !entry.id)}
        title={`Adopt ${entry.name} into the Draft?`}
        description={<>{displaced ? 'The recovered content' : `The ${entry.harness} Harness edit`} at <code className="font-mono break-all">{displaced ? entry.path ?? entry.dest : entry.dest}</code> replaces
          this item in this Machine's Draft. Any existing Draft changes to {entry.items.length ? entry.items.join(', ') : entry.name} are overwritten.</>}
        action={async () => {
          const path = displaced ? await api.adoptDisplaced(entry.id!) : undefined;
          if (!displaced) await api.adopt(target);
          const name = path?.split(/[/\\]/).at(-1) ?? entry.name;
          adopted(name);
          toast({ title: `${name} adopted into Draft`, description: path });
        }} onDone={changed} />}
      <Confirm label={displaced ? 'Discard' : 'Overwrite'} triggerVariant="outline"
        ariaLabel={`${displaced ? 'Discard' : 'Overwrite'} ${entry.name} in ${entry.harness}`} title={`${displaced ? 'Discard' : 'Overwrite'} ${entry.name} in ${entry.harness}?`}
        description={displaced ? <>Permanently remove recovered content for {entry.name} from {entry.harness} at <code className="font-mono break-all">{entry.path ?? entry.dest}</code>. The Draft is unchanged.</>
          : <>Replace {entry.name} in {entry.harness} at <code className="font-mono break-all">{entry.dest}</code> with the content in this Machine's Draft and Baseline.</>}
        action={() => displaced ? api.discardDisplaced(entry.id!) : api.overwrite(target)} onDone={() => { toast({ title: `${entry.name} ${displaced ? 'discarded' : 'overwritten'}` }); changed(); }}
        disabled={displaced && !entry.id} />
    </CardFooter>
  </Card>;
}
function Section({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return <section className="flex flex-col gap-3"><div><SectionTitle>{title}</SectionTitle>
    <p className="text-sm text-muted-foreground">{description}</p></div>{children}</section>;
}
export function InDraft({ name, reviewDraft }: { name?: string; reviewDraft: () => void }) {
  return name ? <Notice title={`${name} is in the Draft`}>Review it in the Draft, then Commit it to the Baseline.
    <div><Button variant="outline" size="default" className="mt-2" onClick={reviewDraft}>Review Draft</Button></div></Notice> : null;
}
export function Conflicts({ revision, changed, reviewDraft }: { revision: number; changed: () => void; reviewDraft: () => void }) {
  const [adopted, setAdopted] = useState<string>();
  const conflicts = useResource(api.conflicts, revision);
  const displaced = useResource(api.recovered, revision);
  const ready = (!conflicts.loading || !!conflicts.data) && (!displaced.loading || !!displaced.data);
  const total = (conflicts.data?.length ?? 0) + (displaced.data?.length ?? 0);
  return <>
    <InDraft name={adopted} reviewDraft={reviewDraft} />
    <ErrorNotice error={conflicts.error} /><ErrorNotice error={displaced.error} />
    {!ready && <Loading rows={2} />}
    {ready && conflicts.data?.length === 0 && displaced.data?.length === 0 && <Nothing title="No Conflicts">Every Harness matches this Machine's Draft.</Nothing>}
    {ready && !!conflicts.data?.length && <Section title="Harness conflicts" description="Adopt copies a Harness edit into the Draft. Overwrite replaces it with the Draft.">
      {conflicts.data.map((entry, i) => <Conflict key={`${entry.harness}/${entry.name}/${i}`} entry={entry} expanded={total <= 3} displaced={false} changed={changed} adopted={setAdopted} />)}
    </Section>}
    {ready && !!displaced.data?.length && <Section title="Recovered content" description="Content saved when a Harness file was replaced. Changed content needs your decision; an edit you overwrote waits here until you adopt or discard it. Adopt copies it into the Draft. Discard permanently removes the saved content.">
      {displaced.data.map(entry => <Conflict key={entry.id} entry={entry} expanded={total <= 3} displaced changed={changed} adopted={setAdopted} />)}
    </Section>}
  </>;
}
