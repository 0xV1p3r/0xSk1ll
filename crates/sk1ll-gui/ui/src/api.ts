import { invoke } from '@tauri-apps/api/core';
import type * as T from './types';

export const api = {
  status: () => invoke<T.StatusView>('status'),
  fleet: () => invoke<T.FleetView>('fleet'),
  items: () => invoke<T.ItemView[]>('items'),
  draft: () => invoke<T.DraftEntry[]>('draft'),
  diff: (item: string) => invoke<T.DiffView>('diff', { item }),
  promoteMessage: (items: string[]) => invoke<T.PromoteMessage>('promote_message', { items }),
  promote: (request: T.PromoteRequest) => invoke<T.PromoteResponse>('promote', { request }),
  reset: (request: T.ResetRequest) => invoke<T.ResetResponse>('reset', { request }),
  pull: () => invoke<T.PullResult>('pull'),
  conflicts: () => invoke<T.ConflictView[]>('conflicts'),
  recovered: () => invoke<T.ConflictView[]>('recovered'),
  adopt: (target: T.ConflictTarget) => invoke<void>('adopt', { target }),
  overwrite: (target: T.ConflictTarget) => invoke<void>('overwrite', { target }),
  adoptDisplaced: (id: string) => invoke<void>('adopt_displaced', { id }),
  discardDisplaced: (id: string) => invoke<void>('discard_displaced', { id }),
  harnesses: () => invoke<T.HarnessView[]>('harnesses'),
  imports: () => invoke<T.ImportsView>('imports'),
  resolveImport: (request: T.ResolveRequest) => invoke<T.ResolvedSource>('resolve_import', { request }),
  addImport: (request: T.AddImportRequest) => invoke<T.ImportUpdated>('add_import', { request }),
  updateImport: (name: string) => invoke<T.ImportUpdated>('update_import', { name }),
  assignment: async (machine: string): Promise<T.CachedAssignmentSnapshot> => {
    const { revision, ...assignment } = await invoke<T.AssignmentSnapshot>('assignment', { machine });
    return { assignment, revision };
  },
  setAssignment: (machine: string, changes: T.AssignmentChanges) =>
    invoke<T.AssignmentUpdate>('set_assignment', { machine, changes }),
  log: (query: T.LogQuery) => invoke<T.LogEntry[]>('log', { query }),
  undo: (entry: number) => invoke<T.AssignmentUpdate>('undo', { entry }),
  join: (request: T.JoinApiRequest) => invoke<T.Joined>('join', { request }),
  joinCode: () => invoke<T.JoinCode>('join_code'),
  removeMachine: (machine: string) => invoke<void>('remove_machine', { machine }),
  connectionInfo: () => invoke<string>('connection_info'),
};

export function errorMessage(error: unknown): string {
  if (typeof error === 'object' && error && 'message' in error) return String(error.message);
  return String(error);
}
