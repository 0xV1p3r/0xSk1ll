// The contract between the Rust API types and `types.ts`, checked by `pnpm tsc --noEmit`; nothing imports it.
// The golden files are written by crates/sk1ll-gui/src-tauri/tests/api_contract.rs, which fails when the Rust
// types serialize differently; the wire bodies the Daemon passes through from the Server come from
// crates/sk1ll-proto/tests/wire.json, which crates/sk1ll-proto/tests/wire.rs checks the same way. Here each sample must fit its `types.ts` type, so tsc fails when this side drifts:
// a field missing, renamed or of another type, a field types.ts doesn't know, or an enum variant added or removed.
import app from '../../contract/app.json';
import content from '../../contract/content.json';
import draft from '../../contract/draft.json';
import fleet from '../../contract/fleet.json';
import harness from '../../contract/harness.json';
import imports from '../../contract/imports.json';
import status from '../../contract/status.json';
import wire from '../../../sk1ll-proto/tests/wire.json';
import type * as T from './types';

// A JSON import types every string as `string`, so samples are compared with literal unions widened. The variant
// sets are compared exactly instead: the golden files key each enum's samples by variant name.
type Widen<V> = V extends string ? string : V extends number ? number : V extends boolean ? boolean
  : V extends readonly (infer E)[] ? Widen<E>[] : V extends object ? { [K in keyof V]: Widen<V[K]> } : V;
type Keys<V> = V extends unknown ? keyof V : never;
type At<V, K> = V extends unknown ? K extends keyof V ? V[K] : never : never;
type Element<V> = V extends readonly (infer E)[] ? E : never;
// The fields of `J` (a sample) that `V` doesn't declare, at any depth.
type Unknown<J, V> = J extends readonly (infer E)[] ? Unknown<E, Element<NonNullable<V>>>
  : J extends object ? { [K in keyof J]-?: K extends Keys<NonNullable<V>> ? Unknown<J[K], At<NonNullable<V>, K>> : K }[keyof J]
  : never;
type Known<J, V> = [Unknown<J, V>] extends [never] ? unknown : { unknownFields: Unknown<J, V> };
type Differ<A, B> = Exclude<A, B> | Exclude<B, A>;
type Same<J, U> = [Differ<keyof J, U>] extends [never] ? unknown : { variantsDiffer: Differ<keyof J, U> };
// The member of `V` whose `tag` is `K`, or `V` itself when the tag is an ordinary field.
type Member<V, Tag extends PropertyKey, K> = [Extract<V, Record<Tag, K>>] extends [never] ? V : Extract<V, Record<Tag, K>>;
type KnownEach<J, V, Tag extends PropertyKey> = [{ [K in keyof J]: Unknown<J[K], Member<V, Tag, K>> }[keyof J]] extends [never] ? unknown
  : { unknownFields: { [K in keyof J]: Unknown<J[K], Member<V, Tag, K>> }[keyof J] };

// `json` is a `V`.
const value = <V>() => <J extends Widen<V>>(json: J & Known<J, V>) => json;
// An enum written as `{ variant: variant }`: exactly the variants of `U`.
const names = <U extends string>() => <J extends { [K in U]: string }>(json: J & Same<J, U>) => json;
// Samples keyed by their `tag`: one for each value `V[tag]` can take, each a `V` (its member of `V`, for a union).
const tagged = <V, Tag extends keyof V & string>() =>
  <J extends { [K in V[Tag] & string]: Widen<Member<V, Tag, K>> }>(json: J & Same<J, V[Tag]> & KnownEach<J, V, Tag>) => json;

export const contract = [
  value<{ joined: T.StatusView; standalone: T.StatusView }>()(status.status_view),
  names<T.Connection>()(status.connection),
  names<T.MachineStatus>()(status.machine_status),
  names<T.StatusView['mode']>()(status.daemon_mode),
  tagged<T.DaemonEvent, 'type'>()(status.event),
  value<T.DaemonConnection[]>()(status.daemon_connection),
  value<T.CommandError[]>()(status.command_error),

  value<T.FleetView>()(wire.fleet_view),
  names<T.Scope>()(fleet.scope),
  value<T.AssignmentSnapshot>()(fleet.assignment_snapshot),
  tagged<T.ToggleChange, 'kind'>()(fleet.toggle_kind),
  value<T.AssignmentChanges>()(wire.assignment_changes),
  value<T.AssignmentUpdate>()(wire.assignment_update),
  tagged<T.LogCause, 'type'>()(fleet.log_cause),
  tagged<T.LogEntry, 'kind'>()(fleet.log_entry),
  value<T.LogQuery>()(fleet.log_query),
  value<T.LogFacets>()(wire.log_facets),
  value<T.OrphansView>()(wire.orphans_view),
  value<T.OrphansPruned>()(wire.orphans_pruned),
  tagged<T.MachineAssignmentCopy, 'mode'>()(fleet.copy_mode),
  value<T.AssignmentCopyPreview>()(wire.assignment_copy_preview),
  value<{ code: T.JoinApiRequest; admin: T.JoinApiRequest }>()(fleet.join_api_request),
  tagged<T.Joined, 'setup'>()(fleet.joined),
  value<T.JoinCode>()(wire.join_code),

  value<T.ItemView[]>()(draft.item_view),
  tagged<T.DraftEntry, 'change'>()(draft.draft_entry),
  value<T.DiffView[]>()(draft.diff_view),
  value<T.CommitMessage>()(draft.commit_message),
  value<T.CommitRequest>()(draft.commit_request),
  value<T.CommitResponse>()(draft.commit_response),
  value<T.RestoreRequest>()(draft.restore_request),
  value<T.RestoreResponse>()(draft.restore_response),
  tagged<T.PullResult, 'state'>()(draft.pull_result),
  value<T.PullKeepRequest>()(draft.pull_keep_request),
  value<T.PullKeepResult>()(draft.pull_keep_result),
  value<T.PushResult>()(draft.push_result),

  value<{ harness: T.ConflictView; displaced: T.ConflictView }>()(harness.conflict_view),
  value<T.ConflictTarget>()(harness.conflict_target),
  value<T.HarnessView>()(harness.harness_view),
  tagged<T.HarnessView['harness'], 'instructions_format'>()(harness.instructions_format),
  tagged<T.HarnessView['harness'], 'mode'>()(harness.link_mode),
  value<T.HarnessSettings>()(harness.harness_settings),
  tagged<T.HarnessChange, 'op'>()(harness.harness_change),
  value<T.HarnessChangeRequest>()(harness.harness_change_request),
  tagged<T.PlacementView, 'kind'>()(harness.placement_kind),
  tagged<T.PlacementView, 'item'>()(harness.placement_item),
  value<T.HarnessPreview>()(harness.harness_preview),
  value<T.HarnessApplied>()(harness.harness_applied),
  value<T.HarnessTakenOut>()(harness.harness_taken_out),
  value<T.SyncResult[]>()(harness.sync_result),
  tagged<T.InstructionPreview, 'format'>()(harness.instruction_preview),

  tagged<T.ContentEntry, 'type'>()(content.content_type),
  value<T.ContentQuery>()(content.content_query),
  value<T.ContentQuery & { path: string }>()(content.file_query),
  tagged<T.FileEntry, 'kind'>()(content.file_kind),
  value<T.FileContent[]>()(content.file_content),
  value<T.WriteFileRequest>()(content.write_file_request),
  value<T.CreateItemRequest>()(content.create_item_request),
  value<T.DeleteItemRequest>()(content.delete_item_request),
  value<T.RenameItemRequest>()(content.rename_item_request),
  value<T.RenameItemResponse>()(content.rename_item_response),

  value<T.ImportsView>()(imports.imports_view),
  value<T.ImportUpdated>()(imports.import_updated),
  value<T.ResolveRequest>()(imports.resolve_request),
  value<T.ResolvedSource>()(imports.resolved_source),
  names<NonNullable<T.SkillCandidate['warnings']>[number]['kinds'][number]>()(imports.warning_kind),
  value<T.AddImportRequest>()(imports.add_import_request),
  value<T.ImportChanges>()(imports.import_changes),

  value<T.EditorsView>()(app.editors_view),
  tagged<T.Editor, 'kind'>()(app.editor_kind),
  value<T.ServiceInfo>()(app.service_info),
  names<T.ServiceManager>()(app.service_manager),
  names<T.DaemonStarted>()(app.daemon_started),
  value<T.AvailableUpdate[]>()(app.available_update),
  tagged<T.UpdateProgress, 'stage'>()(app.update_progress),
];
