# Phase D test map

The CLI entries use the built `sk1ll` binary. The desktop entries invoke the app’s registered IPC commands or exercise its event subscription over a Unix socket.

## Counts and coverage

Scope: inline tests in `crates/sk1ll-cli/src`, `crates/sk1ll-cli/tests`, `crates/sk1ll-gui/src-tauri` (inline and `tests/`) and `crates/sk1ll-gui/tests/test_dev_fleet.py`.

| | before (3cc7ee4) | after |
|---|---|---|
| tests (Rust + Python) | 111 (106 + 5) | 66 (62 + 4) |
| test-code lines | 3,485 | 2,778 |
| test execution, `cargo test -p sk1ll-cli -p sk1ll-gui --all-features` (sum of test binaries, load avg ~30) | 15.6 s | 19.3 s |
| `test_dev_fleet.py` | - | 28.5 s, 4 tests |

The CLI fleet tests now run against a real `testkit::server` and the CLI's own `daemon` command instead of an HTTP stub, which is most of the added runtime.

Line coverage (`cargo llvm-cov -p sk1ll-cli -p sk1ll-gui --all-features --summary-only`; baseline from `coverage-baseline.txt`). Baseline line totals include the inline test modules, which counted as covered lines.

| file | baseline lines (missed) | after lines (missed) |
|---|---|---|
| sk1ll-cli/src/fleet.rs | 77.08% (88) | 84.10% (59) |
| sk1ll-cli/src/harnesses.rs | 89.78% (28) | 87.68% (26) |
| sk1ll-cli/src/main.rs | 81.52% (158) | 87.07% (112) |
| sk1ll-cli/src/release_check.rs | 47.54% (32) | 46.38% (37) |
| sk1ll-cli/src/setup.rs | 64.33% (112) | 73.17% (77) |
| sk1ll-gui/src-tauri/src/commands.rs | 35.56% (174) | 64.71% (96) |
| sk1ll-gui/src-tauri/src/editors.rs | 76.31% (59) | 84.34% (39) |
| sk1ll-gui/src-tauri/src/events.rs | 93.18% (3) | 94.44% (2) |
| sk1ll-gui/src-tauri/src/lib.rs | 0.00% (25) | 77.23% (23) |

- `harnesses.rs`: the percentage falls only because the deleted inline tests no longer count as covered lines. Fewer production lines are missed (28 to 26).
- `release_check.rs`: 5 more lines are missed: the cache read in `cached()`, which only the dropped `the_cache_lasts_a_day` reached. Tests never contact GitHub, so the check is not reachable end to end. The version comparison against the release JSON shape is still unit tested.
- `events.rs`: the two missed lines are the 5 s subscription timeout and the backoff reset after a connection that lasted 30 s. Covering them would add at least 5 s and 30 s of wall time. Paused Tokio time would fire the timeout while real socket I/O is in flight, which makes the test flaky. The baseline missed these same lines plus the malformed-event path, which the event test now covers.


## Replaced and removed tests

### `crates/sk1ll-cli/src/fleet.rs`

- `log_causes_read_as_text` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `utc_dates` dropped: the real Fleet flow checks UTC output for invitations and log entries. Historical edge dates exercised the private calendar formatter.

### `crates/sk1ll-cli/src/harnesses.rs`

- `listing_shows_what_sync_writes_into_unless_all` -> `harness_changes_preview_apply_and_sync` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `add_changes_only_the_given_options` -> `harness_changes_preview_apply_and_sync` (`crates/sk1ll-cli/tests/lifecycle.rs`)

### `crates/sk1ll-cli/src/main.rs`

- `releases_order_by_number_then_pre_release` -> `github_release_tags_only_announce_a_newer_version` (`crates/sk1ll-cli/src/release_check.rs`)
- `import_dry_run_requires_a_name_and_rejects_force` -> `argument_errors_and_help_have_stable_exit_codes` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `only_a_failed_fetch_of_the_bundled_link_falls_back` -> `add_uses_the_bundle_only_when_the_requested_source_cannot_be_fetched` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `conflict_reasons_name_the_command_for_an_edit_by_hand` -> `previews_explain_which_harness_edits_can_be_adopted` (`crates/sk1ll-cli/tests/recovery.rs`)
- `fetch_hint_is_added_once_and_only_to_fetch_failures` -> `fetch_failures_keep_gits_message_and_say_what_to_check` (`crates/sk1ll-cli/tests/lifecycle.rs`)

### `crates/sk1ll-cli/src/release_check.rs`

- `only_a_newer_release_is_announced` -> `github_release_tags_only_announce_a_newer_version` (`crates/sk1ll-cli/src/release_check.rs`)
- `the_cache_lasts_a_day` dropped: the release-check cache TTL and clock handling are private scheduling details.

### `crates/sk1ll-cli/src/setup.rs`

- `everything_on_sets_defaults_before_items` -> `setup_turns_on_existing_fleet_items_and_defaults_for_new_items` (`crates/sk1ll-cli/tests/setup.rs`)
- `shell_quote_leaves_plain_paths_and_quotes_the_rest` -> `setup_prints_a_shell_command_that_accepts_spaces_and_apostrophes` (`crates/sk1ll-cli/tests/setup.rs`)

### `crates/sk1ll-cli/tests/add.rs`

- `add_lists_picks_and_adds_a_skill` -> `add_lists_picks_and_adds_a_skill` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `fetch_failures_keep_gits_message_and_say_what_to_check` -> `fetch_failures_keep_gits_message_and_say_what_to_check` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `relative_sources_mean_the_cwd_on_the_command_line_and_the_repo_root_in_the_manifest` -> `relative_sources_mean_the_cwd_on_the_command_line_and_the_repo_root_in_the_manifest` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `update_says_what_changed_and_asks_for_a_review_only_then` -> `update_says_what_changed_and_asks_for_a_review_only_then` (`crates/sk1ll-cli/tests/lifecycle.rs`)

### `crates/sk1ll-cli/tests/commands.rs`

- `fleet_assignment_fails_when_the_server_is_unreachable` -> `fleet_assignment_fails_when_the_server_is_unreachable` (`crates/sk1ll-cli/tests/fleet.rs`)
- `sync_uses_the_authoritative_assignment_and_requires_a_snapshot` -> `sync_uses_the_authoritative_assignment_and_requires_a_snapshot` (`crates/sk1ll-cli/tests/fleet.rs`)
- `sync_refuses_a_pending_pull` -> `a_pending_pull_blocks_sync_repair_and_push` (`crates/sk1ll-cli/tests/recovery.rs`)
- `init_takes_the_existing_repos_engine_lock_before_writing` -> `init_takes_the_existing_repos_engine_lock_before_writing` (`crates/sk1ll-cli/tests/recovery.rs`)
- `assignment_defaults_preserve_existing_items` -> `standalone_assign_echoes_toggles_and_defaults_preserve_existing_items` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `changes_take_the_engine_lock` -> `changes_take_the_engine_lock` (`crates/sk1ll-cli/tests/recovery.rs`)
- `dry_run_leaves_another_syncs_work_in_progress_alone` -> `changes_take_the_engine_lock` (`crates/sk1ll-cli/tests/recovery.rs`)
- `repair_ids_keeps_the_id_with_the_baselines_claimant` -> `repair_ids_keeps_the_id_with_the_baselines_claimant` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `a_second_engine_for_another_repo_is_refused` -> `a_second_engine_for_another_repo_is_refused` (`crates/sk1ll-cli/tests/recovery.rs`)
- `a_daemon_for_another_repo_refuses_changes_and_lets_reads_through` -> `another_repos_daemon_refuses_writes_and_allows_previews` (`crates/sk1ll-cli/tests/fleet.rs`)
- `repair_ids_and_push_wait_for_a_pending_pull` -> `a_pending_pull_blocks_sync_repair_and_push` (`crates/sk1ll-cli/tests/recovery.rs`)
- `fleet_copy_assignment_confirms_with_preview_preconditions` -> `assignment_copy_confirms_with_both_reviewed_registrations_and_revisions` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_copy_assignment_with_nothing_to_change_asks_nothing` -> `assignment_copy_confirms_with_both_reviewed_registrations_and_revisions` (`crates/sk1ll-cli/tests/fleet.rs`)
- `only_a_missing_or_refused_socket_means_no_daemon` -> `only_a_missing_or_refused_socket_means_no_daemon` (`crates/sk1ll-cli/tests/fleet.rs`)
- `new_rename_and_delete_personal_items` -> `new_rename_and_delete_personal_items` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `list_labels_rules_and_mentions_missing_ids_only_when_they_wait` -> `list_labels_rules_and_mentions_missing_ids_only_when_they_wait` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `list_and_status_report_duplicate_item_id_paths` -> `list_and_status_report_duplicate_item_id_paths` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `recovery_commands_work_with_damaged_configuration` -> `recovery_commands_work_with_damaged_configuration` (`crates/sk1ll-cli/tests/recovery.rs`)
- `harness_changes_preview_apply_and_sync` -> `harness_changes_preview_apply_and_sync` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `harness_disable_and_remove_take_out_what_was_placed` -> `harness_disable_and_remove_take_out_what_was_placed` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `a_harness_change_during_a_pending_pull_defers_placement` -> `a_harness_change_during_a_pending_pull_defers_placement` (`crates/sk1ll-cli/tests/recovery.rs`)
- `harness_changes_stop_on_an_unreadable_assignment_and_report_failed_placements` -> `harness_changes_stop_on_an_unreadable_assignment_and_report_failed_placements` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `a_blocked_machine_that_committed_the_clash_follows_the_hint_out` -> `a_blocked_machine_that_committed_the_clash_follows_the_hint_out` (`crates/sk1ll-cli/tests/recovery.rs`)
- `a_blocked_machine_keeps_its_version_in_one_step` -> `a_blocked_machine_keeps_its_version_in_one_step` (`crates/sk1ll-cli/tests/recovery.rs`)
- `an_interrupted_keep_s_report_is_shown_until_dismissed` -> `an_interrupted_keep_s_report_is_shown_until_dismissed` (`crates/sk1ll-cli/tests/recovery.rs`)
- `draft_conflicts_and_recover_list_only_read` -> `draft_conflicts_and_recover_list_only_read` (`crates/sk1ll-cli/tests/recovery.rs`)
- `toggling_a_rule_in_a_block_file_leaves_no_recovery_entry` -> `toggling_a_rule_in_a_block_file_leaves_no_recovery_entry` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `commit_refuses_a_tree_the_server_would_refuse` -> `commit_refuses_a_tree_the_server_would_refuse` (`crates/sk1ll-cli/tests/recovery.rs`)
- `adopt_updates_a_renamed_skill_instead_of_recreating_the_old_name` -> `adopt_updates_a_renamed_skill_instead_of_recreating_the_old_name` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `a_manifest_broken_in_head_can_be_fixed_with_a_commit` -> `a_manifest_broken_in_head_can_be_fixed_with_a_commit` (`crates/sk1ll-cli/tests/recovery.rs`)
- `sync_fails_on_conflicts_and_force_says_where_the_edit_went` -> `sync_fails_on_conflicts_and_force_says_where_the_edit_went` (`crates/sk1ll-cli/tests/recovery.rs`)
- `empty_variables_are_unset_and_a_missing_repo_is_named` -> `empty_variables_use_defaults_and_missing_repos_name_the_path` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `recover_subcommands_explain_themselves_and_their_id` -> `argument_errors_and_help_have_stable_exit_codes` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `init_beside_a_daemon_for_another_repo_names_it` -> `another_repos_daemon_refuses_writes_and_allows_previews` (`crates/sk1ll-cli/tests/fleet.rs`)
- `standalone_assign_echoes_the_toggle` -> `standalone_assign_echoes_toggles_and_defaults_preserve_existing_items` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `commit_names_its_items_or_takes_them_all` -> `commit_names_its_items_or_takes_them_all` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `a_commit_without_an_origin_says_nothing_of_pushing` -> `commit_names_its_items_or_takes_them_all` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `a_joined_machine_reads_another_repo_but_changes_only_its_own` -> `a_joined_machine_reads_another_repo_but_changes_only_its_own` (`crates/sk1ll-cli/tests/fleet.rs`)

### `crates/sk1ll-cli/tests/fleet.rs`

- `fleet_lists_machines_and_baseline` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_invite_prints_code_and_join_command` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_invite_can_limit_the_scope` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_log_forwards_filters_and_prints_entries` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_undo_targets_entry_and_reports_no_change` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_remove_with_yes_deletes_machine` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_remove_without_yes_or_terminal_sends_no_request` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `removal_requires_yes_without_a_terminal` -> `a_standalone_machine_explains_which_commands_need_a_fleet` (`crates/sk1ll-cli/tests/fleet.rs`)
- `bash_completions_include_fleet_commands` -> `argument_errors_and_help_have_stable_exit_codes` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `assign_all_uses_the_harness_endpoint` -> `assign_all_reports_the_saved_revision_even_when_placement_fails` (`crates/sk1ll-cli/tests/fleet.rs`)
- `assign_all_reports_saved_revision_and_placement_failures` -> `assign_all_reports_the_saved_revision_even_when_placement_fails` (`crates/sk1ll-cli/tests/fleet.rs`)
- `fleet_lists_and_prunes_orphans_with_an_optional_machine` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)
- `assign_another_machine_goes_to_its_assignment` -> `joined_machines_share_a_baseline_and_scope_limits_changes` (`crates/sk1ll-cli/tests/fleet.rs`)

### `crates/sk1ll-cli/tests/recover.rs`

- `recover_list_shows_displaced_content` -> `sync_fails_on_conflicts_and_force_says_where_the_edit_went` (`crates/sk1ll-cli/tests/recovery.rs`)

### `crates/sk1ll-cli/tests/setup.rs`

- `setup_creates_a_standalone_repo_with_the_chosen_harnesses` -> `setup_creates_a_standalone_repo_with_the_chosen_harnesses` (`crates/sk1ll-cli/tests/setup.rs`)
- `setup_offers_and_enables_opt_in_presets` -> `setup_offers_and_enables_opt_in_presets` (`crates/sk1ll-cli/tests/setup.rs`)
- `init_adopts_opt_in_presets_only_without_a_machine_toml` -> `init_adopts_opt_in_presets_only_without_a_machine_toml` (`crates/sk1ll-cli/tests/setup.rs`)
- `setup_reprompts_bad_answers_and_stops_without_input` -> `setup_validates_answers_and_explains_first_machine_deployment` (`crates/sk1ll-cli/tests/setup.rs`)
- `setup_requires_a_join_code` -> `setup_validates_answers_and_explains_first_machine_deployment` (`crates/sk1ll-cli/tests/setup.rs`)
- `setup_explains_first_machine_deployment` -> `setup_validates_answers_and_explains_first_machine_deployment` (`crates/sk1ll-cli/tests/setup.rs`)

### `crates/sk1ll-cli/tests/skills.rs`

- `unmanaged_list_and_adopt_preserve_differing_variants_and_assignment` -> `unmanaged_list_and_adopt_preserve_differing_variants_and_assignment` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `edit_uses_visual_then_editor_and_refuses_imports_and_unmanaged_skills` -> `edit_uses_visual_then_editor_and_refuses_imports_and_unmanaged_skills` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `direct_adoption_rejects_a_repo_other_than_the_joined_repo` -> `joined_adoption_needs_the_joined_repo_and_a_reachable_server` (`crates/sk1ll-cli/tests/fleet.rs`)
- `joined_adoption_requires_a_reachable_server` -> `joined_adoption_needs_the_joined_repo_and_a_reachable_server` (`crates/sk1ll-cli/tests/fleet.rs`)
- `daemonless_assign_all_reaches_items_with_spaces` -> `daemonless_assign_all_reaches_items_with_spaces` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `harness_rename_moves_toggles_and_a_recreated_item_starts_from_the_default` -> `harness_rename_moves_toggles_and_a_recreated_item_starts_from_the_default` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `rename_requires_readable_membership_and_protects_destination_assignment` -> `rename_requires_readable_membership_and_protects_destination_assignment` (`crates/sk1ll-cli/tests/lifecycle.rs`)
- `standalone_sync_recovers_a_harness_rename_after_config_was_saved` -> `previews_leave_a_pending_rename_and_the_harnesses_byte_identical` (`crates/sk1ll-cli/tests/recovery.rs`)
- `previews_leave_a_pending_rename_and_the_harnesses_byte_identical` -> `previews_leave_a_pending_rename_and_the_harnesses_byte_identical` (`crates/sk1ll-cli/tests/recovery.rs`)

### `crates/sk1ll-cli/tests/testkit.rs`

- `a_testkit_machine_syncs_its_repo_into_its_harness` -> `changes_take_the_engine_lock` (`crates/sk1ll-cli/tests/recovery.rs`)

### `crates/sk1ll-gui/src-tauri/src/events.rs`

- `backoff_doubles_caps_and_restores` dropped: the SSE integration test checks increasing delays and reset after a valid event. The exact retry ceiling is a private tuning constant.

### `crates/sk1ll-gui/src-tauri/src/tests.rs`

- `commands_use_the_unix_client_and_preserve_payloads_and_errors` -> `desktop_commands_preserve_daemon_payloads_and_return_errors_without_retrying` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `transport_failure_is_structured` -> `desktop_distinguishes_a_missing_daemon_from_a_missing_route` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `event_task_reconnects_after_failure_and_eof_and_forwards_unchanged_events` -> `event_task_reconnects_after_failures_eof_and_malformed_events_and_forwards_valid_ones` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `draft_review_fingerprint_tracks_binary_edits_deletions_and_symlinks` -> `draft_review_fingerprints_change_when_binary_bytes_links_or_files_change` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `draft_bridge_adds_fingerprints_and_preserves_daemon_entries` -> `draft_review_fingerprints_change_when_binary_bytes_links_or_files_change` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `import_resolve_and_add_forward_once_with_payloads` -> `desktop_commands_preserve_daemon_payloads_and_return_errors_without_retrying` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `editor_detection_and_terminal_arguments` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `editor_targets_reject_escapes_and_imports` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `editor_resolution_uses_daemon_repo_and_rejects_item_paths` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `editor_bundle_detection_and_json_contract` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `skill_commands_preserve_queries_payloads_and_errors` -> `desktop_commands_preserve_daemon_payloads_and_return_errors_without_retrying` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `editor_resolution_uses_inventory_path_and_refuses_inventory_imports_and_missing_items` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `editor_inventory_accepts_literal_unusual_names` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `every_editor_launch_keeps_unusual_paths_in_one_argument` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `mac_terminal_script_shell_quoting_round_trips_unusual_paths` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `new_commands_forward_typed_payloads` -> `desktop_commands_preserve_daemon_payloads_and_return_errors_without_retrying` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `editors_get_the_user_environment_back_from_a_sandboxed_gui` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `harness_commands_forward_changes_and_open_only_the_daemons_machine_config` -> `editors_use_daemon_paths_and_keep_names_in_one_argument` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `the_bridge_preserves_a_missing_route_status` -> `desktop_distinguishes_a_missing_daemon_from_a_missing_route` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)
- `sync_commands_forward_preview_and_apply_separately` -> `desktop_commands_preserve_daemon_payloads_and_return_errors_without_retrying` (`crates/sk1ll-gui/src-tauri/tests/desktop.rs`)

### `crates/sk1ll-gui/tests/test_dev_fleet.py`

- `test_gui_is_explicit_workspace_member_without_anchor` dropped: a hard-coded workspace member list went stale when phase 0 added testkit. Building the desktop crate checks its Cargo membership and configured UI assets.

The harness preview assertions in `harness_commands_forward_changes_and_open_only_the_daemons_machine_config` also moved to the desktop IPC request table. The editor IPC test checks discovered terminal and GUI launchers with a subprocess. Exact argv for every terminal emulator is a launcher implementation detail.
