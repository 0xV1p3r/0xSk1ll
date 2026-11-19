import { toast } from './toast';
import { useEffect, useRef, useState } from 'react';
import { Check, CircleAlert, ChevronRight } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Field, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { api, errorMessage } from './api';
import { ErrorNotice } from './shared';
import { plural, shortSha } from './model';
import type { ImportEntry, ItemView, ResolvedSource } from './types';

function importError(error: unknown) {
  const message = errorMessage(error);
  return /fetch|clone|repository|remote|authentication|credentials|access denied|permission denied|could not resolve host|connection|network|https?:\/\//i.test(message)
    ? `${message} Check this import's repository URL and access credentials.` : message;
}

// Per selected candidate: added (`confirmed` when only the Daemon's Imports showed it landed), failed, or not attempted (the batch was stopped).
type Outcome = { name: string } & ({ state: 'added'; confirmed?: boolean } | { state: 'failed'; error: string } | { state: 'skipped' });
// Like the Daemon's own check: any Skill in the inventory or recorded Import goes by the name.
const takenNames = (inventory: ItemView[], imports: ImportEntry[] = []) => [...new Set([
  ...inventory.filter(i => i.item.startsWith('skill:') && i.draft !== 'deleted').map(i => i.item.slice(6)), ...imports.map(i => i.name)])];
const outcomeMessage = (outcome: Outcome) => outcome.state === 'added' ? `${outcome.name} added into Draft${outcome.confirmed ? ' (confirmed after an error response)' : ''}.`
  : outcome.state === 'failed' ? `${outcome.name} not added: ${outcome.error}` : `${outcome.name} not attempted.`;

export function AddImport({ changed, reviewDraft, initiallyOpen = false }: {
  changed: () => void; reviewDraft: () => void; initiallyOpen?: boolean | number;
}) {
  const [open, setOpen] = useState(!!initiallyOpen);
  // A new truthy request (a counter from App) reopens the dialog on a mounted screen.
  useEffect(() => { if (initiallyOpen) setOpen(true); }, [initiallyOpen]);
  const trigger = useRef<HTMLButtonElement>(null);
  const busy = useRef(false);
  return <><Button ref={trigger} onClick={() => setOpen(true)}>Add Import</Button>
    <Dialog open={open} onOpenChange={value => { if (!busy.current) setOpen(value); }}>{open && <AddImportForm finalFocus={trigger} busy={busy} changed={changed}
      reviewDraft={() => { setOpen(false); reviewDraft(); }} />}</Dialog></>;
}
function AddImportForm({ busy, changed, reviewDraft, finalFocus }: {
  finalFocus: React.RefObject<HTMLButtonElement | null>; busy: { current: boolean }; changed: () => void; reviewDraft: () => void;
}) {
  const [link, setLink] = useState('');
  const [path, setPath] = useState('');
  const [resolved, setResolved] = useState<ResolvedSource>();
  const [taken, setTaken] = useState<string[]>([]);
  const [names, setNames] = useState<string[]>([]);
  const [selected, setSelected] = useState<boolean[]>([]);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState('');
  const [outcomes, setOutcomes] = useState<Record<number, Outcome>>({});
  // Completed batches; each one moves focus to its result.
  const [runs, setRuns] = useState(0);
  const linkInput = useRef<HTMLInputElement>(null);
  const candidates = useRef<HTMLDivElement>(null);
  const [announcement, setAnnouncement] = useState('');
  const reviewButton = useRef<HTMLButtonElement>(null);
  const errorSummary = useRef<HTMLDivElement>(null);
  // Set by Stop, or when the dialog goes away mid-batch: the Skills not yet attempted stay so.
  const stopped = useRef(false);
  useEffect(() => () => { stopped.current = true; }, []);
  useEffect(() => {
    if (runs) (error ? errorSummary.current : reviewButton.current)?.focus();
  }, [runs, error]);
  useEffect(() => {
    if (resolved) (candidates.current?.querySelector<HTMLElement>('[role=checkbox], input') ?? candidates.current?.querySelector('h2'))?.focus();
    else linkInput.current?.focus();
  }, [resolved]);
  const resolve = async () => {
    if (busy.current) return;
    busy.current = true; setPending(true); setError(''); setAnnouncement(''); setResolved(undefined);
    try {
      const [source, inventory] = await Promise.all([api.resolveImport({ link: link.trim(), path: path.trim() }), api.items()]);
      setTaken(takenNames(inventory));
      setResolved(source); setAnnouncement(source.candidates.length ? `${source.candidates.length} ${source.candidates.length === 1 ? 'Skill' : 'Skills'} found` : ''); setNames(source.candidates.map(c => c.default_name));
      setSelected(source.candidates.map(() => source.candidates.length === 1));

    } catch (e) { setError(importError(e)); }
    finally { busy.current = false; setPending(false); }
  };
  const added = (i: number) => outcomes[i]?.state === 'added';
  // Selected candidates still to add: never attempted, failed, or not attempted in an earlier batch.
  const todo = selected.flatMap((on, i) => on && !added(i) ? [i] : []);
  // Wrong names block adding; a clash with what the Daemon last reported only blocks the first batch, since every retry checks again.
  const invalid = names.map((name, i) => added(i) ? '' : !name ? 'Enter a name.'
    : !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name) ? 'Start with a letter or number; use only letters, numbers, dots, underscores and hyphens.'
    : names.some((other, j) => j !== i && selected[j] && other === name) ? 'This name is already taken.' : '');
  const clashes = names.map((name, i) => !added(i) && (taken.includes(name) || (!!resolved?.candidates[i].taken && name === resolved.candidates[i].default_name)));
  const problems = names.map((_, i) => invalid[i] || (clashes[i] ? 'This name is already taken.' : ''));
  const blocked = todo.some(i => !!invalid[i] || (runs === 0 && clashes[i]));
  // Only an Import of exactly this request counts as landed: same name, source, path, ref and locked commit.
  const landed = (imports: ImportEntry[] | undefined, i: number) => !!resolved && !!imports?.some(entry => entry.name === names[i]
    && entry.source === resolved.source && entry.path === resolved.candidates[i].path && entry.ref === resolved.git_ref && entry.locked === resolved.commit);
  // Attempts every selected candidate still to add, each on its own, then reads back from the Daemon what landed.
  const add = async () => {
    if (!resolved || busy.current || !todo.length || blocked) return;
    busy.current = true; stopped.current = false; setPending(true); setError(''); setAnnouncement('');
    const next = { ...outcomes };
    let batch = todo;
    if (runs > 0) {
      // Names may have been taken since the last batch; a retry checks them against the Daemon first.
      try {
        const [inventory, view] = await Promise.all([api.items(), api.imports()]);
        const now = takenNames(inventory, view.imports);
        setTaken(now);
        for (const i of batch) if (landed(view.imports, i)) next[i] = { name: names[i], state: 'added', confirmed: true };
        batch = batch.filter(i => next[i]?.state !== 'added');
        // A name taken now fails on its own; the others still go ahead.
        for (const i of batch) if (now.includes(names[i])) next[i] = { name: names[i], state: 'failed', error: 'This name is taken now. Pick another, then retry.' };
        batch = batch.filter(i => !now.includes(names[i]));
      } catch { /* The Daemon checks every name again as it adds. */ }
    }
    for (const i of batch) {
      const name = names[i];
      if (stopped.current) { next[i] = { name, state: 'skipped' }; continue; }
      try {
        await api.addImport({ name, source: resolved.source, path: resolved.candidates[i].path, ref: resolved.git_ref, commit: resolved.commit });
        next[i] = { name, state: 'added' };
        setAnnouncement(`Added ${name} into Draft.`); toast({ title: `Added ${name} into Draft.` });
      } catch (e) {
        // Even an unreachable Daemon is tried again for the next one: it may be back.
        next[i] = { name, state: 'failed', error: importError(e) };
      }
      setOutcomes({ ...next });
    }
    let checked = true;
    try {
      // A failed response may follow a successful write.
      const [inventory, view] = await Promise.all([api.items(), api.imports()]);
      setTaken(takenNames(inventory, view.imports));
      for (const i of batch) if (next[i].state === 'failed' && landed(view.imports, i)) next[i] = { name: names[i], state: 'added', confirmed: true };
    } catch { checked = false; }
    setOutcomes(next);
    const left = todo.filter(i => next[i].state !== 'added').length;
    if (left) setAnnouncement('');
    setError(left ? `${plural(left, 'Skill')} not added. Fix ${left === 1 ? 'its name' : 'their names'} if needed, then retry.${checked ? '' : ' Could not check what reached the Draft; review it before retrying.'}` : '');
    busy.current = false; setPending(false); setRuns(n => n + 1); changed();
  };
  const ran = runs > 0;
  return <DialogContent finalFocus={finalFocus} showCloseButton={!pending}>
    <DialogHeader><DialogTitle>Add Import</DialogTitle><DialogDescription>Resolve a link, then add Skills into this Machine's Draft.</DialogDescription></DialogHeader>
    {!resolved && <form onSubmit={event => { event.preventDefault(); if (link.trim()) void resolve(); }}><FieldGroup><Field><FieldLabel htmlFor="import-link">Link</FieldLabel>
      <Input ref={linkInput} id="import-link" placeholder="https://github.com/owner/repo" value={link} disabled={pending} onChange={e => setLink(e.target.value)} /></Field>
      <Collapsible><CollapsibleTrigger render={<Button variant="ghost" className="group/advanced -ml-2.5" disabled={pending} />}><ChevronRight className="transition-transform group-data-panel-open/advanced:rotate-90" />Advanced</CollapsibleTrigger>
        <CollapsibleContent><Field><FieldLabel htmlFor="import-path">Path (subdirectory for other git URLs)</FieldLabel>
          <Input id="import-path" value={path} disabled={pending} onChange={e => setPath(e.target.value)} /></Field></CollapsibleContent></Collapsible>
      <Button type="submit" disabled={pending || !link.trim()}>{pending ? 'Resolving…' : 'Resolve'}</Button></FieldGroup></form>}
    {resolved && <form id="add-import" onSubmit={event => { event.preventDefault(); void add(); }}><dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 rounded-xl border border-border bg-muted/40 p-3 text-xs break-all [&_dt]:text-muted-foreground [&_dd]:font-mono"><dt>Source</dt><dd>{resolved.source}</dd><dt>Ref</dt><dd>{resolved.git_ref}</dd><dt>Commit</dt><dd>{shortSha(resolved.commit)}</dd></dl>
      <FieldGroup className="rounded-xl border border-border bg-card p-4" ref={candidates}><h2 tabIndex={-1} className="font-medium outline-none">Resolved Skills</h2>{resolved.candidates.map((candidate, i) => <Field key={candidate.path} data-invalid={selected[i] && !!problems[i]}>
        {resolved.candidates.length > 1 && <FieldLabel><Checkbox aria-label={`Select ${candidate.path || 'root'}`} checked={selected[i]} disabled={pending || added(i)}
          onCheckedChange={checked => setSelected(s => s.map((v, j) => j === i ? !!checked : v))} />{candidate.path || '(root)'}</FieldLabel>}
        <p>{candidate.description}</p>
        {selected[i] && <><FieldLabel htmlFor={`import-name-${i}`}>Name{resolved.candidates.length > 1 ? ` for ${candidate.path || 'root'}` : ''}</FieldLabel>
          <Input id={`import-name-${i}`} value={names[i]} disabled={pending || added(i)} aria-invalid={!!problems[i]} aria-describedby={problems[i] ? `import-name-error-${i}` : undefined}
            onChange={e => setNames(n => n.map((v, j) => j === i ? e.target.value : v))} />
          {problems[i] && <FieldError id={`import-name-error-${i}`}>{problems[i]}</FieldError>}</>}
      </Field>)}</FieldGroup>{!resolved.candidates.length && <p className="rounded-xl bg-muted p-4 text-sm">No Skills found at this source. Try a subdirectory under Advanced → Path using Change source.</p>}</form>}
    <div ref={errorSummary} tabIndex={-1} className="outline-none empty:hidden"><ErrorNotice error={error || undefined} /></div>
    {Object.keys(outcomes).length > 0 && <ul aria-label="Import results" className="space-y-2 text-sm">{Object.entries(outcomes).map(([i, outcome]) => <li key={i} className="flex items-start gap-2">
      {outcome.state === 'added' ? <Check aria-hidden="true" className="size-4 shrink-0 text-green-600" /> : <CircleAlert aria-hidden="true" className={outcome.state === 'failed' ? 'size-4 shrink-0 text-destructive' : 'size-4 shrink-0 text-muted-foreground'} />}
      <span className="min-w-0 break-words">{outcomeMessage(outcome)}</span></li>)}</ul>}
    <p role="status" aria-live="polite" aria-atomic="true" className="sr-only">{announcement}</p>
    {resolved && <DialogFooter>{!ran && <Button variant="outline" disabled={pending} onClick={() => { setResolved(undefined); setTaken([]); setNames([]); setSelected([]); setError(''); setOutcomes({}); setAnnouncement(''); }}>Change source</Button>}
      {ran && <Button ref={reviewButton} variant={todo.length ? 'outline' : 'default'} disabled={pending} onClick={reviewDraft}>Review in Draft</Button>}
      {pending && todo.length > 1 && <Button variant="outline" onClick={() => { stopped.current = true; }}>Stop after this Skill</Button>}
      {(!ran || todo.length > 0) && <Button type="submit" form="add-import" disabled={pending || !todo.length || blocked}>{pending ? 'Adding…' : ran ? `Retry ${plural(todo.length, 'Skill')}` : 'Add into Draft'}</Button>}</DialogFooter>}
  </DialogContent>;
}
