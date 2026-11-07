import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Checkbox } from '@/components/ui/checkbox';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api, errorMessage } from './api';
import { applyToggle, cellState, emptyHarness, itemStateLabel, mergeAssignments, splitItem } from './model';
import { useResource } from './hooks';
import { ErrorNotice, Loading, Nothing, Pending } from './shared';
import type { CachedAssignmentSnapshot as AssignmentSnapshot, ItemView, StatusView, ToggleChange } from './types';

const loadMatrix = async () => {
  const [fleet, items] = await Promise.all([api.fleet(), api.items()]);
  const assignments: Record<string, AssignmentSnapshot> = Object.fromEntries(await Promise.all(fleet.machines.map(async machine =>
    [machine.id, await api.assignment(machine.id)] as const)));
  return { fleet, items, assignments };
};
type MatrixData = Awaited<ReturnType<typeof loadMatrix>>;
const mergeMatrix = (previous: MatrixData | undefined, next: MatrixData): MatrixData => ({
  ...next, assignments: mergeAssignments(previous?.assignments ?? {}, next.assignments),
});
export function Matrix({ revision, status }: { revision: number; status: StatusView }) {
  const locks = useRef(new Set<string>());
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const resource = useResource(loadMatrix, revision, mergeMatrix);
  // Keep optimistic state separate from refreshes arriving while a write is in flight.
  const [overrides, setOverrides] = useState<Record<string, AssignmentSnapshot & { saving?: boolean }>>({});
  useEffect(() => {
    if (!resource.data) return;
    const accepted = resource.data.assignments;
    setOverrides(old => Object.fromEntries(Object.entries(old).filter(([machine, override]) =>
      override.saving || !accepted[machine] || accepted[machine].revision < override.revision)));
  }, [resource.data]);
  const write = async (machine: string, change: ToggleChange) => {
    if (!resource.data || locks.current.has(machine)) return;
    locks.current.add(machine);
    setPending(p => ({ ...p, [machine]: true })); setErrors(e => ({ ...e, [machine]: '' }));
    const before = overrides[machine] ?? resource.data.assignments[machine];
    const clearOverride = () => setOverrides(old => { const next = { ...old }; delete next[machine]; return next; });
    if (change.kind === 'skill' || change.kind === 'fragment') {
      setOverrides(old => ({ ...old, [machine]: { ...before, assignment: applyToggle(before.assignment, change), saving: true } }));
    }
    try {
      const response = await api.setAssignment(machine, { changes: [change] });
      // Accept each mutation directly, independently of superseded refresh generations.
      resource.setData(previous => previous && { ...previous,
        assignments: mergeAssignments(previous.assignments, { [machine]: response }),
      });
      setOverrides(old => ({ ...old, [machine]: response }));
    } catch (error) {
      clearOverride(); setErrors(old => ({ ...old, [machine]: errorMessage(error) }));
    } finally {
      locks.current.delete(machine); setPending(p => ({ ...p, [machine]: false }));
      // useResource surfaces refresh failures while retaining accepted Assignment snapshots.
      void resource.refresh();
    }
  };
  const data = resource.data;
  if (!data) return <><ErrorNotice error={resource.error} />{resource.loading && <Loading />}</>;
  const items = data.items.filter(i => ['skill', 'fragment'].includes(splitItem(i.item).kind));
  const groups = data.fleet.machines.map(machine => ({ machine, harnesses: [...new Set([
    ...(machine.report?.harnesses ?? []), ...Object.keys(data.assignments[machine.id].assignment.harnesses),
  ])].sort() }));
  const columns = groups.flatMap(({ machine, harnesses }) => harnesses.map(harness => ({ machine: machine.id, harness })));
  const assignment = (machine: string) => {
    const override = overrides[machine], accepted = data.assignments[machine];
    return override && (override.saving || override.revision > accepted.revision) ? override.assignment : accepted.assignment;
  };
  const cell = (item: ItemView, machine: string, harness: string) => {
    const state = cellState(assignment(machine), harness, item.item);
    const localOnly = !item.in_baseline && machine !== status.machine_id;
    const reason = localOnly ? `${itemStateLabel(item)}. This item can only be assigned on this Machine until it reaches the Baseline.`
      : `${state.explicit ? 'Explicit Assignment' : 'Inherited from the default'}. Toggle to set an explicit Assignment.`;
    return <TableCell key={`${machine}/${harness}`}><Tooltip><TooltipTrigger render={<span className="cell" tabIndex={localOnly ? 0 : undefined} />}>
      <Checkbox checked={state.enabled} disabled={localOnly || pending[machine]}
        aria-label={`${item.item} in ${machine} / ${harness}, ${state.explicit ? 'explicit' : 'inherited'}`}
        onCheckedChange={enabled => void write(machine, { harness, kind: splitItem(item.item).kind as 'skill' | 'fragment',
          name: splitItem(item.item).name, enabled })} />
      <span className="cell-origin">{state.explicit ? 'Explicit' : 'Default'}</span>
      {state.explicit && !localOnly && <Button size="xs" variant="ghost" disabled={pending[machine]}
        aria-label={`Use default for ${item.item} in ${machine} / ${harness}`}
        onClick={() => void write(machine, { harness, kind: splitItem(item.item).kind as 'skill' | 'fragment',
          name: splitItem(item.item).name, enabled: null })}>Inherit</Button>}
    </TooltipTrigger><TooltipContent>{reason}</TooltipContent></Tooltip></TableCell>;
  };
  return <><p className="description">Assignment takes effect immediately. Explicit toggles override a Harness default.</p>
    <ErrorNotice error={resource.error} />{Object.entries(errors).filter(([, value]) => value).map(([id, error]) =>
      <ErrorNotice key={id} error={`${id}: ${error}`} />)}
    {Object.entries(pending).filter(([, value]) => value).map(([id]) => <p key={id} role="status"><Pending pending>Saving Assignment for {id}…</Pending></p>)}
    <Table className="matrix"><TableHeader><TableRow><TableHead rowSpan={2}>Skill / Fragment</TableHead>
      {groups.filter(g => g.harnesses.length).map(g => <TableHead key={g.machine.id} colSpan={g.harnesses.length}>
        {g.machine.id}{g.machine.id === status.machine_id && ' (this Machine)'}</TableHead>)}</TableRow><TableRow>
      {columns.map(c => <TableHead key={`${c.machine}/${c.harness}`}>{c.harness}</TableHead>)}
    </TableRow></TableHeader><TableBody>
      {(['skill', 'fragment'] as const).map(kind => <TableRow key={kind}><TableCell>Default for new {kind === 'skill' ? 'Skills' : 'Fragments'}</TableCell>
        {columns.map(c => <TableCell key={`${c.machine}/${c.harness}`}><Checkbox
          checked={(assignment(c.machine).harnesses[c.harness] ?? emptyHarness())[kind === 'skill' ? 'new_skills_default' : 'new_fragments_default']}
          disabled={pending[c.machine]} aria-label={`Default for new ${kind}s in ${c.machine} / ${c.harness}`}
          onCheckedChange={enabled => void write(c.machine, { harness: c.harness,
            kind: kind === 'skill' ? 'new_skills_default' : 'new_fragments_default', enabled })} /></TableCell>)}</TableRow>)}
      {items.map(item => <TableRow key={item.item}><TableCell><div className="item-label"><code>{item.item}</code>
        {itemStateLabel(item) && <Badge variant="outline">{itemStateLabel(item)}</Badge>}</div>
        {item.description && <p className="meta item-description">{item.description}</p>}</TableCell>
        {columns.map(c => cell(item, c.machine, c.harness))}</TableRow>)}
    </TableBody></Table>{!items.length && <Nothing>Add a Skill or Fragment to this Machine's Draft to get started.</Nothing>}
    {!columns.length && <Nothing>No Harnesses reported. Check the Machine configuration in Settings.</Nothing>}
  </>;
}
