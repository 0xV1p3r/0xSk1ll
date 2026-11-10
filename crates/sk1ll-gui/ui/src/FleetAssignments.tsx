import { useCallback, useState } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api } from './api';
import { useResource } from './hooks';
import { matrixCheckbox, useAssignmentWrites } from './assignments';
import { cellState, emptyHarness, mergeAssignments, splitItem } from './model';
import { ErrorNotice, Loading, Nothing, Pending } from './shared';
import type { CachedAssignmentSnapshot, ItemView } from './types';

type Data = { items: ItemView[]; assignments: Record<string, CachedAssignmentSnapshot> };
const merge = (previous: Data | undefined, next: Data): Data => ({ ...next, assignments: mergeAssignments(previous?.assignments ?? {}, next.assignments) });
export function FleetAssignments({ revision, machineId, harnesses, navigate }: { revision: number; machineId: string; harnesses: string[]; navigate: (screen: 'Assignment matrix') => void }) {
  const load = useCallback(async (): Promise<Data> => {
    const [items, snapshot] = await Promise.all([api.items(), api.assignment(machineId)]);
    return { items, assignments: { [machineId]: snapshot } };
  }, [machineId]);
  const resource = useResource(load, revision, merge);
  const { pending, errors, write, assignment } = useAssignmentWrites(resource);
  const current = resource.data && assignment(machineId);
  const columns = [...new Set([...harnesses, ...Object.keys(current?.harnesses ?? {})])].sort();
  const score = (item: ItemView) => Math.max(0, ...columns.map(harness => {
    const state = cellState(current!, harness, item.item), defaults = current!.harnesses[harness] ?? emptyHarness();
    return state.explicit ? state.enabled !== defaults[splitItem(item.item).kind === 'skill' ? 'new_skills_default' : 'new_rules_default'] ? 2 : 1 : 0;
  }));
  const all = resource.data?.items.filter(item => ['skill', 'rule'].includes(splitItem(item.item).kind)) ?? [];
  // Rank once per external refresh so toggling a row never moves or removes it under the pointer or focus.
  const key = `${revision}:${all.map(item => item.item).join()}`;
  const [order, setOrder] = useState<{ key: string; ids: string[] }>();
  if (current && order?.key !== key && !pending[machineId]) setOrder({ key, ids: [...all].sort((a, b) => score(b) - score(a) || splitItem(a.item).name.localeCompare(splitItem(b.item).name)).slice(0, 6).map(item => item.item) });
  const items = (order?.ids ?? []).map(id => all.find(item => item.item === id)).filter((item): item is ItemView => !!item);
  return <Card role="region" aria-labelledby="fleet-assignments-title" className="min-w-0 gap-0 overflow-hidden border py-0 ring-0">
    <CardHeader className="flex flex-row flex-wrap items-center gap-2.5 border-b px-3.5 pt-2.5 [--card-spacing:--spacing(2.5)]"><CardTitle><h2 id="fleet-assignments-title" className="text-xs font-semibold tracking-wider text-muted-foreground uppercase">Assignments</h2></CardTitle><div className="ml-auto flex flex-wrap items-center gap-2 text-xs text-muted-foreground"><span className="inline-flex items-center gap-1"><span aria-hidden className="size-3.5 rounded border border-primary bg-primary" />Explicit</span><span className="inline-flex items-center gap-1"><span aria-hidden className="size-3.5 rounded border border-dashed border-primary/35 bg-brand-soft" />Default</span><Button variant="ghost" size="sm" onClick={() => navigate('Assignment matrix')}>Open matrix</Button></div></CardHeader>
    <CardContent className="min-w-0 p-0"><ErrorNotice error={resource.error} /><ErrorNotice error={errors[machineId]} />
      {!resource.data && resource.loading && <Loading />}
      {current && (columns.length && items.length ? <Table aria-label="This Machine assignments"><TableHeader><TableRow><TableHead className="text-xs">Item</TableHead>{columns.map(harness => <TableHead key={harness} className="min-w-20 text-center font-mono text-xs">{harness}</TableHead>)}</TableRow></TableHeader><TableBody>{items.map(item => <TableRow key={item.item}><TableHead scope="row" className="font-mono text-xs font-normal">{splitItem(item.item).name}<span className="ml-2 text-muted-foreground">{splitItem(item.item).kind}</span></TableHead>{columns.map(harness => {
        const state = cellState(current, harness, item.item), { kind, name } = splitItem(item.item);
        return <TableCell key={harness} className="text-center"><label className="inline-flex min-h-8 min-w-8 cursor-pointer items-center justify-center"><Checkbox className={matrixCheckbox(state.explicit, state.enabled)} checked={state.enabled} disabled={pending[machineId]} aria-label={`${item.item} in ${machineId} / ${harness}, ${state.explicit ? 'explicit' : 'inherited'}`} onCheckedChange={enabled => void write(machineId, [{ harness, kind: kind as 'skill' | 'rule', name, enabled }])} /></label></TableCell>;
      })}</TableRow>)}</TableBody></Table> : <Nothing className="rounded-none border-0" title={columns.length ? 'No Skills or Rules' : 'No Harnesses'}>Assignments for this Machine will appear here.</Nothing>)}
      {pending[machineId] && <div role="status" className="px-3.5 py-2 text-xs"><Pending pending>Saving Assignment for {machineId}…</Pending></div>}
    </CardContent>
  </Card>;
}
