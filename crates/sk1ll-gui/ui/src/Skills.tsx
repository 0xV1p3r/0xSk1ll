import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { ArrowDown, ArrowUp, ArrowUpRight, ChevronDown, File, Folder } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Field, FieldLabel } from '@/components/ui/field';
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuLabel, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { toast } from './toast';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { ChangeBadge, ErrorNotice, Loading, Nothing, Notice, Pending } from './shared';
import { SkillFile } from './SkillFile';
import type { ContentEntry, ContentQuery, ContentType, StatusView } from './types';

const typeLabels: Record<ContentType, string> = { personal: 'Personal', import: 'Import', rule: 'Rule', unmanaged: 'Unmanaged' };
const loadEditors = () => api.editors(true);
const keyFor = (entry: ContentEntry) => entry.item ?? `unmanaged:${entry.name}`;
function DraftBadge({ entry }: { entry: ContentEntry }) {
  return entry.draft ? <ChangeBadge change={entry.draft} /> : <span className="text-muted-foreground">—</span>;
}

export function Skills({ entries, revision, changed, reviewDraft, assignment, addImport, connection, initialSearch }: {
  initialSearch?: string; connection: StatusView['connection']; entries: ContentEntry[]; revision: number; changed: () => void; reviewDraft: () => void; assignment: (item: string, harness: string) => void; addImport: () => void;
}) {
  const [filter, setFilter] = useState('all');
  const [search, setSearch] = useState(initialSearch ?? '');
  useEffect(() => { if (initialSearch !== undefined) setSearch(initialSearch); }, [initialSearch]);
  const [creating, setCreating] = useState<'skill' | 'rule'>();
  const [name, setName] = useState(''), [description, setDescription] = useState('');
  const creation = useMutation(changed);
  const nameError = (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name) || name.includes('..')) ? 'Use letters, numbers, dots, underscores or hyphens; start with a letter or number.' : entries.some(entry => (creating === 'rule' ? entry.type === 'rule' : entry.type !== 'rule') && entry.name.toLowerCase() === name.toLowerCase()) ? 'This name is already taken.' : '';
  const startCreate = (kind: 'skill' | 'rule') => { setName(''); setDescription(''); setCreating(kind); };
  const [sort, setSort] = useState<{ column: 'name' | 'type'; descending: boolean }>({ column: 'name', descending: false });
  const [selected, setSelected] = useState<{ key: string; harness?: string; optimistic?: { entry: ContentEntry; inventory: ContentEntry[] } }>();
  const current = entries.find(value => keyFor(value) === selected?.key);
  const entry = selected?.optimistic?.inventory === entries ? selected.optimistic.entry : current;
  const visible = entries.filter(value => (filter === 'all' || value.type === filter) && value.name.toLowerCase().includes(search.toLowerCase()))
    .sort((a, b) => ((sort.column === 'type' ? typeLabels[a.type].localeCompare(typeLabels[b.type]) : 0) || a.name.localeCompare(b.name)) * (sort.descending ? -1 : 1));
  const sortBy = (column: 'name' | 'type') => setSort(previous => ({ column, descending: previous.column === column && !previous.descending }));
  return <div className="flex min-w-0 flex-col gap-4">
    <div className="flex flex-wrap items-end gap-3">
      <Field className="w-44"><FieldLabel htmlFor="skill-type">Type</FieldLabel><NativeSelect id="skill-type" value={filter} onChange={event => setFilter(event.target.value)}>
        <NativeSelectOption value="all">All types</NativeSelectOption>
        {Object.entries(typeLabels).map(([type, label]) => <NativeSelectOption key={type} value={type}>{label}</NativeSelectOption>)}
      </NativeSelect></Field>
      <Field className="min-w-40 flex-1 sm:max-w-xs"><FieldLabel htmlFor="skill-search">Search by name</FieldLabel>
        <Input id="skill-search" type="search" value={search} onChange={event => setSearch(event.target.value)} placeholder="Find a Skill or Rule…" /></Field>
      <DropdownMenu><DropdownMenuTrigger render={<Button />}>New</DropdownMenuTrigger><DropdownMenuContent><DropdownMenuGroup><DropdownMenuItem onClick={() => startCreate('skill')}>New Skill</DropdownMenuItem><DropdownMenuItem onClick={() => startCreate('rule')}>New Rule</DropdownMenuItem></DropdownMenuGroup></DropdownMenuContent></DropdownMenu>
    </div>
    <Dialog open={!!creating} onOpenChange={open => { if (!open && !creation.pending) setCreating(undefined); }}><DialogContent><DialogHeader><DialogTitle>New {creating === 'rule' ? 'Rule' : 'Skill'}</DialogTitle><DialogDescription>Create an item in Draft.</DialogDescription></DialogHeader>
      <Field><FieldLabel htmlFor="new-name">Name</FieldLabel><Input id="new-name" value={name} onChange={event => setName(event.target.value)} aria-invalid={!!name && !!nameError} aria-describedby="name-error" /><p id="name-error" className="text-xs text-muted-foreground">{name && nameError}</p></Field>
      <Field><FieldLabel htmlFor="new-description">Description (optional)</FieldLabel><Input id="new-description" value={description} onChange={event => setDescription(event.target.value)} /></Field><ErrorNotice error={creation.error} />
      <DialogFooter><Button variant="outline" disabled={creation.pending} onClick={() => setCreating(undefined)}>Cancel</Button><Button disabled={!!nameError || creation.pending} onClick={() => { if (creating) void creation.run(() => api.createItem({ kind: creating, name, description: description.trim() || undefined }), () => { setSelected({ key: `${creating}:${name}` }); setSearch(''); setFilter('all'); setCreating(undefined); toast({ title: `${creating === 'rule' ? 'Rule' : 'Skill'} created` }); }); }}>Create</Button></DialogFooter></DialogContent>
    </Dialog>
    {visible.length ? <div className="overflow-hidden rounded-lg border"><Table aria-label="Skills" stickyFirstColumn>
      <TableHeader><TableRow className="bg-muted hover:bg-muted">{(['name', 'type'] as const).map(column => <TableHead key={column}
        aria-sort={sort.column === column ? sort.descending ? 'descending' : 'ascending' : 'none'}>
        <Button variant="ghost" size="sm" className="-ml-2.5" onClick={() => sortBy(column)}>{column === 'name' ? 'Name' : 'Type'}
          {sort.column === column && (sort.descending ? <ArrowDown data-icon="inline-end" /> : <ArrowUp data-icon="inline-end" />)}</Button>
      </TableHead>)}<TableHead>Draft state</TableHead><TableHead>Harnesses</TableHead></TableRow></TableHeader>
      <TableBody>{visible.map(value => {
        const harnesses = value.type === 'unmanaged' ? [...new Set(value.variants.flatMap(variant => variant.locations.map(location => location.harness)))] : value.enabled_harnesses;
        return <TableRow key={keyFor(value)} tabIndex={0} aria-label={`View ${typeLabels[value.type]} ${value.name}`} className="cursor-pointer focus-visible:outline-2 focus-visible:outline-ring focus-visible:-outline-offset-2"
          onClick={() => setSelected({ key: keyFor(value) })} onKeyDown={event => { if (event.target === event.currentTarget && ['Enter', ' '].includes(event.key)) { event.preventDefault(); setSelected({ key: keyFor(value) }); } }}>
          <TableCell className="min-w-48 whitespace-normal"><span className="font-medium">{value.name}</span>
            {value.description && <p className="mt-1 max-w-md text-xs text-muted-foreground">{value.description}</p>}</TableCell>
          <TableCell>{typeLabels[value.type]}</TableCell><TableCell><DraftBadge entry={value} /></TableCell>
          <TableCell><div className="flex max-w-sm flex-wrap gap-1">
            {harnesses.map(name => <Badge key={name} variant="outline" className="hover:underline" title={value.type === 'unmanaged' ? 'View this location' : 'Open in Assignment matrix'} render={<button type="button" aria-label={`${value.type === 'unmanaged' ? 'View' : 'Assignment for'} ${value.name} in ${name}`}
              onClick={event => { event.stopPropagation(); if (value.type === 'unmanaged') setSelected({ key: keyFor(value), harness: name }); else if (value.item) assignment(value.item, name); }} />}>{name}<ArrowUpRight className="size-3" /></Badge>)}
            {value.variants.length > 1 && <Badge variant="warning">Differs</Badge>}
            {value.clashes_with_repo && <Badge variant="warning">Clashes with repo</Badge>}
          </div></TableCell>
        </TableRow>;
      })}</TableBody>
    </Table></div> : <Nothing title={entries.length ? 'No matching Skills' : 'No Skills yet'} action={!entries.length && <div className="flex gap-2"><Button onClick={() => startCreate('skill')}>New Skill</Button><Button variant="outline" onClick={addImport}>Add Import</Button></div>}>{entries.length ? 'Choose another Type or name.' : 'Personal Skills, Imports, Rules, and Unmanaged skills will appear here.'}</Nothing>}
    <Sheet open={!!entry} onOpenChange={open => { if (!open) setSelected(undefined); }}>
      <SheetContent className="overflow-y-auto data-[side=right]:w-full data-[side=right]:sm:max-w-3xl">
        {entry && <SkillDetail key={`${entry.type === 'rule' ? 'rule' : 'skill'}:${entry.name}`} entry={entry} initialHarness={selected?.harness} connection={connection} revision={revision} changed={changed} reviewDraft={() => { setSelected(undefined); reviewDraft(); }}
          assignment={assignment} converted={next => setSelected({ key: keyFor(next), optimistic: { entry: next, inventory: entries } })} />}
      </SheetContent>
    </Sheet>
  </div>;
}

function SkillFilePreview({ query, path, revision }: { query: () => ContentQuery; path: string; revision: number }) {
  const file = useResource(useCallback(() => api.skillFile({ ...query(), path }), [query, path]), revision);
  return <><ErrorNotice error={file.error} />{file.loading ? <Loading /> : file.data && <SkillFile file={file.data} />}</>;
}

function SkillBrowser({ query, revision }: { query: () => ContentQuery; revision: number }) {
  const tree = useResource(useCallback(() => api.skillTree(query()), [query]), revision);
  const [selectedPath, setSelectedPath] = useState('');
  const path = tree.data?.some(file => file.path === selectedPath && file.kind !== 'directory') ? selectedPath
    : tree.data?.find(file => file.path === 'SKILL.md')?.path ?? tree.data?.find(file => file.kind !== 'directory')?.path;
  return <>
      <ErrorNotice error={tree.error} />
      <div className="grid min-w-0 gap-4 md:grid-cols-[12rem_minmax(0,1fr)]">
        <nav aria-label="Files" className="flex max-h-64 min-w-0 flex-col gap-1 overflow-auto md:max-h-none">
          {tree.loading ? <Loading rows={2} /> : tree.data?.length ? tree.data.map(value => value.kind === 'directory'
            ? <span key={value.path} title={value.path} className="flex items-center gap-2 px-2 py-1 text-xs text-muted-foreground" style={{ paddingLeft: 8 + (value.path.split('/').length - 1) * 12 }}><Folder className="size-3 shrink-0" />{value.path.split('/').pop()}</span>
            : <Button key={value.path} variant={path === value.path ? 'secondary' : 'ghost'} size="sm" className="justify-start" aria-current={path === value.path ? 'page' : undefined}
              style={{ paddingLeft: 8 + (value.path.split('/').length - 1) * 12 }} title={value.path} onClick={() => setSelectedPath(value.path)}><File data-icon="inline-start" /><span className="truncate">{value.path.split('/').pop()}</span></Button>)
            : <p className="text-sm text-muted-foreground">No files.</p>}
        </nav>
        <div className="min-w-0">{path && <SkillFilePreview key={path} query={query} path={path} revision={revision} />}</div>
      </div>
  </>;
}

function ServerRequired({ needed, children }: { needed: boolean; children: ReactNode }) {
  return needed ? <Tooltip><TooltipTrigger render={<span tabIndex={0} className="inline-flex" />}>{children}</TooltipTrigger>
    <TooltipContent>Needs a connection to the Server</TooltipContent></Tooltip> : children;
}

function SkillDetail({ entry, initialHarness, revision, changed, reviewDraft, converted, connection, assignment }: {
  assignment: (item: string, harness: string) => void; connection: StatusView['connection']; entry: ContentEntry; initialHarness?: string; revision: number; changed: () => void; reviewDraft: () => void; converted: (entry: ContentEntry) => void;
}) {
  const [location, setLocation] = useState(() => {
    const variant = entry.variants.find(value => value.locations.some(loc => loc.harness === initialHarness));
    return variant ? `${variant.hash}:${initialHarness}` : '';
  });
  const choices = entry.variants.flatMap(variant => variant.locations.map(loc => ({ ...loc, hash: variant.hash, description: variant.description })));
  const choice = choices.find(value => `${value.hash}:${value.harness}` === location) ?? choices[0];
  const hash = choice?.hash, harness = choice?.harness;
  const item = keyFor(entry);
  const query = useCallback(() => ({ item, ...(hash ? { hash, harness } : {}) }), [item, hash, harness]);
  const editors = useResource(loadEditors, revision);
  const mutation = useMutation();
  const [confirmation, setConfirmation] = useState<{ editorId?: string }>();
  const confirmationTrigger = useRef<HTMLButtonElement>(null);
  const reviewButton = useRef<HTMLButtonElement>(null);
  const editorTrigger = useRef<HTMLButtonElement>(null);
  const preferred = editors.data?.editors.find(editor => editor.id === editors.data?.preferred_editor);
  const needsConversion = entry.type === 'import' || entry.type === 'unmanaged';
  const needsServer = entry.type === 'unmanaged' && connection !== 'connected';
  const canAdopt = !needsServer && !entry.clashes_with_repo && !!choice;
  const execute = (editorId?: string) => void mutation.run(async () => {
    let destination = entry.item;
    if (entry.type === 'import') { await api.fork(entry.name); destination = `skill:${entry.name}`; }
    if (entry.type === 'unmanaged') {
      if (needsServer) throw new Error('Adopt needs a connection to the Server');
      if (!choice || entry.clashes_with_repo) throw new Error('Choose a Harness variant with an available name before Adopt.');
      await api.adoptUnmanaged({ name: entry.name, harness: choice.harness }); destination = `skill:${entry.name}`;
    }
    if (needsConversion) {
      // Keep the new Personal Skill selectable even before the next SSE event.
      converted({ ...entry, item: destination, type: 'personal', variants: [], clashes_with_repo: false, draft: entry.type === 'unmanaged' ? 'added' : entry.draft ?? 'modified' });
      changed();
    }
    setConfirmation(undefined);
    if (editorId && destination) {
      await api.setPreferredEditor(editorId);
      await api.openInEditor(editorId, destination);
      await editors.refresh();
    }
  });
  const disabledReason = needsServer ? 'Needs a connection to the Server' : entry.clashes_with_repo ? 'Name clashes with a repo Skill' : editors.loading ? 'Loading editors…' : !preferred ? 'No editors found' : '';
  const edit = (editorId: string) => { if (needsConversion) setConfirmation({ editorId }); else execute(editorId); };
  return <>
    <SheetHeader className="pr-12"><SheetTitle>{entry.name}</SheetTitle>
      <SheetDescription>{choice?.description ?? entry.description ?? `${typeLabels[entry.type]} ${entry.type === 'rule' ? '' : 'Skill'}`}</SheetDescription>
      <div className="mt-2 flex flex-wrap gap-2"><Badge variant="secondary">{typeLabels[entry.type]}</Badge><DraftBadge entry={entry} />
        {entry.variants.length > 1 && <Badge variant="warning">Differs</Badge>}{entry.clashes_with_repo && <Badge variant="warning">Clashes with repo</Badge>}</div>
    </SheetHeader>
    <div className="flex min-w-0 flex-col gap-4 px-4 pb-6">
      <div className="flex flex-wrap gap-2">
        {entry.draft && <Button ref={reviewButton} variant="outline" size="sm" onClick={reviewDraft}>Review in Draft</Button>}
        {entry.draft !== 'deleted' && <>
          <ServerRequired needed={needsServer}><div className="flex" role="group" aria-label="Edit Skill">
            <Button size="sm" className="rounded-r-none" disabled={(entry.type !== 'unmanaged' && !preferred) || mutation.pending || (entry.type === 'unmanaged' && !canAdopt)}
              onClick={event => { confirmationTrigger.current = event.currentTarget; if (entry.type === 'unmanaged') setConfirmation({}); else if (preferred) edit(preferred.id); }}><Pending pending={mutation.pending}>
                {entry.type === 'import' ? 'Fork to edit' : entry.type === 'unmanaged' ? 'Adopt' : `Open in ${preferred?.name ?? 'editor'}`}
              </Pending></Button>
            <DropdownMenu><DropdownMenuTrigger render={<Button size="icon-sm" className="rounded-l-none border-l" ref={editorTrigger} aria-label="Choose editor" disabled={!editors.data?.editors.length || mutation.pending || (entry.type === 'unmanaged' && !canAdopt)} />}><ChevronDown /></DropdownMenuTrigger>
              <DropdownMenuContent align="end">{(['gui', 'terminal', 'system'] as const).map(kind => {
                const choices = editors.data?.editors.filter(editor => editor.kind === kind) ?? [];
                return choices.length > 0 && <DropdownMenuGroup key={kind}><DropdownMenuLabel>{kind === 'gui' ? 'GUI' : kind === 'terminal' ? 'Terminal' : 'System'}</DropdownMenuLabel>
                  {choices.map(editor => <DropdownMenuItem key={editor.id} onClick={() => { confirmationTrigger.current = editorTrigger.current; edit(editor.id); }}>{entry.type === 'unmanaged' ? `Adopt and open in ${editor.name}` : editor.name}</DropdownMenuItem>)}
                </DropdownMenuGroup>;
              })}<DropdownMenuGroup><DropdownMenuItem onClick={() => void editors.refresh()}>Refresh editors</DropdownMenuItem></DropdownMenuGroup></DropdownMenuContent>
            </DropdownMenu>
          </div></ServerRequired>

        </>}
      </div>
      {disabledReason && entry.draft !== 'deleted' && <p className="text-xs text-muted-foreground">{disabledReason}</p>}
      {entry.type !== 'unmanaged' && <div className="flex flex-wrap items-center gap-2 text-xs"><span className="text-muted-foreground">Harnesses: {entry.enabled_harnesses.join(', ') || 'None'}</span><Button variant="link" size="sm" onClick={() => assignment(item, entry.enabled_harnesses[0] ?? '')}>Edit in Assignment matrix</Button></div>}
      <ErrorNotice error={editors.error || mutation.error} />
      {entry.draft === 'deleted' && <Notice title="Deleted in Draft">Browse the read-only Baseline files below.</Notice>}
      {entry.clashes_with_repo && <Notice title="Clashes with repo">A repo Skill already has this name. Adopt is unavailable.</Notice>}
      {entry.type === 'unmanaged' && choices.length > 1 && <Field><FieldLabel htmlFor="skill-variant">Harness location{entry.variants.length > 1 ? ' and variant' : ''}</FieldLabel>
        <NativeSelect className="w-full max-w-full" id="skill-variant" value={choice ? `${choice.hash}:${choice.harness}` : ''} onChange={event => setLocation(event.target.value)}>
          {choices.map(value => <NativeSelectOption key={`${value.hash}:${value.harness}`} value={`${value.hash}:${value.harness}`}>{value.harness}: {value.path}</NativeSelectOption>)}
        </NativeSelect></Field>}
      <SkillBrowser key={`${item}:${hash}:${harness}`} query={query} revision={revision} />
    </div>
    <Dialog open={!!confirmation} onOpenChange={open => { if (!open && !mutation.pending) setConfirmation(undefined); }}>
      <DialogContent finalFocus={() => needsConversion ? confirmationTrigger.current : reviewButton.current} showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>{entry.type === 'import' ? 'Fork' : 'Adopt'} {entry.name}?</DialogTitle>
        <DialogDescription>{entry.type === 'import'
          ? 'Fork turns this Import into a Personal Skill in the Draft. It will no longer receive updates from its source.'
          : `Adopt copies this Unmanaged skill into the Draft as a Personal Skill, enabled in the Harnesses where this variant was found. The original is kept in recovery when sync replaces it.${entry.variants.length > 1 ? ` Adopt uses the selected variant from ${choice?.harness}. Other variants stay unmanaged.` : ''}`}</DialogDescription>
      </DialogHeader><ErrorNotice error={mutation.error} /><DialogFooter>
        <Button variant="outline" disabled={mutation.pending} onClick={() => setConfirmation(undefined)}>Cancel</Button>
        <ServerRequired needed={needsServer}><Button disabled={mutation.pending || needsServer} onClick={() => execute(confirmation?.editorId)}><Pending pending={mutation.pending}>
          {entry.type === 'import' ? 'Fork' : 'Adopt'}{confirmation?.editorId ? ' and open' : ' into Draft'}</Pending></Button></ServerRequired>
      </DialogFooter></DialogContent>
    </Dialog>
  </>;
}
