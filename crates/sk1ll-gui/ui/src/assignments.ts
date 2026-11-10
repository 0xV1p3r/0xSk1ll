import { useEffect, useRef, useState } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { api, errorMessage } from './api';
import { applyToggle, mergeAssignments } from './model';
import { cn } from '@/lib/utils';
import type { CachedAssignmentSnapshot as AssignmentSnapshot, ToggleChange } from './types';

export const matrixCheckbox = (explicit: boolean, enabled: boolean) => cn('size-[22px] rounded-[6px]', !enabled && 'border-dashed', !explicit && 'border-dashed border-primary/35 bg-brand-soft data-checked:border-primary/35 data-checked:bg-brand-soft data-checked:text-brand-ink', !explicit && !enabled && 'bg-muted/60 border-control-border/50');

export function useAssignmentWrites<D extends { assignments: Record<string, AssignmentSnapshot> }>(resource: { data: D | undefined; setData: Dispatch<SetStateAction<D | undefined>>; refresh: () => Promise<void> }) {
  const locks = useRef(new Set<string>());
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  // Keep optimistic state separate from refreshes arriving while a write is in flight.
  const [overrides, setOverrides] = useState<Record<string, AssignmentSnapshot & { saving?: boolean }>>({});
  useEffect(() => {
    if (!resource.data) return;
    const accepted = resource.data.assignments;
    setOverrides(old => Object.fromEntries(Object.entries(old).filter(([machine, override]) =>
      override.saving || !accepted[machine] || accepted[machine].revision < override.revision)));
  }, [resource.data]);
  const write = async (machine: string, changes: ToggleChange[]) => {
    if (!changes.length || !resource.data || locks.current.has(machine)) return;
    locks.current.add(machine);
    setPending(p => ({ ...p, [machine]: true })); setErrors(e => ({ ...e, [machine]: '' }));
    const before = overrides[machine] ?? resource.data.assignments[machine];
    const clearOverride = () => setOverrides(old => { const next = { ...old }; delete next[machine]; return next; });
    if (changes.some(change => change.kind === 'skill' || change.kind === 'rule')) {
      setOverrides(old => ({ ...old, [machine]: { ...before, assignment: changes.reduce(applyToggle, before.assignment), saving: true } }));
    }
    try {
      const response = await api.setAssignment(machine, { changes });
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
  const assignment = (machine: string) => {
    const override = overrides[machine], accepted = resource.data!.assignments[machine];
    return override && (override.saving || override.revision > accepted.revision) ? override.assignment : accepted.assignment;
  };
  return { pending, errors, write, assignment };
}
