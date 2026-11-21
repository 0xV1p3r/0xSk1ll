import { useCallback, useEffect, useRef, useState } from 'react';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Input } from '@/components/ui/input';
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select';
import { toast } from './toast';
import { api } from './api';
import { useResource } from './hooks';
import { terms, time } from './model';
import type { LogCause } from './types';
import { Confirm, ErrorNotice, KindBadge, Loading, Nothing, Panel, PanelToolbar, SectionTitle, When } from './shared';


const causeLabel = (cause: LogCause) => cause.type === 'undo' ? `Restored #${cause.entry}` : ({ change: 'Changed', orphan_pruned: 'Orphan cleared', harness_renamed: 'Harness renamed', default_materialized: 'Default applied', machine_removed: 'Machine removed', harness_added: 'Harness added' })[cause.type];
const toggleLabel = (value: boolean | null) => value === null ? 'Default' : value ? 'On' : 'Off';
export function Log({ revision, fleetRevision = revision, changed }: { fleetRevision?: number; revision: number; changed: () => void }) {
  const log = useRef<HTMLDivElement>(null);
  const restoreFocus = useRef(false);
  const [filters, setFilters] = useState({ machine: '', harness: '', author: '', item: '' });
  const { machine, harness, author, item } = filters;
  const filtering = Object.values(filters).some(Boolean);
  const [before, setBefore] = useState<number>();
  const [pages, setPages] = useState<(number | undefined)[]>([]);
  const [search, setSearch] = useState(item);
  // Committing the search resets paging, so a cursor taken from older results is never reused.
  useEffect(() => { if (item === search) return; const timer = setTimeout(() => { setSearch(item); setBefore(undefined); setPages([]); }, 250); return () => clearTimeout(timer); }, [item, search]);
  const settling = item !== search;
  const updateFilters = (next: typeof filters) => { setFilters(next); setBefore(undefined); setPages([]); };
  const load = useCallback(() => api.log({ before, limit: 50, machine, harness, author, item: search }), [before, machine, harness, author, search]);
  const facets = useResource(api.logFacets, revision);
  const { data, error, loading } = useResource(load, revision);
  const fleet = useResource(api.fleet, fleetRevision);
  const removed = (machine: string) => !!fleet.data && !fleet.data.machines.some(row => row.id === machine);
  useEffect(() => {
    if (restoreFocus.current && document.activeElement === document.body) {
      (log.current?.querySelector<HTMLElement>('[role="region"]') ?? log.current)?.focus();
      restoreFocus.current = false;
    }
  }, [data]);
  const rows = data;
  return <>
    <ErrorNotice error={error} />
    <SectionTitle>Assignment log</SectionTitle>
    <Panel><PanelToolbar><Input className="min-w-36 flex-1 basis-36" aria-label="Search items" placeholder={terms.search} value={item} onChange={e => updateFilters({ ...filters, item: e.target.value })} />{(['machine', 'harness', 'author'] as const).map(key => <NativeSelect key={key} aria-label={`Filter by ${key}`} value={filters[key]} onChange={e => updateFilters({ ...filters, [key]: e.target.value })}>
      <NativeSelectOption value="">All {key === 'machine' ? 'Machines' : key === 'harness' ? 'Harnesses' : 'Authors'}</NativeSelectOption>{[...new Set([...(!facets.error && facets.data ? facets.data[({ machine: 'machines', harness: 'harnesses', author: 'authors' } as const)[key]] : data?.map(entry => entry[key]) ?? []), ...(filters[key] ? [filters[key]] : [])])].sort().map(value => <NativeSelectOption key={value} value={value}>{value}</NativeSelectOption>)}</NativeSelect>)}
      </PanelToolbar>
    {loading && !data && <Loading />}
    <div ref={log} tabIndex={-1} role={!rows?.length ? 'region' : undefined} aria-label={!rows?.length ? 'Assignment log' : undefined} className="outline-none">{data && !rows?.length && filtering ? <Nothing title="No entries match these filters"><Button variant="outline" size="sm" onClick={() => updateFilters({ machine: '', harness: '', author: '', item: '' })}>Clear filters</Button></Nothing> : data?.length === 0 ? <Nothing title="No log entries">No Assignment log entries on this page.</Nothing> : data && <Table aria-label="Assignment log">
      <TableHeader><TableRow className="bg-muted/50 hover:bg-muted/50">{['Time', 'Assignment', 'Change', 'Cause', 'Author'].map(label =>
        <TableHead key={label}>{label}</TableHead>)}<TableHead><span className="sr-only">Actions</span></TableHead></TableRow></TableHeader>
      <TableBody>{rows?.map(entry => <TableRow className="group" key={entry.id}>
        <TableCell className="text-muted-foreground"><When seconds={entry.at} focusable={false} /><span className="mt-0.5 block text-xs">{time(entry.at)}</span><span className="block text-xs">#{entry.id}</span></TableCell>
        <TableCell><span className="text-xs text-muted-foreground">{entry.machine} / {entry.harness}</span>
          <p className="mt-0.5 flex items-center gap-2 text-sm font-medium">{entry.item_id === null ? <span className="first-letter:uppercase">{entry.kind.replaceAll('_', ' ')}</span> : <><span title={entry.item_id}>{entry.name ?? entry.item_id}</span><KindBadge kind={entry.kind} /></>}</p></TableCell>
        <TableCell className="whitespace-nowrap">{toggleLabel(entry.before)} <span className="text-muted-foreground">→</span> {toggleLabel(entry.after)}</TableCell>
        <TableCell>{entry.cause.type === 'change' ? <span className="text-muted-foreground">—</span> : <Badge variant="secondary">{causeLabel(entry.cause)}</Badge>}</TableCell>
        <TableCell className="text-muted-foreground">{entry.author}</TableCell>
        <TableCell data-log-id={entry.id} className="w-24 min-w-24 max-w-24 whitespace-normal xl:w-36 xl:min-w-36 xl:max-w-36">{removed(entry.machine) ? <span className="text-xs text-muted-foreground" title="This Machine was removed. Join it again to restore its Assignment.">Machine removed</span> : <Confirm label={`Restore ${toggleLabel(entry.before)}`} title="Restore Assignment?" description={`Restore ${entry.item_id === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name ?? entry.item_id}`} on Machine ${entry.machine} / Harness ${entry.harness} to ${toggleLabel(entry.before)}?`} confirmLabel="Restore" variant="default" triggerVariant="outline"
          ariaLabel={`Restore ${toggleLabel(entry.before)} for log entry ${entry.id}: ${entry.item_id === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name ?? entry.item_id}`} on ${entry.machine} / ${entry.harness}`} action={() => { restoreFocus.current = true; return api.undo(entry.id); }} onDone={() => {
            toast({ title: `Restored Assignment ${entry.item_id === null ? entry.kind.replaceAll('_', ' ') : `${entry.kind}:${entry.name ?? entry.item_id}`} on ${entry.machine} / ${entry.harness} to ${toggleLabel(entry.before)}.` }); changed();
          }} />}</TableCell>
      </TableRow>)}</TableBody></Table>}</div></Panel>
    {(before !== undefined || pages.length > 0 || (data?.length ?? 0) >= 50) && <div className="flex items-center justify-end gap-2">
      {before !== undefined && <Button variant="ghost" size="default" disabled={settling} onClick={() => { setBefore(undefined); setPages([]); }}>Latest</Button>}
      <span className="text-sm text-muted-foreground">Page {pages.length + 1}</span>
      <Button variant="outline" size="default" disabled={!pages.length || loading || settling} onClick={() => {
        setBefore(pages.at(-1)); setPages(p => p.slice(0, -1));
      }}><ChevronLeft data-icon="inline-start" />Newer</Button>
      <Button variant="outline" size="default" disabled={!data?.length || data.length < 50 || loading || settling} onClick={() => {
        setPages(p => [...p, before]); setBefore(data?.at(-1)?.id);
      }}>Older<ChevronRight data-icon="inline-end" /></Button>
    </div>}
  </>;
}
