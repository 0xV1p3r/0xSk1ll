import { useEffect, useRef, useState } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { api, errorMessage } from './api';
import { applyToggle, mergeAssignments } from './model';
import { cn } from '@/lib/utils';
import type { CachedAssignmentSnapshot as AssignmentSnapshot, ToggleChange } from './types';

// Explicit: solid box with a pinned corner dot. Inherited: hollow dashed outline with a ghost tick. Shape alone tells them apart.
export const matrixCheckbox = (explicit: boolean, enabled: boolean) => cn('size-[22px] rounded-[6px]', explicit
  ? 'before:absolute before:-top-[3px] before:-right-[3px] before:size-2 before:rounded-full before:bg-foreground before:ring-2 before:ring-card'
  : 'border-[1.5px] border-dashed bg-transparent data-checked:bg-transparent', !explicit && (enabled
  ? 'border-primary/70 data-checked:border-primary/70 data-checked:text-brand-ink dark:border-primary/60 dark:data-checked:border-primary/60'
  : 'border-control-border/80'));

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
  // Resolves true once the Server accepted the changes.
  const write = async (machine: string, changes: ToggleChange[]) => {
    if (!changes.length || !resource.data || locks.current.has(machine)) return false;
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
      return true;
    } catch (error) {
      clearOverride(); setErrors(old => ({ ...old, [machine]: errorMessage(error) }));
      return false;
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
