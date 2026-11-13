import { invoke } from '@tauri-apps/api/core';
import type * as T from './types';

export const api = {
  init: () => invoke<void>('init'),
  push: () => invoke<T.PushResult>('push'),
  createItem: (request: T.CreateItemRequest) => invoke<void>('create_item', { request }),
  deleteItem: (request: T.DeleteItemRequest) => invoke<void>('delete_item', { request }),
  renameItem: (request: T.RenameItemRequest) => invoke<T.RenameItemResponse>('rename_item', { request }),
  checkImports: () => invoke<T.ImportsView>('check_imports'),
  removeImport: (name: string) => invoke<void>('remove_import', { name }),
  conflictDiff: (request: T.ConflictDiffRequest) => invoke<T.DiffView>('conflict_diff', { request }),
  setPreferredEditor: (editorId: string) => invoke<void>('set_preferred_editor', { editorId }),
  skills: () => invoke<T.ContentEntry[]>('skills'),
  skillTree: (query: T.ContentQuery) => invoke<T.FileEntry[]>('skill_tree', { query }),
  skillFile: (query: T.ContentQuery & { path: string }) => invoke<T.FileContent>('skill_file', { query }),
  adoptUnmanaged: (target: T.ConflictTarget) => invoke<void>('adopt_unmanaged', { target }),
  fork: (name: string) => invoke<void>('fork', { name }),
  editors: (refresh = false) => invoke<T.EditorsView>('editors', { refresh }),
  openInEditor: (editorId: string, item: string) => invoke<void>('open_in_editor', { editorId, item }),
  status: () => invoke<T.StatusView>('status'),
  fleet: () => invoke<T.FleetView>('fleet'),
  items: () => invoke<T.ItemView[]>('items'),
  draft: () => invoke<T.DraftEntry[]>('draft'),
  diff: (item: string) => invoke<T.DiffView>('diff', { item }),
  commitMessage: (items: string[]) => invoke<T.CommitMessage>('commit_message', { items }),
  commit: (request: T.CommitRequest) => invoke<T.CommitResponse>('commit', { request }),
  restore: (request: T.RestoreRequest) => invoke<T.RestoreResponse>('restore', { request }),
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
  logFacets: () => invoke<T.LogFacets>('log_facets'),
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
