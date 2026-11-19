import { useEffect, useRef, useState } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { api, errorMessage, errorStatus } from './api';
import { applyToggle, mergeAssignments, own, stamped } from './model';
import type { StampedSnapshot } from './model';
import { cn } from '@/lib/utils';
import type { Assignment, Expected, ToggleChange } from './types';

// Explicit: solid box with a pinned corner dot. Inherited: hollow dashed outline with a ghost tick. Shape alone tells them apart.
export const matrixCheckbox = (explicit: boolean, enabled: boolean) => cn('size-[22px] rounded-[6px]', explicit
  ? 'before:absolute before:-top-[3px] before:-right-[3px] before:size-2 before:rounded-full before:bg-foreground before:ring-2 before:ring-card'
  : 'border-[1.5px] border-dashed bg-transparent data-checked:bg-transparent', !explicit && (enabled
  ? 'border-primary/70 data-checked:border-primary/70 data-checked:text-brand-ink dark:border-primary/60 dark:data-checked:border-primary/60'
  : 'border-control-border/80'));

const withEntry = <V,>(map: ReadonlyMap<string, V>, key: string, value: V | undefined) => {
  const next = new Map(map);
  if (value === undefined) next.delete(key); else next.set(key, value);
  return next;
};
export function useAssignmentWrites<D extends { assignments: Record<string, StampedSnapshot> }>(resource: { data: D | undefined; setData: Dispatch<SetStateAction<D | undefined>>; refresh: () => Promise<void> }) {
  const locks = useRef(new Set<string>());
  // A write started from an older render (an Undo, say) checks the data as it is now.
  const latest = useRef(resource.data);
  useEffect(() => { latest.current = resource.data; }, [resource.data]);
  // Keyed by Machine id in Maps, so no id can collide with an object's own properties.
  const [pending, setPending] = useState<ReadonlyMap<string, boolean>>(new Map());
  const [errors, setErrors] = useState<ReadonlyMap<string, string>>(new Map());
  // The optimistic state while a write is in flight, separate from refreshes arriving meanwhile.
  const [overrides, setOverrides] = useState<ReadonlyMap<string, Assignment>>(new Map());
  // Resolves to the accepted snapshot once the Server took the changes, otherwise null. A Machine whose Assignment never loaded (or left the Fleet) is never written.
  // With `expected`, a write the authority refuses because the Assignment changed since (HTTP 409) calls `onConflict` instead of showing an error.
  const write = async (machine: string, changes: ToggleChange[], options: { expected?: Expected; onConflict?: () => void } = {}) => {
    const before = latest.current && own(latest.current.assignments, machine);
    if (!changes.length || !before || locks.current.has(machine)) return null;
    // The registration the write was meant for; a reply arriving after the Machine joined again is dropped.
    const registration = before.registration;
    locks.current.add(machine);
    setPending(p => withEntry(p, machine, true)); setErrors(e => withEntry(e, machine, undefined));
    if (changes.some(change => change.kind === 'skill' || change.kind === 'rule')) {
      setOverrides(old => withEntry(old, machine, changes.reduce(applyToggle, overrides.get(machine) ?? before.assignment)));
    }
    try {
      const response = { ...await stamped(() => api.setAssignment(machine, { changes, ...options.expected && { expected: options.expected } })), registration };
      // Accept each mutation directly, independently of superseded refresh generations, unless the Machine left or joined again meanwhile.
      resource.setData(previous => previous && own(previous.assignments, machine)?.registration === registration ? { ...previous,
        assignments: mergeAssignments(previous.assignments, { [machine]: response }),
      } : previous);
      return response;
    } catch (error) {
      if (options.expected && options.onConflict && errorStatus(error) === 409) options.onConflict();
      else setErrors(old => withEntry(old, machine, errorMessage(error)));
      return null;
    } finally {
      setOverrides(old => withEntry(old, machine, undefined));
      locks.current.delete(machine); setPending(p => withEntry(p, machine, undefined));
      // useResource surfaces refresh failures while retaining accepted Assignment snapshots.
      void resource.refresh();
    }
  };
  // Only for Machines with a loaded Assignment.
  const assignment = (machine: string) => overrides.get(machine) ?? own(resource.data!.assignments, machine)!.assignment;
  return { pending, errors, write, assignment };
}
