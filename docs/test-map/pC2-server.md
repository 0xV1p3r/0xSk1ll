# Phase C2 test map: server + wire protocol

Every test deleted from `crates/sk1ll-proto` and `crates/sk1ll-server` (source of truth: 3cc7ee4).
Tests in `tests/server.rs` that kept their name are not listed. `server.rs`, `wire.rs` and
`config.rs` below are `crates/sk1ll-server/tests/server.rs`, `crates/sk1ll-proto/tests/wire.rs` and
`crates/sk1ll-server/tests/config.rs`.

## sk1ll-proto/src/lib.rs (inline)

- assignment_writes_round_trip -> daemon_messages_have_golden_json, server_messages_have_golden_json (wire.rs)
- item_refs_are_strings -> every_enum_variant_has_its_wire_name, malformed_messages_are_refused (wire.rs)
- reports_accept_every_draft_item_and_carry_ids -> daemon_messages_have_golden_json (wire.rs)
- assignment_snapshots_carry_a_revision -> server_messages_have_golden_json (wire.rs)
- messages_are_tagged -> daemon_messages_have_golden_json, server_messages_have_golden_json (wire.rs)
- scopes_parse -> every_enum_variant_has_its_wire_name, absent_defaulted_fields_and_unknown_fields_are_accepted, malformed_messages_are_refused (wire.rs)
- headline_picks_most_severe -> the_fleet_reports_the_most_severe_current_status (server.rs; now also "heard from, no report: behind"); offline -> silent_connections_go_offline, websocket_upgrade_alone_does_not_refresh_last_seen (server.rs); dropped: `headline(_, None, None)` (no baseline at all), since the server always has one after bootstrap

## sk1ll-server/src/git.rs (inline)

- unconfirmed_cleanup_rejects_queued_and_new_pushes_but_allows_fetch -> unconfirmed_cleanup_refuses_pushes_and_prune_until_restart (server.rs)
- a_push_queued_behind_its_machines_removal_is_refused -> a_push_checks_registration_again_after_waiting_for_removal (server.rs)
- a_removal_waits_for_its_machines_running_push -> a_push_checks_registration_again_after_waiting_for_removal (server.rs)
- a_dropped_removal_keeps_queued_pushes_out_until_it_commits -> dropped: not reachable over HTTP. The window is the removal's database work after its caller disconnects. Stalling that work from outside with an external SQLite write lock makes the removal fail with 500 at once rather than wait, and every other path through the one database mutex orders the push's recheck after the removal anyway. A failpoint would need a new `Point` in sk1ll-core (out of C2's scope). The guard moving into the DB closure stays documented on `Server::remove_machine`.
- replacement_refs_do_not_change_what_the_server_reads -> a_baseline_inventory_ignores_local_replacement_refs, pushes_to_refs_outside_the_baseline_branch_are_refused (server.rs)
- a_stuck_git_read_times_out_and_is_stopped_with_its_children -> a_failed_or_cancelled_baseline_read_stops_its_child_processes, case `timeout` (server.rs)
- a_read_past_its_output_cap_fails_and_is_stopped -> a_failed_or_cancelled_baseline_read_stops_its_child_processes, case `output-cap` (server.rs)
- a_cancelled_read_stops_its_children -> a_failed_or_cancelled_baseline_read_stops_its_child_processes, case `cancel` (server.rs)
- the_hook_checks_each_new_baseline_with_the_server_binary -> pushes_need_an_item_id_of_its_own_on_every_item (server.rs; check binary at a path with `'`, spaces and `$`); dropped: the hook without a check binary, since only tests' `Config::new` leaves `push_check` unset and `Config::from_env` always sets it
- check_push_refuses_items_without_an_item_id_of_their_own -> pushes_need_an_item_id_of_its_own_on_every_item (missing, shared), oversized_rules_keep_their_claim_and_incomplete_baselines_refuse_prune (unreadable), a_push_rejects_filter_drivers_in_the_new_baseline (filter drivers) (server.rs)
- redirects_are_turned_off_only_for_isolated_checks -> allowlisted_import_checks_do_not_follow_redirects (server.rs)
- cleanup_kills_term_resistant_descendant_after_backend_exits -> backend_deadline_kills_pre_receive_descendants (server.rs; the hook now also leaves a TERM-resistant child that holds no pipe)
- annotated_tags_resolve_to_their_commit -> import_updates_resolve_branches_and_tags_to_upstream_commits (server.rs; an unknown ref is now an import error there too)
- isolated_checks_ignore_the_servers_git_and_ssh_config -> allowlisted_imports_ignore_inherited_git_rewrites_and_ssh_configuration (server.rs)
- import_transports_are_restricted -> imports_cannot_use_local_or_external_command_transports_by_default (server.rs; the `ext::` command is checked not to run)

## sk1ll-server/src/ws.rs (inline)

- malformed_report_entries_are_skipped -> valid_report_entries_survive_malformed_neighbors (server.rs)
- malformed_unpushed_entries_are_skipped -> valid_report_entries_survive_malformed_neighbors (server.rs)
- assignments_coalesce_to_the_newest_revision -> stalled_daemon_gets_only_the_latest_assignment, assignment_revisions_arrive_in_order (server.rs)
- revocation_during_a_send_still_delivers_the_revoked_message -> a_daemon_removed_mid_send_still_hears_it_was_revoked (server.rs)
- a_delayed_upgrade_cannot_replace_a_rejoined_machines_session -> dropped: the window between a WebSocket upgrade's authentication and its session registration can't be held open from a client (no body to delay, both steps run inside the server back to back). A failpoint would need a new `Point` in sk1ll-core (out of C2's scope). The HTTP counterpart (a request authenticated before remove + rejoin) is pinned by removing_and_rejoining_revokes_requests_that_already_authenticated, and a revoked session's end by replaced_socket_cannot_report_after_revocation (server.rs)
- session_writes_follow_the_status_before_them -> a_websocket_write_uses_the_status_sent_before_it (server.rs)
- replayed_rename_and_replaced_session_cannot_overwrite_assignment -> renaming_a_harness_moves_toggles_and_replay_leaves_the_destination_intact (replay), a_replaced_sessions_pending_write_is_not_applied (stale session) (server.rs)
- idle_limit_covers_two_pings -> the_server_reads_and_validates_its_environment (config.rs), silent_connections_go_offline (server.rs)

## sk1ll-server/src/api.rs (inline)

- toggles_for_unknown_ids_are_accepted -> pruning_orphans_respects_scope_and_preserves_reported_items (server.rs)
- setting_a_harness_sets_defaults_and_every_toggle -> setting_a_harness_updates_defaults_and_explicit_toggles (server.rs)
- harness_renames_move_toggles_and_log_them -> renaming_a_harness_moves_toggles_and_replay_leaves_the_destination_intact (server.rs)
- orphans_are_pruned_by_scope_and_logged -> pruning_orphans_respects_scope_and_preserves_reported_items (server.rs)
- prune_refuses_degraded_push_and_incomplete_machine_inventory -> unconfirmed_cleanup_refuses_pushes_and_prune_until_restart (degraded), pruning_orphans_respects_scope_and_preserves_reported_items (incomplete) (server.rs)
- writes_recheck_the_callers_token_and_scope -> removing_and_rejoining_revokes_requests_that_already_authenticated (server.rs; join codes, assignment writes, prune); dropped: undo and removal with the stale token (bodyless, so the request can't be paused between authentication and handler), and a caller carrying a new token with the old scope (no request can carry that)
- join_codes_normalize -> join_codes_are_single_use (server.rs)

## sk1ll-server/src/db.rs (inline)

- creates_once_round_trips_log_and_replaces_other_versions -> a_database_reopens_current_data_and_resets_an_old_schema (server.rs); undo and copy entries read back -> assignment_changes_are_logged_and_undoable, copy_machine_assignment_previews_merges_replaces_and_logs (server.rs)
- log_filters_apply_before_the_limit_and_keep_the_cursor -> log_filters_and_facets_cover_the_whole_log (server.rs; now also Item ID, kind, a literal `%` and empty filters)
- revisions_grow_across_machines_and_rejoins -> an_expected_revision_and_registration_guard_a_write, self_scoped_machines_manage_only_themselves (server.rs)
- join_codes_are_single_use_and_expire -> join_codes_are_single_use, expired_join_code_is_invalid (server.rs)

## sk1ll-server/src/lib.rs (inline)

- database_work_runs_off_the_async_workers -> a_blocked_database_write_leaves_health_requests_responsive (server.rs)
- a_stale_head_is_retried_after_push -> a_push_retries_its_head_read_and_nudges_when_the_ref_recovers (server.rs)
- a_push_that_deletes_items_changes_no_assignment -> each_push_is_announced_and_changes_no_assignment (server.rs)

## sk1ll-server/src/imports.rs (inline)

- import_hosts_are_parsed_strictly -> the_import_allowlist_parses_each_sources_host_strictly (server.rs; every case, through the skip reason), an_import_allowlist_skips_ambiguous_and_unlisted_hosts (server.rs; which host ssh contacts)
- settings_decide_what_is_skipped -> import_checks_can_be_turned_off_or_limited_to_hosts, an_import_allowlist_skips_ambiguous_and_unlisted_hosts, import_updates_resolve_branches_and_tags_to_upstream_commits (no allowlist: nothing skipped) (server.rs)

## sk1ll-server/src/config.rs (inline)

- durations -> durations_accept_seconds_minutes_hours_and_days (config.rs)
- switches_and_hosts -> the_server_reads_and_validates_its_environment (config.rs)

## sk1ll-server/tests/server.rs (renamed or merged)

- restarting_server_preserves_initial_commit -> a_database_reopens_current_data_and_resets_an_old_schema, a_restart_mid_push_keeps_assignments_and_serves_the_current_head (server.rs)
- existing_empty_server_gets_initial_commit -> existing_empty_repo_on_another_branch_takes_pushes_to_main (server.rs)
- offline_clones_share_a_root_and_can_rebase_then_push -> competing_pushes_land_one_at_a_time_and_losers_rebase_onto_the_shared_root (server.rs)
- imports_with_newer_upstream_are_reported -> import_updates_resolve_branches_and_tags_to_upstream_commits (server.rs)
- encoded_userinfo_cannot_reach_an_unlisted_host -> an_import_allowlist_skips_ambiguous_and_unlisted_hosts (server.rs)
- reports_keep_every_kind_of_draft_item -> valid_report_entries_survive_malformed_neighbors (server.rs)

## New simulation tests (no predecessor)

- concurrent_assignment_writes_commit_once_per_revision: four writers per round in a seeded polling order
- competing_pushes_land_one_at_a_time_and_losers_rebase_onto_the_shared_root: three machines push at once until all land
- a_restart_mid_push_keeps_assignments_and_serves_the_current_head: stop with the push lock held and an unannounced HEAD
- unconfirmed_cleanup_refuses_pushes_and_prune_until_restart: the `ServerCleanupConfirmation` failpoint, then restart

## Coverage

`cargo llvm-cov -p sk1ll-server -p sk1ll-proto --all-features`, lines, `#[cfg(test)]` lines excluded
on both sides (before: 3cc7ee4). Every file is level or up except `git.rs`, 94.78% -> 94.40% (2 lines):
the refusal in `check_push` and its empty-tree arm now run only inside `sk1ll-server --check-push`, which
the pre-receive hook starts; its profile is not collected (most likely the hook environment drops `LLVM_PROFILE_FILE`). Pushes refused by it
are asserted in `pushes_need_an_item_id_of_its_own_on_every_item` and `a_push_rejects_filter_drivers_in_the_new_baseline`.

| file | before | after |
|---|---|---|
| sk1ll-proto/src/lib.rs | 100.00% | 100.00% |
| sk1ll-server/src/api.rs | 97.74% | 97.74% |
| sk1ll-server/src/assign.rs | 95.89% | 96.16% |
| sk1ll-server/src/config.rs | 65.71% | 100.00% |
| sk1ll-server/src/db.rs | 97.25% | 97.25% |
| sk1ll-server/src/error.rs | 84.21% | 84.21% |
| sk1ll-server/src/git.rs | 94.78% | 94.40% |
| sk1ll-server/src/imports.rs | 94.77% | 95.42% |
| sk1ll-server/src/lib.rs | 93.49% | 93.49% |
| sk1ll-server/src/main.rs | 17.50% | 17.50% |
| sk1ll-server/src/ws.rs | 88.63% | 88.63% |
| total | 92.47% | 93.91% |
