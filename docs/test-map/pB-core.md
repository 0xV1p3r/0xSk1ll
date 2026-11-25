# Phase B (rest of core): test map

Base 3cc7ee4. Every inline `#[cfg(test)]` test of the in-scope sk1ll-core
modules was deleted; each line below says where what it pinned is pinned now
(all new files are `crates/sk1ll-core/tests/<file>`), or why it was dropped.
`common/mod.rs` holds the shared fixtures (`Scratch`, `Fleet`, `snapshot`).

## Known gap: an import interrupted after publication (not fixed)

Pinned by `known_gap_an_import_interrupted_after_publication_is_left_with_stale_or_missing_metadata`
(imports.rs). The sweep `an_import_interrupted_at_any_failpoint_keeps_the_draft_and_one_whole_version`
covers every other hit and hands these cases to it.

- **Window**: `Repo::add_fetched` / `Repo::apply_updates` (`sk1ll add`, `sk1ll update`)
  publish the staged tree with a rename into `imported/<name>` right after
  failpoint `SourceInstall`, then `lock_vendored` hashes the published tree
  (failpoint `HashMidRead`, the only one in the window) and writes `sk1ll.lock`;
  an add then writes `[import.<name>]` into `sk1ll.toml` (`record_import`). A crash
  or kill anywhere between the rename and those writes is not journaled.
- **On disk afterwards**: `imported/<name>/` holds the whole new upstream tree (no
  mixed files, no staging directory left). `sk1ll.lock` still holds the old entry
  (update) or none (add); for an add `sk1ll.toml` has no `[import.<name>]`. A crash
  between the lock write and the manifest write (no failpoint there) leaves the
  lock entry without a manifest entry. `recover_imports` does nothing for any of
  these: it only restores set-aside backups.
- **User impact**: no content is lost, but the import is stuck until fixed by hand.
  After an interrupted **add**, `imported/<name>` is recorded nowhere: the Draft
  lists it as a new skill, and running the same add again fails with "… already
  exists but is not a recorded import; move it away or pick another name" until the
  user deletes or moves the directory. After an interrupted **update**, the new tree
  sits under the old lock hash, so `import_status` reports it Modified, `sk1ll
  update` refuses with "locally modified imports: <name> (use `fork` to keep edits,
  or force to overwrite)" (misleading: the user edited nothing), and only `update
  --force` records it; committing before that would push the new tree with the old
  lock entry.

## Coverage, before and after

`cargo llvm-cov -p sk1ll-core --all-features --summary-only`, at 3cc7ee4 and at
this branch (core's own tests only; coverage-baseline.txt is workspace-wide, so
its percentages are higher where daemon, server and CLI tests reach core). At
3cc7ee4 a file's line count includes its inline test module, so raw percentages
don't compare: the table divides by the production lines (this branch's count)
and compares missed lines. Missed lines at 3cc7ee4 include a few unexecuted
lines of the inline tests themselves, so "before" errs low.

| file | prod lines | missed before | missed after | prod line cover before | after |
|---|---:|---:|---:|---:|---:|
| assignment.rs | 93 | 16 | 15 | 82.8% | 83.9% |
| baseline_check.rs | 54 | 12 | 10 | 77.8% | 81.5% |
| bundled.rs | 12 | 0 | 0 | 100.0% | 100.0% |
| config_file.rs | 215 | 22 | 19 | 89.8% | 91.2% |
| content.rs | 410 | 91 | 89 | 77.8% | 78.3% |
| content/pinned.rs | 117 | 13 | 13 | 88.9% | 88.9% |
| draft.rs | 978 | 107 | 81 | 89.1% | 91.7% |
| draft/keep.rs | 338 | 44 | 44 | 87.0% | 87.0% |
| draft/pull.rs | 978 | 96 | 96 | 90.2% | 90.2% |
| draft/pull/journal.rs | 93 | 11 | 11 | 88.2% | 88.2% |
| draft/pull/worktree.rs | 375 | 69 | 65 | 81.6% | 82.7% |
| engine.rs | 50 | 17 | 16 | 66.0% | 68.0% |
| git/config_env.rs | 13 | 13 | 0 | 0.0% | 100.0% |
| git/filters.rs | 94 | 5 | 5 | 94.7% | 94.7% |
| git/mod.rs | 290 | 54 | 51 | 81.4% | 82.4% |
| git/plumbing.rs | 417 | 133 | 132 | 68.1% | 68.3% |
| harness_change.rs | 117 | 30 | 30 | 74.4% | 74.4% |
| harness_rename.rs | 128 | 128 | 23 | 0.0% | 82.0% |
| hash.rs | 384 | 36 | 34 | 90.6% | 91.1% |
| imports.rs | 383 | 51 | 50 | 86.7% | 86.9% |
| instructions.rs | 357 | 26 | 22 | 92.7% | 93.8% |
| item_id.rs | 611 | 108 | 109 | 82.3% | 82.2% |
| lock.rs | 15 | 2 | 1 | 86.7% | 93.3% |
| machine.rs | 574 | 93 | 51 | 83.8% | 91.1% |
| paths.rs | 40 | 19 | 18 | 52.5% | 55.0% |
| redact.rs | 35 | 1 | 1 | 97.1% | 97.1% |
| repo.rs | 378 | 73 | 78 | 80.7% | 79.4% |
| skill.rs | 60 | 6 | 0 | 90.0% | 100.0% |
| source.rs | 472 | 20 | 26 | 95.8% | 94.5% |
| sync.rs | 4924 | 312 | 325 | 93.7% | 93.4% |
| TOTAL | 14437 | 1800 | 1587 | 87.5% | 89.0% |

Where a file misses more lines than before:
- repo.rs (+5): `stored_spelling` and `respell` only run on a case-insensitive
  filesystem; through the public API they are reached by
  restoring_a_case_only_rename_restores_the_spelling, which returns early on
  case-sensitive filesystems (docs/development.md). Their direct unit tests were
  private-function tests.
- source.rs (+6): `explain_github_failure` needs a real github.com answer
  ("could not read Username" / "Repository not found") and no offline boundary
  produces one; one branch of an explicit ref on a plain source.
- item_id.rs (+1): one error arm of the per-tree claims cache.
- sync.rs (+13 in the summary; phase A's file): `--show-missing-lines` lists no
  line missed now that was hit before, so this is region accounting, not a lost
  path.

## Numbers

- Tests in scope: 224 inline tests (3cc7ee4) -> 190 integration/contract tests.
- Test code in scope: 6,856 lines deleted from `src/` -> 6,478 lines in
  `crates/sk1ll-core/tests/` (incl. `common/mod.rs`).
- Wall time of `cargo test -p sk1ll-core --all-features` (built): 17 s at
  3cc7ee4 (all 321 core tests in one binary) -> 46 s here (97 inline sync tests,
  phase A's, plus 190 in 17 binaries). The single-threaded sweeps dominate:
  pull.rs 18 s, keep.rs 14 s, baseline.rs 3 s.

## Deleted tests

### assignment.rs
- materializing_preserves_existing_items_when_defaults_change -> sync_keeps_explicit_toggles_when_defaults_flip_and_applies_a_whole_harness_toggle (harness_change.rs)
- toggles_override_defaults -> sync_keeps_explicit_toggles_when_defaults_flip_and_applies_a_whole_harness_toggle (harness_change.rs); per-kind separation also by turning_the_items_off_first_lets_sync_take_them_out (harness_change.rs)
- setting_a_harness_reaches_every_item -> sync_keeps_explicit_toggles_when_defaults_flip_and_applies_a_whole_harness_toggle (harness_change.rs)
- renaming_a_harness_moves_its_toggles -> a_pending_rename_carries_assignment_toggles_and_reports_rules_still_routed_to_the_old_name (harness_rename.rs)
- round_trips_and_missing_is_default -> the_assignment_file_round_trips_and_a_missing_one_is_the_default (contracts.rs)

### bundled.rs
- only_the_documented_link_has_a_bundled_copy -> the_bundled_skill_is_added_for_its_documented_link_only_and_never_replaces (repo.rs)
- the_bundled_copy_lands_in_personal_and_never_replaces -> the_bundled_skill_is_added_for_its_documented_link_only_and_never_replaces (repo.rs)

### config_file.rs
- creates_config_under_a_missing_directory_beneath_a_symlink -> creates_config_under_a_missing_directory_beneath_a_symlink (config_file.rs)
- retargeting_during_staging_aborts_publish -> retargeting_during_staging_aborts_publish (config_file.rs)
- replaces_keeping_mode_and_never_using_an_occupied_temp_name -> replaces_keeping_mode_and_never_using_an_occupied_temp_name (config_file.rs)
- a_symlinked_file_stays_a_link_and_a_retargeted_link_is_refused -> a_symlinked_file_stays_a_link_and_a_retargeted_link_is_refused (config_file.rs)
- a_change_since_the_look_is_refused -> a_change_since_the_look_is_refused (config_file.rs)
- a_fifo_is_refused_without_blocking -> a_fifo_is_refused_without_blocking (config_file.rs)
- staging_never_follows_or_publishes_a_planted_temp_name -> replaces_keeping_mode_and_never_using_an_occupied_temp_name (config_file.rs) for the planted temp name; `Staged::place_new` never replacing is exercised only through sync's index writes (phase A)
- (new) every_config_write_checkpoint_leaves_a_readable_file_that_can_be_retried (config_file.rs): ConfigBeforeCheck/ConfigBeforePublish sweep

### content.rs
- listing_and_browsing_use_draft_and_fall_back_to_head_for_deleted_items -> same name (content.rs)
- a_personal_sidecar_is_reserved_and_malformed_metadata_is_an_error -> same name (content.rs)
- rule_symlinks_cannot_carry_ids_on_disk_or_in_git -> same name (content.rs)
- files_reject_escape_flag_binary_and_bound_memory -> same name (content.rs)
- unmanaged_browsing_requires_variant_when_content_differs -> same name (content.rs)
- removing_only_skill_md_still_serves_the_remaining_draft_files -> same name (content.rs)
- sk1ll_names_below_a_skills_root_are_browsed_and_edited -> same name (content.rs)
- ancestor_swap_cannot_redirect_file_or_tree -> same name (content.rs)

### draft.rs (and draft/keep.rs, draft/pull*)
- draft_commands_run_no_filter_driver -> draft_commands_run_no_filter_driver (draft.rs)
- one_side_of_an_explicit_id_rename_cannot_be_committed_or_restored -> both_sides_of_an_item_id_rename_go_together_or_not_at_all (draft.rs)
- committing_a_handmade_import_persists_its_id_and_requires_the_manifest -> a_handmade_import_is_committed_with_the_manifest_that_holds_its_new_item_id (draft.rs)
- item_commit_tracks_an_ignored_sidecar -> committing_a_skill_tracks_its_item_id_sidecar_even_when_gitignore_hides_it (draft.rs)
- item_refs_round_trip -> item_refs_round_trip_and_name_the_item_a_path_belongs_to (contracts.rs)
- detects_draft_items -> the_draft_lists_each_changed_item_once_with_its_paths_and_diff (draft.rs)
- partial_commit_leaves_the_rest_in_draft -> a_partial_commit_records_only_the_chosen_items_and_leaves_the_rest_in_the_draft (draft.rs)
- restore_restores_baseline -> restoring_items_returns_them_and_only_them_to_the_baseline (draft.rs)
- pull_with_clean_draft_fast_forwards -> fetching_and_pushing_split_from_the_local_steps (pull.rs; Updated) and a_pull_restarts_at_every_checkpoint_without_losing_draft_or_staged_changes (pull.rs; UpToDate after)
- fetching_and_pushing_split_from_the_local_steps -> same name (pull.rs)
- pull_leaves_imports_being_staged_alone -> same name (pull.rs)
- pull_carries_a_disjoint_draft -> a_pull_restarts_at_every_checkpoint_without_losing_draft_or_staged_changes and pull_rebases_without_touching_the_draft_or_author_metadata (pull.rs)
- pull_replay_detects_duplicate_ids_from_two_clones_before_push -> same name (pull.rs)
- pull_blocked_by_clash_changes_nothing -> same name (pull.rs)
- restore_and_commit_refuse_to_cross_item_boundaries -> restore_and_commit_refuse_to_split_a_path_that_changed_between_file_and_directory (draft.rs)
- pull_never_overwrites_ignored_files -> same name (pull.rs)
- pull_protects_an_ignored_file_created_during_the_pull -> same name (pull.rs)
- pull_blocks_a_save_made_during_the_pull -> same name (pull.rs)
- pull_rebase_conflict_leaves_head_index_and_files_unchanged -> same name (pull.rs)
- a_conflicting_unpushed_commit_turns_back_into_draft_and_comes_back_after_the_pull -> same name (pull.rs)
- keep_mine_takes_the_baseline_and_puts_the_draft_back_on_top -> same name (keep.rs)
- keep_mine_leaves_a_pull_blocked_on_an_item_it_doesnt_name -> same name (keep.rs)
- keep_mine_that_cant_put_a_file_back_leaves_it_in_recovery -> same name (keep.rs)
- keep_mine_never_overwrites_a_save_made_during_its_restore -> same name (keep.rs)
- keep_mine_takes_names_that_clash_only_in_an_unpushed_commit -> same name (keep.rs)
- a_directory_in_the_way_of_a_restore_is_left_alone -> same name (keep.rs)
- an_interrupted_keep_leaves_your_versions_in_recovery_reported -> an_interrupted_keep_preserves_originals_and_later_edits_at_every_checkpoint (keep.rs; sweep over every Keep* hit)
- an_interrupted_keep_is_finished_by_hand_with_recover_adopt -> an_interrupted_keep_s_report_stays_until_each_version_is_acted_on (keep.rs; adopts a reported version and checks it comes back, and that the pull side is done)
- an_interrupted_keep_resets_only_its_own_staging -> same name (keep.rs)
- keep_mine_tells_a_pull_s_staging_from_the_user_s -> same name (keep.rs)
- an_un_commit_stopped_before_recording_its_staging_counts_as_the_user_s -> same name (keep.rs)
- an_interrupted_keep_s_report_stays_until_each_version_is_acted_on -> same name (keep.rs)
- an_unreadable_recovery_entry_stays_in_the_report -> an_interrupted_keep_s_report_stays_until_each_version_is_acted_on (keep.rs)
- report_notes_stay_until_dismissed -> an_interrupted_keep_s_report_stays_until_each_version_is_acted_on (keep.rs)
- keep_mine_refuses_a_kept_path_the_user_staged -> same name (keep.rs)
- commands_in_messages_quote_paths_for_the_shell -> dropped: private `shell_quote`; the commands in keep's messages are checked as text by keep_mine_refuses_a_kept_path_the_user_staged and keep_mine_that_cant_put_a_file_back_leaves_it_in_recovery, but quoting of unusual paths is no longer pinned
- paths_are_byte_exact -> paths_that_are_not_utf_8_or_hold_backslashes_are_listed_and_restored_byte_exactly (draft.rs)
- restore_keeps_a_manifest_that_was_never_committed -> restore_keeps_a_manifest_that_no_baseline_has (draft.rs)
- a_commit_can_replace_metadata_head_cannot_read -> a_commit_may_repair_metadata_head_cannot_read_but_never_commit_unreadable_metadata (draft.rs)
- commit_refuses_a_detached_head -> commit_and_push_refuse_a_detached_head_and_change_nothing (draft.rs)
- restore_refuses_to_delete_ignored_content -> restore_never_deletes_ignored_content_and_commit_never_touches_it (draft.rs)
- other_item_names_are_injective_and_round_trip -> same name (contracts.rs)
- quoted_and_literal_names_stay_distinct -> a_quoted_name_and_the_literal_name_it_spells_are_two_items (draft.rs)
- pull_rebases_without_touching_the_draft_or_author_metadata -> same name (pull.rs)
- pull_leaves_an_identical_stash_in_place -> same name (pull.rs)
- pull_drops_commits_that_become_empty -> same name (pull.rs)
- pull_refuses_unpushed_merge_commits -> same name (pull.rs)
- pull_refuses_unrelated_histories_without_touching_anything -> same name (pull.rs)
- pull_and_push_refuse_a_branch_other_than_the_fleet_branch -> same name (pull.rs)
- pull_needs_the_fleet_branch_on_origin_unless_origin_is_empty -> same name (pull.rs)
- pull_ignores_autostash_merge_options_and_hooks -> same name (pull.rs)
- pull_never_writes_into_or_replaces_a_staging_directory -> same name (pull.rs)
- pull_checks_the_whole_worktree_around_a_skills_subdirectory -> same name (pull.rs; now with an isolated HOME)
- pull_refuses_submodules -> same name (pull.rs)
- pull_refuses_sparse_checkouts -> same name (pull.rs)
- pull_names_the_items_a_replayed_commit_conflicts_in -> same name (pull.rs)
- pull_replays_commits_byte_for_byte -> same name (pull.rs)
- a_pull_that_stopped_half_way_is_finished_later -> a_pull_restarts_at_every_checkpoint_without_losing_draft_or_staged_changes (pull.rs; sweep over every Pull* hit, fast-forward and rebase, finish or pull to resume, one real process exit)
- a_save_during_a_file_swap_is_kept_in_recovery -> same name (pull.rs)
- a_draft_change_after_the_checks_blocks_the_rest_of_the_pull -> same name (pull.rs)
- a_save_to_an_untouched_file_of_a_changed_item_stays_in_the_draft -> same name (pull.rs)
- an_index_lock_is_an_error_not_a_clash -> same name (pull.rs)
- a_pull_resumes_over_converted_line_endings -> same name (pull.rs)
- a_pull_through_a_filter_driver_is_refused_and_runs_no_driver -> same name (pull.rs; table with the `set` cases)
- a_filter_driver_named_set_is_a_driver -> a_pull_through_a_filter_driver_is_refused_and_runs_no_driver (pull.rs)
- a_resumed_pull_finishes_with_filter_drivers_off -> same name (pull.rs)
- pull_keeps_permissions_and_sets_the_executable_bit -> same name (pull.rs)
- pull_stops_when_another_command_switches_branches -> same name (pull.rs)
- pull_stops_when_another_command_moves_the_branch -> same name (pull.rs)
- a_failed_write_rolls_forward_on_the_next_pull -> same name (pull.rs)
- pull_keeps_the_working_tree_free_of_its_own_files -> same name (pull.rs)
- pull_probes_exchange_before_starting -> same name (pull.rs)
- recovery_keeps_open_writers_through_prune_and_adoption -> same name (pull.rs)
- draft_adoption_refuses_an_ignored_destination_absent_from_head -> same name (pull.rs)
- a_directory_displaced_during_removal_is_reported_and_preserved -> same name (pull.rs)
- resumed_pull_leaves_later_edits_to_published_paths_as_draft -> same name (pull.rs)
- pull_replaces_a_nested_directory_with_a_file -> same name (pull.rs)
- pull_recovers_byte_paths_and_full_length_leaf_names -> same name (pull.rs)
- pull_applies_mode_only_changes_and_recovers_a_racing_chmod -> same name (pull.rs)
- pull_accepts_a_case_only_rename_on_case_folding_filesystems -> same name (pull.rs)
- pull_requires_the_repos_engine_lock -> same name (pull.rs)

### engine.rs
- second_holder_is_refused_until_the_first_drops -> same name (engine.rs)
- a_forked_child_does_not_keep_a_dropped_lock -> same name (engine.rs)

### git/filters.rs
- drivers_are_read_from_every_key_with_dots_and_case_kept -> the_repo_disables_git_drivers_named_by_local_and_environment_config (git.rs; `Mixed=Name`, `a.b`, an environment-config driver)
- set_unset_and_unspecified_count_only_as_configured_drivers -> a_pull_through_a_filter_driver_is_refused_and_runs_no_driver (pull.rs; `filter=set` and bare `filter`); `-filter` and `filter=unset` dropped: they name no driver, which every unfiltered pull and draft test exercises
- neutralized_drivers_never_run -> the_repo_disables_git_drivers_named_by_local_and_environment_config (git.rs) and draft_commands_run_no_filter_driver (draft.rs)

### git/mod.rs
- locale_is_pinned -> git_commands_keep_credentials_isolated_and_merge_config_entries_case_insensitively (git.rs; LC_ALL/LANGUAGE seen by a git alias)
- hung_network_command_times_out_and_is_killed -> same name (git.rs; the deadline now comes from `SK1LL_GIT_TIMEOUT=300ms` in a child process)
- stdin_and_output_still_flow_under_a_deadline -> same name (git.rs)
- deadlines_parse_and_pick_network_commands -> hung_network_command_times_out_and_is_killed (git.rs; a `ms` value of `SK1LL_GIT_TIMEOUT` applied to a network command); the other spellings (`90`, `2m`, `1h`, `0`, `none`, garbage) are dropped: private `parse_timeout`

### git/plumbing.rs
- credentials_in_urls -> secrets_are_redacted_from_messages_and_stripped_from_urls (contracts.rs)
- an_unborn_clone_blocks_every_first_baseline_obstruction -> the_first_baseline_never_replaces_an_obstruction_present_or_appearing_later (baseline.rs)
- first_baseline_preserves_content_created_after_preflight -> the_first_baseline_never_replaces_an_obstruction_present_or_appearing_later (baseline.rs; now also unignored and dangling-symlink obstructions appearing during publish)
- a_committed_init_is_pristine_until_anything_else_is_kept -> same name (baseline.rs)
- a_lone_init_commit_gives_way_to_an_unrelated_first_baseline_only -> same name (baseline.rs)
- standalone_join_refuses_ignored_content_in_unborn_clone -> standalone_join_refuses_ignored_content_in_an_unborn_repo (baseline.rs)
- an_unborn_clone_adopts_the_baseline_and_keeps_unrelated_drafts -> an_unborn_repo_adopts_the_baseline_and_keeps_unrelated_drafts (baseline.rs)
- an_existing_clone_on_another_branch_is_refused_before_joining -> same name (baseline.rs)
- an_adopted_clone_must_share_the_servers_history -> a_clone_is_checked_out_and_an_adopted_one_must_share_the_servers_history (baseline.rs)
- a_clone_runs_no_filter_driver_a_conditional_include_names -> same name (baseline.rs)
- a_clone_is_checked_out -> a_clone_is_checked_out_and_an_adopted_one_must_share_the_servers_history (baseline.rs)

### harness_change.rs
- disabling_lists_what_sync_placed_and_leaves_it -> same name (harness_change.rs)
- unreadable_old_instructions_warn_without_blocking_disable_or_remove -> same name (harness_change.rs)
- turning_the_items_off_first_lets_sync_take_them_out -> same name (harness_change.rs)
- moving_a_path_lists_only_the_old_location -> same name (harness_change.rs)
- a_harness_cannot_sync_into_the_repo -> same name (harness_change.rs)

### hash.rs
- hashes_of_ordinary_trees_are_unchanged -> copied_content_uses_the_same_digest_for_old_trees_and_large_files (hashing.rs; golden digests)
- only_git_and_the_root_placement_marker_are_left_out -> copied_content_uses_the_same_digest_for_old_trees_and_large_files (hashing.rs)
- files_larger_than_the_read_buffer_hash_as_before -> copied_content_uses_the_same_digest_for_old_trees_and_large_files (hashing.rs)
- a_file_resized_while_hashed_never_yields_a_mixed_hash -> a_file_resized_during_a_sync_hash_leaves_the_copy_consistent_with_the_source (hashing.rs; HashMidRead)
- payload_removal_keeps_what_changed_after_the_check -> recovery_keeps_payloads_changed_after_their_last_check (hashing.rs; HashBeforeRemoval)
- extended_attributes_are_part_of_the_payload -> recovery_keeps_payloads_changed_after_their_last_check (hashing.rs; xattr case)
- backslash_is_part_of_a_unix_file_name -> digests_distinguish_unix_path_bytes_and_symlink_targets (hashing.rs)
- the_cache_rehashes_a_tree_whose_content_changed_with_the_same_mtime -> sync_plans_see_rewrites_replacements_and_additions_with_an_unchanged_mtime (hashing.rs)
- a_tree_changed_within_the_racy_window_is_not_remembered -> sync_plans_see_rewrites_replacements_and_additions_with_an_unchanged_mtime (hashing.rs; also `HashCache::sweep` forgetting unused trees)

### imports.rs
- remove_rejects_unsafe_paths_before_mutating -> same name (imports.rs)
- remove_rejects_symlink_target_and_parent -> same name (imports.rs)
- preview_reports_warnings_from_git_blobs -> same name (imports.rs)
- update_preview_lists_commits_and_only_files_in_the_skill -> same name (imports.rs)
- resolving_lists_skills_without_writing_the_repo -> same name (imports.rs)
- adding_a_link_needs_exactly_one_skill -> same name (imports.rs)
- adding_never_replaces_an_unrecorded_imported_dir -> same name (imports.rs)
- adding_a_previewed_commit_vendors_exactly_that_commit -> same name (imports.rs)
- fork_rejects_symlinked_paths_before_touching_the_import -> same name (imports.rs)
- a_local_vendor_sidecar_edit_blocks_update -> same name (imports.rs)
- fork_removes_inline_import_and_requires_both_commit_and_restore_sides -> same name (imports.rs)
- add_records_an_id_and_fork_carries_it_into_the_personal_skill -> same name (imports.rs)
- invalid_input_is_refused_before_fetching -> same name (imports.rs)
- fetched_links_are_vendored_offline_and_rechecked -> same name (imports.rs)
- fetched_updates_apply_offline_unless_the_import_changed -> same name (imports.rs)
- smudge_filters_run_while_staging_not_while_installing -> smudge_filters_run_in_the_network_phase_never_while_installing (imports.rs)
- smudge_filters_run_while_fetching_updates_not_while_applying -> smudge_filters_run_in_the_network_phase_never_while_installing (imports.rs)
- adding_never_removes_what_appeared_while_staging -> same name (imports.rs; now also through `add_import`)
- a_failed_update_install_keeps_the_previous_import -> same name (imports.rs)
- staging_happens_where_imported_really_is -> same name (imports.rs)
- staging_never_shows_in_the_draft_or_as_a_skill -> same name (imports.rs)
- add_import_never_removes_what_appeared_while_staging -> adding_never_removes_what_appeared_while_staging (imports.rs)
- a_failed_rollback_is_recovered_before_the_next_sync -> same name (imports.rs)
- the_exclude_line_is_only_ever_appended_once -> same name (imports.rs)
- recovery_never_deletes_the_backup_on_doubt -> same name (imports.rs)
- (new) an_import_interrupted_at_any_failpoint_keeps_the_draft_and_one_whole_version (imports.rs): Source*/Hash* sweep of add, update, failed update
- (new) known_gap_an_import_interrupted_after_publication_is_left_with_stale_or_missing_metadata (imports.rs): see above

### instructions.rs
- rule_strips_multiline_flow_routing_without_changing_metadata -> routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept (contracts.rs)
- rule_preserves_unindented_quoted_scalar_continuations -> routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept (contracts.rs)
- rule_preserves_comments_and_crlf_around_routing_entries -> routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept (contracts.rs)
- rule_omits_frontmatter_when_routing_is_only_key -> routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept (contracts.rs)
- rule_restores_equivalent_routing_values -> routing_written_any_yaml_way_leaves_exactly_the_rest_of_the_front_matter (contracts.rs)
- rule_preserves_comments_after_routing_and_inside_block_scalars -> routing_written_any_yaml_way_leaves_exactly_the_rest_of_the_front_matter (contracts.rs)
- unsafe_metadata_source_ranges_are_discovery_errors -> malformed_routing_or_front_matter_it_cannot_remove_safely_is_a_discovery_error (contracts.rs)
- multiline_harnesses_and_targets_restrict_rules -> routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept (contracts.rs)
- routing_ignores_nested_and_embedded_keys_and_preserves_rule_frontmatter -> routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept (contracts.rs)
- routing_accepts_yaml_strings_and_validates_both_top_level_keys -> a_routing_value_names_one_harness_or_a_list_and_harnesses_wins_over_targets and malformed_routing_or_front_matter_it_cannot_remove_safely_is_a_discovery_error (contracts.rs)
- invalid_harness_restrictions_are_errors -> malformed_routing_or_front_matter_it_cannot_remove_safely_is_a_discovery_error (contracts.rs)
- appends_replaces_and_removes_block -> syncing_a_block_replaces_and_removes_only_the_managed_lines (harness_change.rs)
- rule_keeps_frontmatter_except_harnesses -> routing_is_removed_from_a_rendered_rule_and_every_other_byte_of_its_front_matter_kept (contracts.rs)
- detects_hand_edits_and_bad_markers -> syncing_a_block_replaces_and_removes_only_the_managed_lines (harness_change.rs; a hand edit is a Conflict) and instructions_markers_bound_the_managed_text_and_report_a_missing_end (contracts.rs)

### item_id.rs
- id_problems_name_items_without_an_item_id_of_their_own -> the_baseline_rejection_names_missing_and_shared_item_ids (item_ids.rs)
- historical_claims_are_cached_by_tree_and_stream_past_large_rule_bodies -> item_id_discovery_agrees_between_git_and_the_working_tree (item_ids.rs; a 2 MiB rule body read from git); the per-tree cache is dropped: private, a performance detail
- tree_ids_match_the_working_tree -> item_id_discovery_agrees_between_git_and_the_working_tree (item_ids.rs)

### machine.rs
- notification_preference_defaults_on_and_preserves_other_config -> same name (machine_toml.rs)
- detects_presets_by_home_dir -> same name (machine_toml.rs)
- entries_override_extend_and_disable -> same name (machine_toml.rs)
- choices_list_disabled_presets_and_set_enabled_keeps_comments -> same name (machine_toml.rs)
- harness_changes_keep_the_file_and_write_only_what_differs -> same name (machine_toml.rs; its list of refused edits is covered, with messages, by invalid_harness_edits_name_the_problem_and_preserve_machine_toml and paths_are_checked_where_they_lead)
- opt_in_presets_stay_off_on_an_upgraded_machine -> same name (machine_toml.rs)
- first_run_setup_enables_the_opt_in_presets_it_finds -> same name (machine_toml.rs)
- setup_skips_adoption_when_destinations_overlap_harnesses_or_repo -> same name (machine_toml.rs)
- loading_machine_config_rejects_a_fifo -> machine_toml_reports_malformed_fields_at_the_config_path (machine_toml.rs)
- a_custom_harness_named_like_an_opt_in_preset_keeps_its_paths -> same name (machine_toml.rs)
- an_opt_in_preset_with_paths_of_its_own_reads_the_same_when_off -> same name (machine_toml.rs)
- setting_never_switches_off_a_harness_that_was_on -> same name (machine_toml.rs)
- setting_a_preset_value_again_drops_the_override -> same name (machine_toml.rs)
- paths_are_checked_where_they_lead -> same name (machine_toml.rs)
- missing_file_uses_hostname_and_rejects_unknown_keys -> machine_toml_reports_malformed_fields_at_the_config_path (machine_toml.rs)
- (new) machine_toml_reports_malformed_fields_at_the_config_path, invalid_harness_edits_name_the_problem_and_preserve_machine_toml (machine_toml.rs): every error message a user can hit

### redact.rs
- redaction -> secrets_are_redacted_from_messages_and_stripped_from_urls (contracts.rs)

### repo.rs
- init_starts_on_the_fleet_branch_whatever_git_defaults_to -> same name (repo.rs)
- a_committed_init_starts_with_an_empty_draft_but_leaves_an_existing_repo_alone -> same name (repo.rs)
- init_preserves_existing_destinations_and_refuses_symlink_ancestors -> same name (repo.rs)
- recovery_discovery_finds_a_deleted_manifest -> same name (repo.rs)
- recovery_discovery_never_picks_an_unrelated_repo -> same name (repo.rs)
- recovery_discovery_recognises_a_skills_repo_by_layout_and_history -> same name (repo.rs)
- creates_draft_content_and_refuses_invalid_or_taken_names -> same name (repo.rs)
- rename_never_writes_through_a_planted_stage_symlink -> same name (repo.rs)
- a_failed_rename_says_whether_the_item_moved -> same name (repo.rs)
- deletes_and_renames_only_personal_items -> same name (repo.rs)
- reader_leaves_item_ids_unassigned_until_they_are_persisted -> same name (repo.rs)
- copied_skill_ids_report_both_paths_and_block_creation -> same name (repo.rs)
- an_unassigned_skill_stays_at_its_old_name_if_the_id_cannot_be_written -> same name (repo.rs)
- an_interrupted_case_rename_reports_the_stage_path -> same name (repo.rs)
- a_case_only_rename_keeps_the_id_and_uses_the_requested_spelling -> same name (repo.rs)
- respelling_moves_everything_and_replaces_nothing -> restoring_a_case_only_rename_restores_the_spelling (repo.rs; case-folding filesystems only, returns early elsewhere)
- a_stored_spelling_needs_a_case_folding_alias -> restoring_a_case_only_rename_restores_the_spelling (repo.rs) and pull_accepts_a_case_only_rename_on_case_folding_filesystems (pull.rs)
- restoring_a_case_only_rename_restores_the_spelling -> same name (repo.rs)
- frontmatter_rename_keeps_everything_else -> deletes_and_renames_only_personal_items (repo.rs; `name:` rewritten, other keys and a non-matching `name:` kept); the CRLF and YAML-keyword quoting cases are dropped: private `rename_frontmatter`

### skill.rs
- splits_frontmatter -> skill_front_matter_splits_off_and_reads_inline_and_folded_fields (contracts.rs)
- parses_inline_and_folded -> skill_front_matter_splits_off_and_reads_inline_and_folded_fields (contracts.rs)

### source.rs
- a_github_username_prompt_means_missing_or_private -> dropped: private `explain_github_failure` needs github.com; no offline boundary reaches it
- parses_github_links -> source_links_resolve_the_longest_ref_and_check_the_blob_at_that_commit (imports.rs; github `tree`/`blob` links resolved through `insteadOf` to a local origin) and invalid_input_is_refused_before_fetching (imports.rs; malformed links and their messages)
- decodes_link_segments_and_rejects_unsafe_ones -> source_links_resolve_the_longest_ref_and_check_the_blob_at_that_commit (imports.rs; `%2F`) and invalid_input_is_refused_before_fetching (imports.rs; bad percent-encoding, `..` plain and encoded, non-SKILL.md blobs)
- default_names -> resolving_lists_skills_without_writing_the_repo and adding_a_link_needs_exactly_one_skill (imports.rs)
- splits_the_longest_ref_that_resolves -> source_links_resolve_the_longest_ref_and_check_the_blob_at_that_commit (imports.rs)
- explicit_ref_and_path_override_the_link -> source_links_resolve_the_longest_ref_and_check_the_blob_at_that_commit (imports.rs)
- fetches_into_one_cache_take_turns -> concurrent_resolution_of_one_source_returns_the_same_pinned_commit (imports.rs)
- ext_import_cannot_execute -> same name (git.rs)
