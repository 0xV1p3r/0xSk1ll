// Daemon serde contract: sk1ll-daemon/src/client.rs and sk1ll-proto/src/lib.rs.
// ItemRef and PathBuf serialize as strings; timestamps are Unix seconds.
export type ItemRef = string;
export type ItemId = string;
export type MachineStatus = 'offline' | 'blocked' | 'conflict' | 'behind' | 'drafting' | 'in_sync';
export type Connection = 'connecting' | 'connected' | 'offline' | 'auth_rejected' | 'revoked' | 'standalone';
export interface MachineReport {
  seq: number; items: { item: ItemRef; id: ItemId }[];
  baseline: string | null; drafts: ItemRef[]; unpushed: ItemRef[];
  conflicts: { item: ItemRef; harness: string }[]; harnesses: string[];
  blocked: boolean; clashes: ItemRef[]; behind: boolean;
}
export interface StatusView {
  mode: 'joined' | 'standalone'; has_origin: boolean; repo_ready: boolean;
  machine_id: string; server_url: string | null; repo: string; headline: MachineStatus; connection: Connection;
  last_error: string | null; revoked: boolean; server_baseline: string | null; assignment_revision: number;
  awaiting_assignment: boolean; auth_rejected_since: number | null; report: MachineReport;
  version?: string;
}
// What a Machine may manage, from its join code.
export type Scope = 'fleet' | 'self';
// `registration`: random and new for every join (two joins can share a `joined_at` second).
export interface MachineRow {
  id: string; status: MachineStatus; connected: boolean; joined_at: number; last_seen: number | null;
  report: MachineReport | null; scope?: Scope; registration?: string | null;
}
export interface FleetView { baseline: string | null; machines: MachineRow[] }
export interface HarnessAssignment {
  new_skills_default: boolean; new_rules_default: boolean;
  skills: Record<string, boolean>; rules: Record<string, boolean>;
}
export interface Assignment { harnesses: Record<string, HarnessAssignment> }
export type ToggleKind = 'skill' | 'rule' | 'new_skills_default' | 'new_rules_default';
export interface ToggleChange { harness: string; kind: ToggleKind; item_id?: ItemId; name?: string; enabled: boolean | null }
// `expected`: apply only to this revision (and registration, when given), else HTTP 409 and nothing changes.
export interface Expected { revision: number; registration?: string | null }
export interface AssignmentChanges { changes: ToggleChange[]; expected?: Expected | null }
// `registration` comes with a Server's GET of one Machine's Assignment.
export interface AssignmentSnapshot extends Assignment { revision: number; registration?: string | null }
// `registration`: which registration of the Machine it was read for (see `registrationOf`).
export interface CachedAssignmentSnapshot { assignment: Assignment; revision: number; registration?: string | null }
export interface AssignmentUpdate extends Omit<CachedAssignmentSnapshot, 'registration'> { entries: LogEntry[]; errors?: string[] }
export type LogCause = { type: 'change' | 'orphan_pruned' | 'default_materialized' | 'machine_removed' | 'harness_added' }
  | { type: 'undo'; entry: number } | { type: 'harness_renamed'; from: string; to: string };
export interface LogEntry {
  id: number; at: number; author: string; machine: string; harness: string; kind: ToggleKind;
  item_id: ItemId | null; name: string | null; before: boolean | null; after: boolean | null; cause: LogCause;
}
// Filters run on the Server before `limit`, so the last id of a page is the next `before`.
// `item` matches a substring of `<kind>:<name>` ignoring ASCII case; empty strings match everything.
export interface Orphan { machine: string; harness: string; kind: 'skill' | 'rule'; item_id: ItemId; name: string | null }
export interface OrphansView { orphans: Orphan[] }
export interface OrphansPruned { entries: LogEntry[] }
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
// `preset`: built in (machine.toml can only override or disable it); `configured`: machine.toml has an entry;
// `available`: an opt-in preset (added after 0.1.0) that machine.toml doesn't enable, or gemini without its opt-in
// skills directory; enabling it takes that up (`harness` shows what it then syncs into).
export interface HarnessChoice { name: string; installed: boolean; enabled: boolean; harness: HarnessView['harness']; preset?: boolean; configured?: boolean; available?: boolean; setup_warning?: string }
export interface HarnessSettings { path: string; harnesses: HarnessChoice[] }
export type HarnessChange = { op: 'enable' | 'disable' | 'remove'; name: string } | { op: 'set'; name: string; harness: HarnessView['harness'] };
// Apply answers 409 if machine.toml changed while it was written; it never removes what sk1ll placed.
export interface HarnessChangeRequest { change: HarnessChange; fingerprint?: string }
export interface InstructionPreview {
  path: string; format: 'block' | 'rules'; content: string; next_content: string; differs: boolean; diff: string;
}
export interface SyncResult { fingerprint?: string; conflicts: ConflictView[]; actions?: PlacementView[]; completed?: PlacementView[]; changed?: PlacementView; errors?: string[] }
export interface PlacementView {
  harness: string; kind: 'create' | 'update' | 'remove' | 'conflict'; item: 'skill' | 'rule' | 'instructions';
  name: string; dest: string; reason: string | null;
}
// `placed`: what sk1ll placed where the change stops syncing, which stays there (`remove`: managed, `conflict`: edited
// by hand); turning the Harness's items off first lets sync take it out. `sync`: what the sync after the change does.
export interface HarnessPreview {
  fingerprint: string;
  path: string; name: string; before: HarnessView['harness'] | null; after: HarnessView['harness'] | null;
  placed: PlacementView[]; warnings?: string[]; sync: PlacementView[]; awaiting_assignment: boolean;
}
// `errors`: what the sync after the change failed to place; machine.toml changed either way.
export interface HarnessApplied { harnesses: HarnessChoice[]; errors: string[] }
// How the app can start the Daemon itself; not a Daemon type.
export type ServiceManager = 'systemd' | 'launchd';
export interface ServiceInfo { default_socket: boolean; binary: string | null; manager: ServiceManager; available: boolean; active: boolean; restartable: boolean; unit: string | null }
export type DaemonStarted = ServiceManager | 'process';
export interface ImportUpdate { name: string; source: string; ref: string; locked: string; upstream: string }
// `skipped`: Imports the Server's import check settings kept it from checking.
export interface ImportsView { imports?: ImportEntry[]; checked_at: number | null; updates: ImportUpdate[]; errors: { name: string; message: string }[]; skipped?: { name: string; reason: string }[] }
export interface ImportUpdated { name: string; old_commit: string | null; new_commit: string; changed: boolean }
// `admin_token` replaces `code` for the first Machine of a new Fleet.
export interface JoinApiRequest { server_url: string; code: string; admin_token?: string; machine_id?: string; copy_from?: string; repo?: string }
export interface Joined { machine_id: string; repo: string; setup: 'cloned' | 'adopted'; harnesses: string[]; revision: number }
export interface JoinCode { code: string; expires_at: number; scope: Scope }
export interface CommandError { message: string; status: number | null; unreachable: boolean }
export interface DaemonConnection { reachable: boolean; message: string | null; retry_in_ms: number }
export type DaemonEvent = { type: 'status' | 'draft' | 'fleet' } | { type: 'assignment'; revision: number }
  | { type: 'imports'; updates: ImportUpdate[] };

export interface ItemView { id: ItemId | null;
  item: ItemRef; in_baseline: boolean; draft: DraftEntry['change'] | null; unpushed: boolean; description: string | null;
}

export interface SkillCandidate { path: string; default_name: string; description: string | null; taken: boolean; total_bytes?: number; warnings?: { path: string; kinds: ('executable' | 'script' | 'binary' | 'large')[] }[] }
export interface ResolvedSource { source: string; git_ref: string; commit: string; candidates: SkillCandidate[] }
export interface ResolveRequest { link: string; path?: string; ref?: string }
export interface ImportChanges {
  locked: string; upstream: string; truncated: boolean; files: string[];
  commits: { sha: string; date: string; subject: string }[];
}
export interface AddImportRequest { name: string; source: string; path: string; ref: string; commit?: string }

export type ContentType = 'personal' | 'import' | 'rule' | 'unmanaged';
// `resumable`: identical to the existing personal copy, so Adopt resumes an interrupted adoption.
export interface ContentVariant {
  hash: string; description: string | null; locations: { harness: string; path: string }[]; resumable?: boolean;
}
export interface ContentEntry {
  name: string; item: ItemRef | null; type: ContentType; description: string | null;
  draft: DraftEntry['change'] | null; in_baseline: boolean; path: string | null;
  enabled_harnesses: string[]; clashes_with_repo: boolean; variants: ContentVariant[];
}
export interface ContentQuery { item: string; hash?: string; harness?: string }
export interface FileEntry { path: string; kind: 'file' | 'directory' | 'symlink'; size: number }
export type WriteFileRequest = ContentQuery & { path: string; content: string; expected: string };
export interface FileContent { fingerprint: string | null; path: string; text: string | null; binary: boolean; too_large: boolean; size: number }
export interface Editor { id: string; name: string; kind: 'gui' | 'terminal' | 'system' }
export interface EditorsView { editors: Editor[]; preferred_editor: string }

export interface PushResult { pushed: boolean }
export interface CreateItemRequest { kind: 'skill' | 'rule'; name: string; description?: string }
// Personal Skills and Rules only; Imports use removeImport. Both are Draft changes.
export interface DeleteItemRequest { kind: 'skill' | 'rule'; name: string }
export interface RenameItemRequest { kind: 'skill' | 'rule'; name: string; to: string }
export interface RenameItemResponse { item: ItemRef }
export type ConflictDiffRequest = { harness: string; name: string; id?: never } | { id: string; harness?: never; name?: never };
export interface ImportEntry { name: string; source: string; ref: string; path: string; locked: string | null }

export interface MachineAssignmentCopy {
  source: string;
  mode: 'replace' | 'merge';
  harnesses?: { from: string; to: string }[];
  expected?: Expected;
  source_expected?: Expected;
}
export interface AssignmentCopyPreview { count: number; target: Expected; source: Expected }
