import { useCallback, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api } from './api';
import { useResource } from './hooks';
import { matrixCheckbox, useAssignmentWrites } from './assignments';
import { cellState, harnessOf, mergeAssignments, splitItem, stamped, terms } from './model';
import type { StampedSnapshot } from './model';
import { CheckboxSwatch, ErrorNotice, KindBadge, Loading, Nothing, Panel, PanelToolbar, Pending, SectionTitle } from './shared';
import type { ItemView } from './types';

type Data = { items: ItemView[]; assignments: Record<string, StampedSnapshot> };
const merge = (previous: Data | undefined, next: Data): Data => ({ ...next, assignments: mergeAssignments(previous?.assignments ?? {}, next.assignments) });
export function FleetAssignments({ revision, machineId, harnesses, navigate, registration }: { revision: number; machineId: string; harnesses: string[]; registration?: string | null; navigate: (screen: 'Assignment matrix') => void }) {
  const load = useCallback(async (): Promise<Data> => {
    const [items, snapshot] = await Promise.all([api.items(), stamped(() => api.assignment(machineId))]);
    return { items, assignments: { [machineId]: snapshot } };
  }, [machineId]);
  const resource = useResource(load, revision, merge);
  const { pending, errors, write, assignment } = useAssignmentWrites(resource);
  const current = resource.data && assignment(machineId);
  const shown = resource.data?.assignments[machineId];
  const expected = registration && shown ? { revision: shown.revision, registration: shown.registration ?? registration } : undefined;
  const columns = [...harnesses].sort();
  const score = (item: ItemView) => Math.max(0, ...columns.map(harness => {
    const state = cellState(current!, harness, item.item, item.id), defaults = harnessOf(current!, harness);
    return state.explicit ? state.enabled !== defaults[splitItem(item.item).kind === 'skill' ? 'new_skills_default' : 'new_rules_default'] ? 2 : 1 : 0;
  }));
  const all = resource.data?.items.filter(item => item.id != null && item.draft !== 'deleted') ?? [];
  // Rank once per external refresh so toggling a row never moves or removes it under the pointer or focus.
  const key = `${revision}:${all.map(item => item.id).join()}`;
  const [order, setOrder] = useState<{ key: string; ids: string[] }>();
  if (current && order?.key !== key && !pending.get(machineId)) setOrder({ key, ids: [...all].sort((a, b) => score(b) - score(a) || splitItem(a.item).name.localeCompare(splitItem(b.item).name)).slice(0, 6).map(item => item.id!) });
  const items = (order?.ids ?? []).map(id => all.find(item => item.id === id)).filter((item): item is ItemView => !!item);
  return <Panel label="This Machine's Assignments" className="min-w-0 gap-0 overflow-hidden border py-0 ring-0">
    <PanelToolbar><SectionTitle id="fleet-assignments-title">This Machine's Assignments</SectionTitle><div className="ml-auto flex flex-wrap items-center gap-3 text-xs text-muted-foreground">{items.length > 0 && <><span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit enabled />{terms.explicit}</span><span className="inline-flex items-center gap-1.5"><CheckboxSwatch explicit={false} enabled />{terms.inherited}</span></>}{all.length > items.length && <span className="tabular-nums">{items.length} of {all.length}</span>}<Button variant="ghost" size="sm" onClick={() => navigate('Assignment matrix')}>Open matrix</Button></div></PanelToolbar>
    <div className="min-w-0 p-0"><ErrorNotice error={resource.error} /><ErrorNotice error={errors.get(machineId)} />
      {!resource.data && resource.loading && <Loading />}
      {current && (columns.length && items.length ? <Table aria-label="This Machine assignments"><TableHeader><TableRow><TableHead className="text-xs">Item</TableHead>{columns.map(harness => <TableHead key={harness} className="min-w-20 text-center text-xs">{harness}</TableHead>)}</TableRow></TableHeader><TableBody>{items.map(item => <TableRow key={item.id!}><TableHead scope="row" className="text-sm font-medium"><span className="inline-flex items-center gap-2">{splitItem(item.item).name}<KindBadge kind={splitItem(item.item).kind} /></span></TableHead>{columns.map(harness => {
        const state = cellState(current, harness, item.item, item.id), { kind, name } = splitItem(item.item);
        return <TableCell key={harness} className="text-center"><label className="inline-flex min-h-8 min-w-8 cursor-pointer items-center justify-center"><Checkbox className={matrixCheckbox(state.explicit, state.enabled)} checked={state.enabled} disabled={!!pending.get(machineId)} aria-label={`${item.item} in ${machineId} / ${harness}, ${state.explicit ? terms.explicit : terms.inherited}`} onCheckedChange={enabled => void write(machineId, [{ harness, kind: kind as 'skill' | 'rule', item_id: item.id!, name, enabled }], { expected })} /></label></TableCell>;
      })}</TableRow>)}</TableBody></Table> : <Nothing className="rounded-none border-0" title={columns.length ? 'No Skills or Rules' : 'No Harnesses'}>Assignments for this Machine will appear here.</Nothing>)}
      {pending.get(machineId) && <div role="status" className="px-3.5 py-2 text-xs"><Pending pending>Saving Assignment for {machineId}…</Pending></div>}
    </div>
  </Panel>;
}
