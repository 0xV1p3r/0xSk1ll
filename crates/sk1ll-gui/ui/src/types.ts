// Daemon serde contract: sk1ll-daemon/src/client.rs and sk1ll-proto/src/lib.rs.
// ItemRef and PathBuf serialize as strings; timestamps are Unix seconds.
export type ItemRef = string;
export type MachineStatus = 'offline' | 'blocked' | 'conflict' | 'behind' | 'drafting' | 'in_sync';
export type Connection = 'connecting' | 'connected' | 'offline' | 'auth_rejected' | 'revoked' | 'standalone';
export interface MachineReport {
  baseline: string | null; drafts: ItemRef[]; unpushed: ItemRef[];
  conflicts: { item: ItemRef; harness: string }[]; harnesses: string[];
  blocked: boolean; clashes: ItemRef[]; behind: boolean;
}
export interface StatusView {
  mode: 'joined' | 'standalone'; has_origin: boolean; repo_ready: boolean;
  machine_id: string; server_url: string | null; repo: string; headline: MachineStatus; connection: Connection;
  last_error: string | null; revoked: boolean; server_baseline: string | null; assignment_revision: number;
  awaiting_assignment: boolean; auth_rejected_since: number | null; pending_discards: number; report: MachineReport;
  version?: string;
}
export interface MachineRow {
  id: string; status: MachineStatus; connected: boolean; joined_at: number; last_seen: number | null;
  report: MachineReport | null;
}
export interface FleetView { baseline: string | null; machines: MachineRow[] }
export interface HarnessAssignment {
  new_skills_default: boolean; new_rules_default: boolean;
  skills: Record<string, boolean>; rules: Record<string, boolean>;
}
export interface Assignment { harnesses: Record<string, HarnessAssignment> }
export type ToggleKind = 'skill' | 'rule' | 'new_skills_default' | 'new_rules_default';
export interface ToggleChange { harness: string; kind: ToggleKind; name?: string; enabled: boolean | null }
export interface AssignmentChanges { changes: ToggleChange[] }
export interface AssignmentSnapshot extends Assignment { revision: number }
export interface CachedAssignmentSnapshot { assignment: Assignment; revision: number }
export interface AssignmentUpdate extends CachedAssignmentSnapshot { entries: LogEntry[] }
export type LogCause = { type: 'change' | 'item_removed' | 'default_materialized' | 'machine_removed' | 'harness_added' }
  | { type: 'undo'; entry: number };
export interface LogEntry {
  id: number; at: number; author: string; machine: string; harness: string; kind: ToggleKind;
  name: string | null; before: boolean | null; after: boolean | null; cause: LogCause;
}
// Filters run on the Server before `limit`, so the last id of a page is the next `before`.
// `item` matches a substring of `<kind>:<name>` ignoring ASCII case; empty strings match everything.
export interface LogQuery { machine?: string; harness?: string; author?: string; item?: string; before?: number; limit?: number }
// Distinct values over the whole Assignment log, sorted.
export interface LogFacets { machines: string[]; harnesses: string[]; authors: string[] }
// The GUI bridge adds a byte fingerprint to the Daemon's Draft entry.
export interface DraftEntry { item: ItemRef; change: 'added' | 'modified' | 'deleted'; paths: string[]; fingerprint: string }
export interface DiffView { item: ItemRef; diff: string }
export interface CommitMessage { items: ItemRef[]; message: string }
export interface CommitRequest { items: string[]; message?: string; author?: string }
export interface CommitResponse { sha: string; items: ItemRef[]; message: string; pushed: boolean; push_error: string | null }
export interface RestoreRequest { items: string[]; all: boolean }
export interface RestoreResponse { discarded: ItemRef[] }
export type PullResult = { state: 'up_to_date' } | { state: 'updated'; from: string | null; to: string }
  | { state: 'blocked'; baseline: string; items: ItemRef[] };
export interface ConflictView {
  kind?: string; id?: string; item?: ItemRef; path?: string; displaced_at?: number;
  // Harness conflicts only: newest mtime of the Harness content (when it was last edited), not a first-seen time.
  detected_at?: number | null;
  harness: string; name: string; items: ItemRef[]; dest: string; reason: string | null; adoptable: boolean;
}
export interface ConflictTarget { harness: string; name: string }
export interface HarnessView {
  name: string; origin: string;
  harness: { skills: string | null; instructions: string | null; instructions_format: 'block' | 'rules'; mode: 'symlink' | 'copy' };
}
export interface HarnessChoice { name: string; installed: boolean; enabled: boolean; harness: HarnessView['harness'] }
// How the app can start the Daemon itself; not a Daemon type.
export type ServiceManager = 'systemd' | 'launchd';
export interface ServiceInfo { default_socket: boolean; binary: string | null; manager: ServiceManager; available: boolean; active: boolean; restartable: boolean; unit: string | null }
export type DaemonStarted = ServiceManager | 'process';
export interface ImportUpdate { name: string; source: string; ref: string; locked: string; upstream: string }
export interface ImportsView { imports?: ImportEntry[]; checked_at: number | null; updates: ImportUpdate[]; errors: { name: string; message: string }[] }
export interface ImportUpdated { name: string; old_commit: string | null; new_commit: string; changed: boolean }
// `admin_token` replaces `code` for the first Machine of a new Fleet.
export interface JoinApiRequest { server_url: string; code: string; admin_token?: string; machine_id?: string; copy_from?: string; repo?: string }
export interface Joined { machine_id: string; repo: string; setup: 'cloned' | 'initialized' | 'adopted'; harnesses: string[]; revision: number }
export interface JoinCode { code: string }
export interface CommandError { message: string; status: number | null; unreachable: boolean }
export interface DaemonConnection { reachable: boolean; message: string | null; retry_in_ms: number }
export type DaemonEvent = { type: 'status' | 'draft' | 'fleet' } | { type: 'assignment'; revision: number }
  | { type: 'imports'; updates: ImportUpdate[] };

export interface ItemView {
  item: ItemRef; in_baseline: boolean; draft: DraftEntry['change'] | null; unpushed: boolean; description: string | null;
}

export interface SkillCandidate { path: string; default_name: string; description: string | null; taken: boolean }
export interface ResolvedSource { source: string; git_ref: string; commit: string; candidates: SkillCandidate[] }
export interface ResolveRequest { link: string; path?: string }
export interface AddImportRequest { name: string; source: string; path: string; ref: string; commit?: string }

export type ContentType = 'personal' | 'import' | 'rule' | 'unmanaged';
export interface ContentVariant {
  hash: string; description: string | null; locations: { harness: string; path: string }[];
}
export interface ContentEntry {
  name: string; item: ItemRef | null; type: ContentType; description: string | null;
  draft: DraftEntry['change'] | null; in_baseline: boolean; path: string | null;
  enabled_harnesses: string[]; clashes_with_repo: boolean; variants: ContentVariant[];
}
export interface ContentQuery { item: string; hash?: string; harness?: string }
export interface FileEntry { path: string; kind: 'file' | 'directory' | 'symlink'; size: number }
export interface FileContent { path: string; text: string | null; binary: boolean; too_large: boolean; size: number }
export interface Editor { id: string; name: string; kind: 'gui' | 'terminal' | 'system' }
export interface EditorsView { editors: Editor[]; preferred_editor: string }

export interface PushResult { pushed: boolean }
export interface CreateItemRequest { kind: 'skill' | 'rule'; name: string; description?: string }
// Personal Skills and Rules only; Imports use removeImport. Both are Draft changes.
export interface DeleteItemRequest { kind: 'skill' | 'rule'; name: string }
export interface RenameItemRequest { kind: 'skill' | 'rule'; name: string; to: string }
// The rename is in the Draft either way. `migrated` lists the Harnesses whose explicit toggles on this
// Machine were copied to the new name; `assignment_error` says why they were not (e.g. offline).
// Other Machines are not migrated: after the push, the old name's toggles are dropped and the new name follows their defaults.
export interface RenameItemResponse { item: ItemRef; migrated: string[]; assignment_error: string | null }
export type ConflictDiffRequest = { harness: string; name: string; id?: never } | { id: string; harness?: never; name?: never };
export interface ImportEntry { name: string; source: string; ref: string; path: string; locked: string | null }
