# Phase C1 (sk1ll-daemon): where each deleted test went

Before: 3cc7ee4. New homes, all in `crates/sk1ll-daemon/tests/`:
`engine/main.rs` and `engine/races.rs` (the in-process engine through
`sk1ll_daemon::local`, failpoint scenarios, `testkit::child` crashes),
`daemon.rs` (daemon processes and a real or hand-driven server),
`contracts/*.rs` (local API requests, files on disk, service files, git's
environment). `src/error.rs` keeps its inline test: it pins the local API's
error-to-status contract on a private type.

## src/daemon/tests.rs (deleted; 49 tests)

- inventory_keeps_both_ids_when_a_draft_recreates_the_same_name -> inventory_reads_the_applied_baseline_and_leaves_the_index_alone (engine/main.rs)
- inventory_reads_applied_baseline_and_never_refreshes_the_index -> inventory_reads_the_applied_baseline_and_leaves_the_index_alone (engine/main.rs)
- only_small_blobs_are_fetched -> inventory_reads_the_applied_baseline_and_leaves_the_index_alone (engine/main.rs; the 17 KiB skill has no description)
- batch_output_is_read_by_size -> inventory_reads_the_applied_baseline_and_leaves_the_index_alone (engine/main.rs; a body starting with NUL and newline)
- plan_hashes_each_copy_source_once_per_computation -> dropped: a private helper's call count (performance detail); hashing is bounded end to end by a_full_reconcile_rehashes_the_same_files_after_the_interval (engine/main.rs)
- reviewed_sync_reports_partial_progress_and_stops_on_digest_mismatch -> reviewed_sync_reports_partial_progress_and_stops_on_digest_mismatch (engine/races.rs, DaemonAfterSyncReview)
- stale_revisions_are_ignored -> stale_assignment_revision_is_ignored (daemon.rs)
- only_rejected_pushes_are_retried -> offline_commits_are_queued_and_pushed_on_reconnect, a_push_the_server_refuses_for_good_is_reported_until_fixed (daemon.rs)
- refused_adoption_rolls_back_only_its_own_unchanged_copy_and_can_retry -> adoption_preserves_every_copy_it_no_longer_owns_when_confirmation_arrives (daemon.rs, "unchanged")
- a_slow_assignment_confirmation_blocks_no_other_call -> a_slow_assignment_reply_leaves_the_engine_free_and_cannot_replace_a_newer_revision (daemon.rs)
- a_confirmation_overtaken_by_a_newer_revision_is_not_applied -> a_slow_assignment_reply_leaves_the_engine_free_and_cannot_replace_a_newer_revision (daemon.rs)
- a_restart_remembers_the_block_and_the_server_baseline -> blocked_pull_reports_clashes_and_keeps_files (daemon.rs, restart while blocked), a_restart_clears_a_finished_pull_and_keeps_a_newer_baseline_pending (engine/main.rs), revoked_machine_stops_managing_and_keeps_files (daemon.rs)
- a_halted_daemon_leaves_the_state_file_alone -> dropped: drives `halt_with`/`revoke` on a private engine; the public surface can't revoke a halted engine. The reachable cases are pinned by recovering_a_connection_during_registration_keeps_the_old_registration_active and a_slow_assignment_reply_leaves_the_engine_free_and_cannot_replace_a_newer_revision (revocation with a reply in flight; daemon.rs) and interrupted_join_activation_keeps_credentials_and_toggles_from_the_same_registration (engine/races.rs)
- a_reconcile_runs_once_per_input_change_and_rehashes_only_changed_trees -> reconcile_follows_symlinked_config_and_distinguishes_skill_content_from_staging (engine/main.rs, PlanSource count), connecting_costs_no_reconcile_when_nothing_changed (daemon.rs)
- a_failed_reconcile_is_tried_again -> a_disappearing_repo_clears_the_daemons_published_inventory (daemon.rs, invalid machine.toml twice, then recovered)
- readers_answer_from_the_last_snapshot_while_the_engine_is_busy -> readers_see_the_published_snapshot_while_fresh_reads_wait_for_reconcile (engine/races.rs)
- a_refused_adoption_keeps_a_skill_committed_or_reidentified_meanwhile -> adoption_preserves_every_copy_it_no_longer_owns_when_confirmation_arrives (daemon.rs, "committed", "reidentified", "repo-edited", "source-edited")
- an_adoption_confirmed_during_a_pending_pull_leaves_the_harness_alone -> adoption_preserves_every_copy_it_no_longer_owns_when_confirmation_arrives (daemon.rs, "pending-pull")
- a_stale_rename_confirmation_leaves_the_next_registrations_snapshot_alone -> dropped: needs a private World token and a forced revoke-then-rejoin between a rename's request and its reply. The World check is pinned through the public surface by a_slow_assignment_reply_leaves_the_engine_free_and_cannot_replace_a_newer_revision and recovering_a_connection_during_registration_keeps_the_old_registration_active (daemon.rs); the rename's own replay by reconnect_replays_harness_rename_before_first_status (daemon.rs)
- a_finished_pull_recorded_as_pending_is_clear_after_a_restart -> a_restart_clears_a_finished_pull_and_keeps_a_newer_baseline_pending (engine/main.rs)
- a_daemon_starts_over_an_unreadable_pull_record -> a_restart_clears_a_finished_pull_and_keeps_a_newer_baseline_pending (engine/main.rs, the old record shape), an_unreadable_pull_record_is_dropped_not_fatal (contracts/state.rs)
- a_rejoin_does_not_replace_a_registration_that_recovered_meanwhile -> recovering_a_connection_during_registration_keeps_the_old_registration_active (daemon.rs)
- a_rejoin_checks_and_halts_in_one_step -> dropped: the race is a connection coming back inside the rejoin's own critical section (DaemonBeforeRejoinPersist), which needs a hand-driven session link the in-process engine doesn't run and a daemon process can't be hooked into. The wider race (the connection recovers before the check) is pinned by recovering_a_connection_during_registration_keeps_the_old_registration_active (daemon.rs); a crash at that point by interrupted_join_activation_keeps_credentials_and_toggles_from_the_same_registration (engine/races.rs)
- a_symlinked_machine_config_is_read_through_the_link -> reconcile_follows_symlinked_config_and_distinguishes_skill_content_from_staging (engine/main.rs)
- a_skill_file_named_like_a_stage_is_content -> reconcile_follows_symlinked_config_and_distinguishes_skill_content_from_staging (engine/main.rs)
- a_harness_changed_within_the_racy_window_is_reconciled_again -> a_harness_edit_within_the_racy_window_is_reconciled_again (engine/main.rs)
- a_git_pointer_is_followed_to_the_git_and_common_dirs -> a_commit_behind_a_git_pointer_is_noticed_without_a_change_to_the_tree (engine/main.rs), a_disappearing_repo_clears_the_daemons_published_inventory (daemon.rs)
- a_missing_standalone_repo_replaces_the_snapshot -> a_disappearing_repo_clears_the_daemons_published_inventory (daemon.rs, "work tree")
- a_repo_whose_git_dir_is_gone_replaces_the_snapshot -> a_disappearing_repo_clears_the_daemons_published_inventory (daemon.rs, "git directory", "invalid pointer")
- a_full_reconcile_corrects_what_the_fingerprint_missed -> a_full_reconcile_rehashes_the_same_files_after_the_interval (engine/main.rs)
- a_conflict_diff_while_the_engine_is_busy_waits_for_a_fresh_snapshot -> readers_see_the_published_snapshot_while_fresh_reads_wait_for_reconcile (engine/races.rs)
- an_on_demand_engine_connects_reports_and_stores_its_write -> an_on_demand_engine_reports_its_inventory_and_protects_server_only_assignment (engine/main.rs)
- an_on_demand_rename_protects_a_destination_only_the_server_has -> an_on_demand_engine_reports_its_inventory_and_protects_server_only_assignment (engine/main.rs)
- a_starting_daemon_waits_for_a_commands_toggle_and_starts_from_it -> a_starting_daemon_waits_for_an_on_demand_engine_and_loads_its_revision (engine/main.rs)
- a_toggle_racing_a_hand_edit_keeps_both -> assignment_store_retries_on_a_newer_hand_edit_and_keeps_the_record_in_step (engine/races.rs, "toggle", DaemonStoreBeforeSwap)
- reading_the_assignment_leaves_a_pending_rename_alone -> previews_leave_a_pending_rename_for_the_writer_even_through_a_second_config_link (engine/main.rs)
- a_rename_stopped_under_one_name_is_finished_through_another -> previews_leave_a_pending_rename_for_the_writer_even_through_a_second_config_link (engine/main.rs)
- previews_through_the_daemon_never_reconcile -> previews_leave_a_pending_rename_for_the_writer_even_through_a_second_config_link (engine/main.rs)
- taking_up_a_hand_edit_never_replaces_a_newer_one -> assignment_store_retries_on_a_newer_hand_edit_and_keeps_the_record_in_step (engine/races.rs, "startup", "refresh")
- a_starting_daemon_writes_back_nothing_it_loaded -> assignment_store_retries_on_a_newer_hand_edit_and_keeps_the_record_in_step (engine/races.rs, "startup without edits")
- take_out_disables_a_harness_only_once_its_placements_are_gone -> take_out_only_disables_a_harness_after_a_complete_scan_and_preserves_concurrent_edits (engine/races.rs, "removal fails": a removal failed at RecoveryRename leaves the index readable, `left` names the copy, machine.toml stays on, the items stay off, and the same reviewed request succeeds once the fault clears; "clean")
- a_blocked_reconcile_without_a_keep_leaves_the_draft_alone -> blocked_pull_reports_clashes_and_keeps_files (daemon.rs, the clashing draft survives reconciles and a restart while blocked)
- take_out_needs_a_complete_scan_before_it_changes_machine_toml -> take_out_only_disables_a_harness_after_a_complete_scan_and_preserves_concurrent_edits (engine/races.rs, "first scan")
- take_out_reports_what_it_did_when_machine_toml_changed_meanwhile -> take_out_only_disables_a_harness_after_a_complete_scan_and_preserves_concurrent_edits (engine/races.rs, "config edit")
- harness_changes_wait_for_a_take_out -> applying_a_harness_change_waits_for_take_out_then_rechecks_its_preview (engine/races.rs)
- take_out_writes_only_from_a_complete_scan_of_its_own -> take_out_only_disables_a_harness_after_a_complete_scan_and_preserves_concurrent_edits (engine/races.rs, "second scan", "stale preview")
- take_out_reports_what_it_did_when_the_harness_went_meanwhile -> take_out_only_disables_a_harness_after_a_complete_scan_and_preserves_concurrent_edits (engine/races.rs, "removed by hand")
- take_out_reports_a_harness_turned_off_by_hand_meanwhile -> take_out_only_disables_a_harness_after_a_complete_scan_and_preserves_concurrent_edits (engine/races.rs, "disabled by hand")
- a_reconcile_reports_an_interrupted_keep -> reconcile_publishes_and_dismisses_the_note_from_an_interrupted_keep (engine/main.rs)

## Inline test modules (deleted)

- api::the_socket_dir_must_be_private -> the_socket_requires_a_private_directory_and_refuses_overlong_paths (daemon.rs)
- api::a_socket_path_too_long_for_sun_path_is_refused_before_creating_its_dir -> the_socket_requires_a_private_directory_and_refuses_overlong_paths (daemon.rs)
- api::our_own_connections_are_accepted -> dropped: every daemon.rs test connects as the same user; the peer-credential check runs on each accept
- auth::credentials_are_an_extra_header_for_the_server -> git_gets_the_token_as_a_header_for_the_server_beside_the_users_own_config (contracts/auth.rs)
- auth::existing_env_config_is_kept -> git_gets_the_token_as_a_header_for_the_server_beside_the_users_own_config (contracts/auth.rs, a child process with the user's GIT_CONFIG_* entries)
- client::encodes_query_values -> previews_leave_a_pending_rename_for_the_writer_even_through_a_second_config_link (engine/main.rs, `skill:new`), the_local_api_routes_requests_and_rejects_incompatible_clients (daemon.rs, `missing skill`)
- client::events_are_tagged -> dropped: response JSON the GUI reads is phase E's golden samples (crates/sk1ll-gui/contract)
- client::resolved_sources_use_plain_field_names -> an_import_request_tells_an_absent_path_from_the_repo_root (contracts/api.rs, the request half); the response half is phase E's golden samples
- client::response_errors_keep_the_message_or_status -> the_local_api_routes_requests_and_rejects_incompatible_clients (daemon.rs) and every test asserting a ClientError's message, e.g. an_on_demand_engine_reports_its_inventory_and_protects_server_only_assignment (engine/main.rs)
- client::harness_changes_are_tagged_by_op -> harness_changes_are_tagged_by_op (contracts/api.rs)
- client::add_requests_tell_an_absent_path_from_the_root -> an_import_request_tells_an_absent_path_from_the_repo_root (contracts/api.rs)
- config::macos_default_socket_fits_sun_path -> dropped: arithmetic on a string literal, no code under test; overlong socket paths are refused by the_socket_requires_a_private_directory_and_refuses_overlong_paths (daemon.rs)
- config::durations -> daemon_intervals_accept_documented_units_and_require_a_positive_value (contracts/config.rs)
- join::replacement_is_locked_before_publication_and_original_stays_locked_after_rollback -> a_credentials_collision_restores_the_original_and_saves_edits_to_the_replacement (engine/races.rs, engine locks checked at DaemonJoinBeforeCredentials and after the rollback)
- join::credentials_collision_restores_snapshot_and_assignment_without_replacing_obstruction -> a_credentials_collision_restores_the_original_and_saves_edits_to_the_replacement (engine/races.rs)
- join::a_rejoin_stopped_half_way_never_pairs_new_credentials_with_the_old_snapshot -> interrupted_join_activation_keeps_credentials_and_toggles_from_the_same_registration (engine/races.rs; process exit at DaemonBeforeRejoinPersist, DaemonJoinAfterSnapshot, DaemonJoinBeforeCredentials)
- join::stalled_join_times_out -> dropped: the join request's timeout is a fixed 30 s reqwest setting, and an end-to-end test of it would cost 30 s
- join::http_loopback_is_exempt -> only_plain_http_to_another_host_sends_the_token_in_clear_text (contracts/auth.rs)
- lifecycle::connection_transitions_only_while_joined -> a_401_is_not_a_revocation (daemon.rs, auth_rejected_since set by a 401, cleared by a connection), standalone_starts_without_a_repo_and_init_creates_it (daemon.rs)
- lifecycle::a_halted_daemon_stays_halted -> recovering_a_connection_during_registration_keeps_the_old_registration_active (daemon.rs), joining_again_while_the_daemon_runs_resumes_management (daemon.rs)
- lifecycle::the_view_is_derived_from_the_pull_state -> blocked_pull_reports_clashes_and_keeps_files, a_refresh_finishes_a_pull_that_stopped_half_way, revoked_machine_stops_managing_and_keeps_files (daemon.rs)
- machine_lock::one_holder_per_machine_whatever_the_repo -> one_holder_per_machine_whatever_the_repo (contracts/machine_lock.rs)
- machine_lock::a_refused_attempt_keeps_nothing -> a_refused_attempt_keeps_nothing (contracts/machine_lock.rs)
- machine_lock::a_lock_is_keyed_on_the_file_not_its_name -> a_lock_is_keyed_on_the_file_not_its_name (contracts/machine_lock.rs)
- machine_lock::the_lock_files_are_in_one_directory_of_sk1lls_own -> the_lock_files_are_in_one_directory_of_sk1lls_own (contracts/machine_lock.rs)
- notifications::conflict_count_growth_bypasses_cooldown_and_recurrence_waits -> desktop_notifications_deliver_conflicts_and_remember_retries_across_restarts (daemon.rs, a second conflict is delivered at once)
- notifications::new_import_failure_is_reported_after_cooldown_even_at_same_count -> dropped: cooldown arithmetic for import alerts, the same scheduler the conflict flow in desktop_notifications_deliver_conflicts_and_remember_retries_across_restarts drives
- notifications::unchanged_alert_survives_restart -> desktop_notifications_deliver_conflicts_and_remember_retries_across_restarts (daemon.rs, restart with the history file, 0600)
- notifications::unchanged_conflict_retries_after_delivery_failure -> desktop_notifications_deliver_conflicts_and_remember_retries_across_restarts (daemon.rs)
- notifications::failed_notification_schedule_survives_restart_and_success_resets_it -> desktop_notifications_deliver_conflicts_and_remember_retries_across_restarts (daemon.rs)
- notifications::escapes_applescript_string_literals -> dropped: macOS-only delivery (osascript), which no Linux test can run and main no longer supports (04272a0)
- service::unit_points_at_the_binary -> unit_points_at_the_binary (contracts/service.rs)
- service::agent_runs_the_binary_and_logs_under_home -> agent_runs_the_binary_and_logs_under_home (contracts/service.rs)
- service::agent_path_prepends_the_git_directory_and_escapes_it_in_xml -> moving_a_daemon_reloads_the_generated_agent_and_preserves_an_edited_plist (contracts/service.rs, macOS only)
- service::launchd_reloads_moved_agents_and_preserves_edited_plists -> moving_a_daemon_reloads_the_generated_agent_and_preserves_an_edited_plist (contracts/service.rs, macOS only)
- service::launchctl_targets_the_users_gui_domain -> moving_a_daemon_reloads_the_generated_agent_and_preserves_an_edited_plist (contracts/service.rs, macOS only)
- service::agent_status_reads_the_services_own_fields -> moving_a_daemon_reloads_the_generated_agent_and_preserves_an_edited_plist (contracts/service.rs, macOS only)
- service::binary_is_found_next_to_the_executable_then_on_path_then_in_the_user_dirs -> binary_is_found_next_to_the_executable_then_on_path_then_in_the_user_dirs (contracts/service.rs)
- state::private_write_ignores_staging_symlinks -> private_write_ignores_staging_symlinks (contracts/state.rs)
- state::an_unreadable_pull_record_is_dropped_not_fatal -> an_unreadable_pull_record_is_dropped_not_fatal (contracts/state.rs)
- state::urls -> the_server_url_names_the_git_and_websocket_endpoints (contracts/state.rs; core's credential stripping is core's to test)
- state::a_dangling_snapshot_link_is_an_error_not_a_missing_snapshot -> a_dangling_snapshot_link_is_an_error_not_a_missing_snapshot (contracts/state.rs)
- state::debug_hides_the_token -> debug_hides_the_token (contracts/state.rs)
- state::state_is_private -> state_is_private (contracts/state.rs)
- state::the_baseline_record_survives_a_save -> the_baseline_record_survives_a_save (contracts/state.rs)
- state::a_standalone_record_is_not_restored_for_another_repo -> a_standalone_record_is_not_restored_for_another_repo (contracts/state.rs)
- state::snapshots_keep_revision_and_content_together -> snapshots_keep_revision_and_content_together (contracts/state.rs)
- watch::only_ref_changes_count_inside_git -> dropped as a filter detail: that a commit wakes the daemon and a repo edit does is pinned by repo_edits_reach_the_harness_before_and_after_the_first_connection and join_commit_push_and_nudge_another_machine (daemon.rs); what is filtered out only saves wake-ups
- watch::targets_change_when_a_missing_dir_appears -> replacing_a_watched_skills_directory_rearms_the_watch (daemon.rs)
- watch::missing_paths_are_watched_through_an_ancestor -> repo_edits_reach_the_harness_before_and_after_the_first_connection (daemon.rs, the repo appears after the daemon starts), standalone_starts_without_a_repo_and_init_creates_it (daemon.rs)
- error::displaced_is_a_conflict_with_recovery_path -> kept inline (src/error.rs): the local API's error-to-status contract

## tests/daemon.rs (tests removed or merged)

- harness_assignment_passes_through_to_the_fleet_and_reaches_sync -> machine_harness_assignment_writes_this_or_another_machine (daemon.rs)
- machine_harness_assignment_of_another_machine_is_refused_to_self_scope -> machine_harness_assignment_writes_this_or_another_machine (daemon.rs)
- standalone_harness_assignment_reaches_items_with_spaces -> standalone_assignment_tracks_item_ids_through_adopt_and_recreate (engine/main.rs, `old name`)
- a_keep_report_stays_in_the_status_until_its_version_is_acted_on -> blocked_pull_reports_clashes_and_keeps_files (daemon.rs)
- repo_edits_reach_the_harness_without_another_trigger -> repo_edits_reach_the_harness_before_and_after_the_first_connection (daemon.rs)
- repo_edits_before_the_first_server_connection_reach_the_harness -> repo_edits_reach_the_harness_before_and_after_the_first_connection (daemon.rs)
- standalone_adopts_unmanaged_skills_with_local_assignment -> standalone_assignment_tracks_item_ids_through_adopt_and_recreate (engine/main.rs)
- standalone_init_refuses_existing_user_content -> standalone_starts_without_a_repo_and_init_creates_it (daemon.rs)
- clients_of_another_local_api_version_are_told_to_restart_the_daemon -> the_local_api_routes_requests_and_rejects_incompatible_clients (daemon.rs)
- a_bundled_skill_is_created_as_a_personal_draft_skill -> a_bundled_skill_is_created_as_a_personal_draft_skill (engine/main.rs)
- reviewed_sync_replans_after_recovering_an_interrupted_import -> reviewed_sync_replans_after_recovering_an_interrupted_import (engine/main.rs)
- recreated_personal_skill_gets_a_new_assignment_identity -> standalone_assignment_tracks_item_ids_through_adopt_and_recreate (engine/main.rs)

## tests/testkit.rs (deleted)

- a_testkit_daemon_joins_a_testkit_server_and_reconnects_after_its_restart -> blocked_pull_reports_clashes_and_keeps_files (daemon.rs, server stop and restart, reconnect), and every daemon.rs test runs the same fixtures

## cfg(test) helpers deleted with them

`Config::new`'s temporary test home (`config::test_home`), `Registered::for_test`,
`Daemon::new`/`Daemon::standalone` and the `plans` counter, the test-build
hash cache with no racy window (the window is now a constructor argument),
`server::jitter`'s self-check, and notifications' `cfg(any(test, macos))`
AppleScript escaping (now macOS only). `harnesses.rs`'s take-out failpoint is
`cfg(feature = "failpoints")`.

## Production items made public

- `sk1ll_daemon::local::Engine::for_tests(cfg, writer, racy_window)`, behind the
  `failpoints` feature: an engine without `prepare_process` (whose process-wide
  environment parallel tests can't share), with a chosen racy window.

## Numbers (3cc7ee4 -> this branch)

- Tests in sk1ll-daemon: 167 (100 inline, 66 daemon.rs, 1 testkit.rs) -> 105
  (1 inline, 62 daemon.rs, 21 engine, 21 contracts). Test code: 6308 -> 4894 lines.
- Wall time, alternating runs on one machine (load 10-15): tests/daemon.rs
  16.7-17.2 s -> 16.0-16.4 s; `cargo test -p sk1ll-daemon --all-features`
  23.5-27.5 s -> 24.9-27.0 s (the engine binary is ~3.5 s: a 3 s full-reconcile
  interval and a 1 s racy window, both sized for a loaded machine).
  daemon.rs is bound by the daemon itself: within the 2 s racy window after a
  change it reconciles every ~150 ms (each reconcile's writes wake its own
  watch), and callers that need the engine (a commit) queue behind that,
  ~1.7 s per commit in `fleet_of_two`. Fixing that is production work.
- Line coverage of `sk1ll-daemon/src` (`cargo llvm-cov --workspace
  --all-features --lcov`, `#[cfg(test)]` lines excluded on both sides): the
  fixtures stop daemon processes with SIGKILL, so a process's own coverage is
  never written, and tests moved into a daemon process look like lost
  coverage (notifications 20.8% -> 0%, watch 20.7% -> 2.2%). Measured with
  daemons stopped by SIGTERM on both sides (a local patch, not committed):
  87.7% -> 86.5% overall. Per file, the drops: service.rs 47.1% -> 18.8%
  (launchd code, tested on macOS only now; main deletes it in 04272a0),
  machine_lock.rs 90.0% -> 78.6% and lifecycle.rs 99.2% -> 90.8% (the waits and
  the rejoin halt run in daemon processes that exit by `kill` or restart),
  notifications.rs 89.9% -> 86.8%, assignment.rs 92.9% -> 90.5%, join.rs
  80.1% -> 77.8% (error paths, the stalled-join timeout); membership.rs
  62.5% -> 78.7%, pull.rs 88.3% -> 91.7%; every other file is within 1 point or higher.
