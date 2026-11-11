import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { ArrowDown, ArrowUp, ChevronDown, Eye, File, Folder, LayoutGrid } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Field, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { toast } from './toast';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { ChangeBadge, Confirm, ErrorNotice, Loading, Nothing, Notice, Panel, PanelToolbar, Pending, SectionTitle } from './shared';
import { terms } from './model';
import { SkillFile } from './SkillFile';
import type { ContentEntry, ContentQuery, ContentType, StatusView } from './types';

const typeLabels: Record<ContentType, string> = { personal: 'Personal', import: 'Import', rule: 'Rule', unmanaged: 'Unmanaged' };
const loadEditors = () => api.editors(true);
const keyFor = (entry: ContentEntry) => entry.item ?? `unmanaged:${entry.name}`;
function DraftBadge({ entry }: { entry: ContentEntry }) {
  return entry.draft ? <ChangeBadge change={entry.draft} /> : <span className="text-muted-foreground">—</span>;
}

// A request from another screen; every new object is applied, even when the Library is already open.
export type SkillsHandoff = { search?: string; filter?: ContentType; create?: 'skill' | 'rule' };
const noHandoff: SkillsHandoff = {};
export function Skills({ entries, revision, changed, reviewDraft, assignment, addImport, connection, handoff = noHandoff }: {
  handoff?: SkillsHandoff; connection: StatusView['connection']; entries: ContentEntry[]; revision: number; changed: () => void; reviewDraft: () => void; assignment: (item: string, harness: string) => void; addImport: () => void;
}) {
  const [filter, setFilter] = useState<string>(handoff.filter ?? 'all');
  const [search, setSearch] = useState(handoff.search ?? '');
  const [creating, setCreating] = useState<'skill' | 'rule'>();
  const [name, setName] = useState(''), [description, setDescription] = useState('');
  const creation = useMutation(changed);
  const [nameTouched, setNameTouched] = useState(false);
  const nameError = !name ? 'Enter a name' : (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name) || name.includes('..')) ? 'Use letters, numbers, dots, underscores or hyphens; start with a letter or number.' : entries.some(entry => (creating === 'rule' ? entry.type === 'rule' : entry.type !== 'rule') && entry.name.toLowerCase() === name.toLowerCase()) ? 'This name is already taken.' : '';
  const startCreate = (kind: 'skill' | 'rule') => { setName(''); setNameTouched(false); setDescription(''); setCreating(kind); };
  useEffect(() => {
    setSearch(handoff.search ?? ''); setFilter(handoff.filter ?? 'all');
    if (handoff.create) { setName(''); setNameTouched(false); setDescription(''); setCreating(handoff.create); }
  }, [handoff]);
  const [sort, setSort] = useState<{ column: 'name' | 'type'; descending: boolean }>({ column: 'name', descending: false });
  const [selected, setSelected] = useState<{ key: string; harness?: string; optimistic?: { entry: ContentEntry; inventory: ContentEntry[] } }>();
  const sheet = useRef<HTMLDivElement>(null);
  const current = entries.find(value => keyFor(value) === selected?.key);
  const entry = selected?.optimistic?.inventory === entries ? selected.optimistic.entry : current;
  const unmanagedCount = entries.filter(value => value.type === 'unmanaged').length;
  const visible = entries.filter(value => (filter === 'all' || value.type === filter) && value.name.toLowerCase().includes(search.toLowerCase()))
    .sort((a, b) => ((sort.column === 'type' ? typeLabels[a.type].localeCompare(typeLabels[b.type]) : 0) || a.name.localeCompare(b.name)) * (sort.descending ? -1 : 1));
  const sortBy = (column: 'name' | 'type') => setSort(previous => ({ column, descending: previous.column === column && !previous.descending }));
  return <div className="flex min-w-0 flex-col gap-4">
    <SectionTitle>Library</SectionTitle>
    {(handoff.filter === 'unmanaged' || unmanagedCount > 0) && <Notice title="Unmanaged skills">{unmanagedCount} unmanaged {unmanagedCount === 1 ? 'skill found' : 'skills found'} in your Harnesses. Adopt {unmanagedCount === 1 ? 'it' : 'them'} to manage {unmanagedCount === 1 ? 'it' : 'them'} with 0xSk1ll. <Button variant="outline" size="sm" onClick={() => { setFilter(filter === 'unmanaged' ? 'all' : 'unmanaged'); setSearch(''); }}>{filter === 'unmanaged' ? 'Show all' : 'Show Unmanaged'}</Button></Notice>}
    <Panel label="Library contents"><PanelToolbar>
      <Field className="min-w-40 flex-1 sm:max-w-xs"><FieldLabel className="sr-only" htmlFor="skill-search">Search by name</FieldLabel>
        <Input id="skill-search" type="search" value={search} onChange={event => setSearch(event.target.value)} placeholder={terms.search} /></Field>
      <Field className="w-36"><FieldLabel className="sr-only" htmlFor="skill-type">Type</FieldLabel><NativeSelect id="skill-type" value={filter} onChange={event => setFilter(event.target.value)}>
        <NativeSelectOption value="all">All Types</NativeSelectOption>
        {Object.entries(typeLabels).map(([type, label]) => <NativeSelectOption key={type} value={type}>{label}</NativeSelectOption>)}
      </NativeSelect></Field>
      <DropdownMenu><DropdownMenuTrigger render={<Button className="ml-auto" />}>New</DropdownMenuTrigger><DropdownMenuContent><DropdownMenuGroup><DropdownMenuItem onClick={() => startCreate('skill')}>New Skill</DropdownMenuItem><DropdownMenuItem onClick={() => startCreate('rule')}>New Rule</DropdownMenuItem></DropdownMenuGroup></DropdownMenuContent></DropdownMenu>
    </PanelToolbar>
    {visible.length ? <Table aria-label="Skills" stickyFirstColumn>
      <TableHeader><TableRow className="bg-muted hover:bg-muted">{(['name', 'type'] as const).map(column => <TableHead key={column}
        aria-sort={sort.column === column ? sort.descending ? 'descending' : 'ascending' : 'none'}>
        <Button variant="ghost" size="default" className="-ml-2.5" onClick={() => sortBy(column)}>{column === 'name' ? 'Name' : 'Type'}
          {sort.column === column && (sort.descending ? <ArrowDown data-icon="inline-end" /> : <ArrowUp data-icon="inline-end" />)}</Button>
      </TableHead>)}<TableHead>Draft state</TableHead><TableHead>Harnesses</TableHead></TableRow></TableHeader>
      <TableBody>{visible.map(value => {
        const harnesses = value.type === 'unmanaged' ? [...new Set(value.variants.flatMap(variant => variant.locations.map(location => location.harness)))] : value.enabled_harnesses;
        return <TableRow key={keyFor(value)} tabIndex={0} aria-label={`View ${typeLabels[value.type]} ${value.name}`} className="cursor-pointer focus-visible:outline-2 focus-visible:outline-ring focus-visible:-outline-offset-2"
          onClick={() => setSelected({ key: keyFor(value) })} onKeyDown={event => { if (event.target === event.currentTarget && ['Enter', ' '].includes(event.key)) { event.preventDefault(); setSelected({ key: keyFor(value) }); } }}>
          <TableCell className="h-16 min-w-48 whitespace-normal"><span className="font-medium">{value.name}</span>
            {value.description && <p className="mt-1 max-w-md truncate text-xs text-muted-foreground">{value.description}</p>}</TableCell>
          <TableCell>{typeLabels[value.type]}</TableCell><TableCell><DraftBadge entry={value} /></TableCell>
          <TableCell><div className="flex max-w-sm flex-wrap gap-1">
            {harnesses.map(name => <Badge key={name} variant="outline" className="cursor-pointer underline decoration-dotted underline-offset-2 hover:bg-muted focus-visible:outline-2 focus-visible:outline-ring" title={value.type === 'unmanaged' ? `View in ${name}` : 'Open in Assignment matrix'} render={<button type="button" aria-label={`${value.type === 'unmanaged' ? 'View' : 'Assignment for'} ${value.name} in ${name}`}
              onClick={event => { event.stopPropagation(); if (value.type === 'unmanaged') setSelected({ key: keyFor(value), harness: name }); else if (value.item) assignment(value.item, name); }} />}>{value.type === 'unmanaged' ? `View in ${name}` : name}{value.type === 'unmanaged' ? <Eye className="size-3" /> : <LayoutGrid className="size-3" />}</Badge>)}
            {value.variants.length > 1 && <Badge variant="warning">Differs</Badge>}
            {value.clashes_with_repo && <Badge variant="warning">Clashes with repo</Badge>}
          </div></TableCell>
        </TableRow>;
      })}</TableBody>
    </Table> : <Nothing title={entries.length ? 'No matching Skills or Rules' : 'Nothing in the Library yet'} action={!entries.length && <div className="flex gap-2"><Button onClick={() => startCreate('skill')}>New Skill</Button><Button variant="outline" onClick={addImport}>Add Import</Button></div>}>{entries.length ? 'Choose another Type or name.' : 'Personal Skills, Imports, Rules, and Unmanaged skills will appear here.'}</Nothing>}</Panel>
    <Dialog open={!!creating} onOpenChange={open => { if (!open && !creation.pending) setCreating(undefined); }}><DialogContent><DialogHeader><DialogTitle>New {creating === 'rule' ? 'Rule' : 'Skill'}</DialogTitle><DialogDescription>Create an item in Draft.</DialogDescription></DialogHeader>
      <form className="flex flex-col gap-4" onSubmit={event => { event.preventDefault(); setNameTouched(true); if (creating && !nameError && !creation.pending) void creation.run(() => api.createItem({ kind: creating, name, description: description.trim() || undefined }), () => { setSelected({ key: `${creating}:${name}` }); setSearch(''); setFilter('all'); setCreating(undefined); toast({ title: `${creating === 'rule' ? 'Rule' : 'Skill'} created` }); }); }}>
      <FieldGroup><Field data-invalid={nameTouched && !!nameError}><FieldLabel htmlFor="new-name">Name</FieldLabel><Input id="new-name" value={name} onBlur={() => setNameTouched(true)} onChange={event => { setName(event.target.value); setNameTouched(true); }} aria-invalid={nameTouched && !!nameError} aria-describedby="name-error" />{nameTouched && nameError ? <FieldError id="name-error">{nameError}</FieldError> : <p id="name-error" className="text-xs text-muted-foreground">Enter a name for your new {creating === 'rule' ? 'Rule' : 'Skill'}.</p>}</Field>
      <Field><FieldLabel htmlFor="new-description">Description (optional)</FieldLabel><Input id="new-description" value={description} onChange={event => setDescription(event.target.value)} /></Field></FieldGroup>
      <p className="text-sm text-muted-foreground">{creating === 'rule' ? "New Rules follow each Harness's default for new Rules; change it in the Assignment matrix." : "New Skills follow each Harness's default for new Skills; change it in the Assignment matrix."}</p><ErrorNotice error={creation.error} />
      <DialogFooter><Button type="button" variant="outline" disabled={creation.pending} onClick={() => setCreating(undefined)}>Cancel</Button><Button type="submit" disabled={!!nameError || creation.pending}>Create</Button></DialogFooter></form></DialogContent>
    </Dialog>
    <Dialog open={!!entry} onOpenChange={open => { if (!open) setSelected(undefined); }}>
      <DialogContent ref={sheet} initialFocus={sheet} className="flex h-[calc(100dvh-2rem)] flex-col gap-0 overflow-hidden p-0 sm:h-[calc(100dvh-4rem)] sm:max-w-[min(72rem,calc(100%-4rem))]">
        {entry && <SkillDetail key={`${entry.type === 'rule' ? 'rule' : 'skill'}:${entry.name}`} entry={entry} entries={entries} close={() => setSelected(undefined)} initialHarness={selected?.harness} connection={connection} revision={revision} changed={changed} reviewDraft={() => { setSelected(undefined); reviewDraft(); }}
          assignment={assignment} converted={next => setSelected({ key: keyFor(next), optimistic: { entry: next, inventory: entries } })} />}
      </DialogContent>
    </Dialog>
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
  const multiple = (tree.data?.filter(file => file.kind !== 'directory').length ?? 0) > 1;
  if (tree.loading) return <Loading rows={2} />;
  if (!tree.data?.length) return <><ErrorNotice error={tree.error} />{!tree.error && <p className="text-sm text-muted-foreground">No files.</p>}</>;
  return <>
      <ErrorNotice error={tree.error} />
      <div className={multiple ? 'grid min-w-0 gap-4 md:grid-cols-[13rem_minmax(0,1fr)]' : 'min-w-0'}>
        {multiple && <nav aria-label="Files" className="flex max-h-64 min-w-0 flex-col gap-1 overflow-auto md:sticky md:top-0 md:max-h-[calc(100dvh-16rem)] md:self-start">
          {tree.data.map(value => value.kind === 'directory'
            ? <span key={value.path} title={value.path} className="flex h-8 items-center gap-1.5 text-sm text-muted-foreground" style={{ paddingLeft: 8 + (value.path.split('/').length - 1) * 12 }}><Folder className="size-4 shrink-0" /><span className="truncate">{value.path.split('/').pop()}</span></span>
            : <Button key={value.path} variant={path === value.path ? 'secondary' : 'ghost'} size="default" className="justify-start" aria-current={path === value.path ? 'page' : undefined}
              style={{ paddingLeft: 8 + (value.path.split('/').length - 1) * 12 }} title={value.path} onClick={() => setSelectedPath(value.path)}><File data-icon="inline-start" /><span className="truncate">{value.path.split('/').pop()}</span></Button>)}
        </nav>}
        <div className="min-w-0">{path && <SkillFilePreview key={path} query={query} path={path} revision={revision} />}</div>
      </div>
  </>;
}

function ServerRequired({ needed, children }: { needed: boolean; children: ReactNode }) {
  return needed ? <Tooltip><TooltipTrigger render={<span tabIndex={0} className="inline-flex" />}>{children}</TooltipTrigger>
    <TooltipContent>Needs a connection to the Server</TooltipContent></Tooltip> : children;
}

function SkillDetail({ entry, entries, close, initialHarness, revision, changed, reviewDraft, converted, connection, assignment }: {
  entries: ContentEntry[]; close: () => void; assignment: (item: string, harness: string) => void; connection: StatusView['connection']; entry: ContentEntry; initialHarness?: string; revision: number; changed: () => void; reviewDraft: () => void; converted: (entry: ContentEntry) => void;
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
  const [renaming, setRenaming] = useState(false), [renameTo, setRenameTo] = useState(entry.name);
  const rename = useMutation();
  const kind = entry.type === 'rule' ? 'rule' : 'skill';
  const renameError = !renameTo ? 'Enter a name' : (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(renameTo) || renameTo.includes('..')) ? 'Use letters, numbers, dots, underscores or hyphens; start with a letter or number.' : renameTo === entry.name ? 'Choose a different name.' : entries.some(value => (kind === 'rule' ? value.type === 'rule' : value.type !== 'rule') && value.name.toLowerCase() === renameTo.toLowerCase()) ? 'This name is already taken.' : '';
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
  const disabledReason = needsServer ? 'Needs a connection to the Server' : entry.clashes_with_repo ? 'Name clashes with a repo Skill' : !editors.data ? '' : !preferred ? 'No editors found' : '';
  const edit = (editorId: string) => { if (needsConversion) setConfirmation({ editorId }); else execute(editorId); };
  return <>
    <div className="flex shrink-0 flex-wrap items-start justify-between gap-x-6 gap-y-3 border-b border-border px-5 py-4 pr-12">
      <DialogHeader className="min-w-0 flex-1 gap-1.5"><DialogTitle className="text-lg">{entry.name}</DialogTitle>
        <DialogDescription>{choice?.description ?? entry.description ?? `${typeLabels[entry.type]} ${entry.type === 'rule' ? '' : 'Skill'}`}</DialogDescription>
        <div className="mt-1 flex flex-wrap items-center gap-2 text-xs"><Badge variant="secondary">{typeLabels[entry.type]}</Badge>{entry.draft && <ChangeBadge change={entry.draft} />}
          {entry.variants.length > 1 && <Badge variant="warning">Differs</Badge>}{entry.clashes_with_repo && <Badge variant="warning">Clashes with repo</Badge>}
          {entry.type !== 'unmanaged' && <><span className="text-muted-foreground">Harnesses: {entry.enabled_harnesses.join(', ') || 'None'}</span><Button variant="link" size="default" className="h-auto p-0 text-xs" onClick={() => assignment(item, entry.enabled_harnesses[0] ?? 'all')}>Open in Assignment matrix</Button></>}</div>
      </DialogHeader>
      <div className="flex flex-col items-end gap-1.5"><div className="flex flex-wrap justify-end gap-2">
        {(entry.type === 'personal' || entry.type === 'rule') && entry.draft !== 'deleted' && <>
          <Button variant="outline" size="sm" onClick={() => { setRenameTo(entry.name); setRenaming(true); }}>Rename…</Button>
          <Confirm label="Delete…" confirmLabel="Delete" triggerVariant="outline" title={`Delete ${entry.name}?`} description="This removes it from this Machine's Draft. Commit removes it from the Baseline and every Machine." action={() => api.deleteItem({ kind, name: entry.name })} onDone={() => { toast({ title: `${entry.name} deleted in Draft` }); close(); changed(); }} />
        </>}
        {entry.draft && <Button ref={reviewButton} variant="outline" size="default" onClick={reviewDraft}>Review in Draft</Button>}
        {entry.draft !== 'deleted' && <>
          <ServerRequired needed={needsServer}><div className="flex" role="group" aria-label="Edit Skill">
            <Button size="default" className="rounded-r-none" disabled={(entry.type !== 'unmanaged' && !preferred) || mutation.pending || (entry.type === 'unmanaged' && !canAdopt)}
              onClick={event => { confirmationTrigger.current = event.currentTarget; if (entry.type === 'unmanaged') setConfirmation({}); else if (preferred) edit(preferred.id); }}><Pending pending={mutation.pending}>
                {entry.type === 'import' ? 'Fork to edit' : entry.type === 'unmanaged' ? 'Adopt into Draft' : `Open in ${preferred?.name ?? 'editor'}`}
              </Pending></Button>
            <DropdownMenu><DropdownMenuTrigger render={<Button size="icon" className="rounded-l-none border-l" ref={editorTrigger} aria-label="Choose editor" disabled={!editors.data?.editors.length || mutation.pending || (entry.type === 'unmanaged' && !canAdopt)} />}><ChevronDown /></DropdownMenuTrigger>
              <DropdownMenuContent align="end">{(['gui', 'terminal', 'system'] as const).map(kind => {
                const choices = editors.data?.editors.filter(editor => editor.kind === kind) ?? [];
                return choices.length > 0 && <DropdownMenuGroup key={kind}><DropdownMenuLabel>{kind === 'gui' ? 'GUI' : kind === 'terminal' ? 'Terminal' : 'System'}</DropdownMenuLabel>
                  {choices.map(editor => <DropdownMenuItem key={editor.id} onClick={() => { confirmationTrigger.current = editorTrigger.current; edit(editor.id); }}>{entry.type === 'unmanaged' ? `Adopt and open in ${editor.name}` : editor.name}</DropdownMenuItem>)}
                </DropdownMenuGroup>;
              })}<DropdownMenuSeparator /><DropdownMenuGroup><DropdownMenuItem onClick={() => void editors.refresh()}>Refresh editors</DropdownMenuItem></DropdownMenuGroup></DropdownMenuContent>
            </DropdownMenu>
          </div></ServerRequired>
        </>}
      </div>
      {disabledReason && entry.draft !== 'deleted' && <p className="text-xs text-muted-foreground">{disabledReason}</p>}</div>
    </div>
    <div className="flex min-h-0 min-w-0 flex-1 flex-col gap-4 overflow-y-auto px-5 py-4">
      <ErrorNotice error={editors.error || mutation.error} />
      {entry.draft === 'deleted' && <Notice title="Deleted in Draft">Browse the read-only Baseline files below.</Notice>}
      {entry.clashes_with_repo && <Notice title="Clashes with repo">A repo Skill already has this name. Adopt is unavailable.</Notice>}
      {entry.type === 'unmanaged' && choices.length > 1 && <Field><FieldLabel htmlFor="skill-variant">Harness location{entry.variants.length > 1 ? ' and variant' : ''}</FieldLabel>
        <NativeSelect className="w-full max-w-full" id="skill-variant" value={choice ? `${choice.hash}:${choice.harness}` : ''} onChange={event => setLocation(event.target.value)}>
          {choices.map(value => <NativeSelectOption key={`${value.hash}:${value.harness}`} value={`${value.hash}:${value.harness}`}>{value.harness}: {value.path}</NativeSelectOption>)}
        </NativeSelect></Field>}
      <SkillBrowser key={`${item}:${hash}:${harness}`} query={query} revision={revision} />
    </div>
    <Dialog open={renaming} onOpenChange={open => { if (!rename.pending) setRenaming(open); }}><DialogContent showCloseButton={!rename.pending}><DialogHeader><DialogTitle>Rename {entry.name}</DialogTitle><DialogDescription>Rename this item in Draft.</DialogDescription></DialogHeader>
      <form className="flex flex-col gap-4" onSubmit={event => { event.preventDefault(); if (!renameError && !rename.pending) void rename.run(() => api.renameItem({ kind, name: entry.name, to: renameTo }), result => { toast({ title: `Renamed to ${renameTo}` }); if (result.assignment_error) toast({ title: `Renamed, but Assignments weren't copied: ${result.assignment_error}`, tone: 'destructive' }); setRenaming(false); converted({ ...entry, name: renameTo, item: result.item, draft: 'added', in_baseline: false, enabled_harnesses: result.assignment_error ? [] : entry.enabled_harnesses }); changed(); }); }}>
        <FieldGroup><Field data-invalid={!!renameError}><FieldLabel htmlFor="rename-name">Name</FieldLabel><Input id="rename-name" value={renameTo} onChange={event => setRenameTo(event.target.value)} aria-invalid={!!renameError} aria-describedby="rename-error" />{renameError && <FieldError id="rename-error">{renameError}</FieldError>}</Field></FieldGroup>
        <Notice title="Assignment changes">Assignment toggles carry over on this Machine only. Other Machines use each Harness's default for the new name after you Commit and Push.</Notice><ErrorNotice error={rename.error} />
        <DialogFooter><Button type="button" variant="outline" disabled={rename.pending} onClick={() => setRenaming(false)}>Cancel</Button><Button type="submit" disabled={!!renameError || rename.pending}><Pending pending={rename.pending}>Rename</Pending></Button></DialogFooter>
      </form></DialogContent></Dialog>
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
