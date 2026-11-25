# Phase A test map

Paths below are relative to `crates/sk1ll-core/`.

## Summary

Inline tests left in scope pin on-disk formats only:
`src/sync/ownership.rs` (`.sk1ll-state/index.json` golden JSON, round trip,
malformed rejected; `.sk1ll-placement` marker bytes) and `src/manifest.rs`
(manifest golden, round trip, legacy and malformed rejected). Everything else
moved to `tests/sync_*.rs` (shared fixtures in `tests/sync_common/`).
`tests/sync_simulation.rs` runs two sweeps over 17 operation shapes (8 skill
placements, 7 instructions writes, 2 adopts): a crash at every recorded
failpoint hit, then a restart, and a concurrent editor save at every hit.

| | before (3cc7ee4) | after |
|---|---|---|
| tests in scope | 98 (1 ignored) | 92: 4 inline + 88 in `tests/sync_*.rs` (1 ignored) |
| test-code lines | 3426 inline | 87 inline + 3285 in `tests/sync_*.rs` |
| wall time, scope only (built) | 4 s | 16-19 s (the sweeps take 9-13 s) |

The ignored `sync_plan_no_change` benchmark moved to
`tests/sync_integration.rs`; the command in docs/development.md still finds it.

Line coverage of production lines only (`#[cfg(test)]` items excluded from the
denominator), `cargo llvm-cov --workspace --all-features --lcov`, HOME empty
and XDG unset as for the baseline:

| file | before | after |
|---|---|---|
| `src/sync.rs` | 2180/2379 91.64% | 2191/2379 92.10% |
| `src/sync/ownership.rs` | 340/368 92.39% | 341/368 92.66% |
| `src/sync/unmanaged.rs` | 236/257 91.83% | 241/257 93.77% |
| `src/placement.rs` | 105/117 89.74% | 105/117 89.74% |
| `src/manifest.rs` | 84/89 94.38% | 84/89 94.38% |
| `src/real_path.rs` | 26/29 89.66% | 26/29 89.66% |

Made public: `sync::displaced::prune_with` (was `pub(crate)`), so the
size-cap pruning order is tested through it.

Removed cfg(test)-only helpers: sync's per-thread recovery root
(`displaced::test_root`, `TEST_ROOT`), `with_hook`, `prune_older_than`,
`ownership::save_index`, `HashCache::remember_payload` (`src/hash.rs`).
Kept: the `#[cfg(test)]` fallback in `displaced::root()` that gives each
thread a temp recovery directory when no failpoints dirs are set, because
inline core tests outside this phase (draft, imports, repo and others) still run sync without
isolated dirs. Drop it once they move out.

## `src/sync.rs`

- `a_cached_plan_rereads_only_changed_trees_and_apply_checks_the_real_one` -> `a_cached_plan_rereads_a_changed_placement` (`tests/sync_integration.rs`), `stale_skill_plans_preserve_harness_edits` (`tests/sync_integration.rs`)
- `a_clone_with_the_same_marker_bytes_stays_unmanaged` -> `a_clone_with_the_same_marker_bytes_stays_unmanaged` (`tests/sync_integration.rs`)
- `a_copy_replaced_between_syncs_or_after_an_interrupted_removal_stays_unmanaged` -> `a_copy_replaced_between_syncs_or_after_an_interrupted_removal_stays_unmanaged` (`tests/sync_integration.rs`)
- `a_create_never_replaces_what_appeared_after_the_last_check` -> `an_atomic_save_or_newcomer_after_the_last_check_stays_live` (`tests/sync_simulation.rs`; regular-file newcomer at vacant rule, block and skill link destinations), `an_edit_at_any_skill_placement_step_survives_sync_and_retry` (`tests/sync_simulation.rs`), `an_edit_at_any_instructions_step_survives_sync_and_retry` (`tests/sync_simulation.rs`; `BlockCreate` is now vacant, `BlockAppend` the existing-file case)
- `a_created_block_file_swapped_during_its_removal_goes_back` -> `a_created_block_file_swapped_during_its_removal_goes_back` (`tests/sync_integration.rs`)
- `a_forced_swap_interrupted_after_detach_preserves_the_edited_backup` -> `a_forced_swap_interrupted_after_detach_preserves_the_edited_backup` (`tests/sync_recovery.rs`)
- `a_forced_update_of_an_edited_link_keeps_a_backup` -> `a_forced_update_of_an_edited_link_keeps_a_backup` (`tests/sync_recovery.rs`)
- `a_fresh_process_crash_after_the_backup_rename_preserves_both_saves` -> `process_death_after_detach_preserves_both_editor_saves` (`tests/sync_simulation.rs`)
- `a_hand_edit_conflicts_even_when_it_matches_the_source` -> `a_hand_edit_conflicts_even_when_it_matches_the_source` (`tests/sync_integration.rs`)
- `a_partially_written_index_invalidates_the_plan` -> `a_partially_written_index_invalidates_the_plan` (`tests/sync_index.rs`)
- `a_plan_hashes_each_source_skill_once` -> `one_plan_uses_the_same_source_observation_for_every_harness` (`tests/sync_integration.rs`), `an_edit_at_any_skill_placement_step_survives_sync_and_retry` (`tests/sync_simulation.rs`)
- `a_planted_staging_name_is_never_followed` -> `bookkeeping_writes_never_follow_planted_temp_names` (`tests/sync_index.rs`)
- `a_rule_file_that_isnt_a_plain_file_is_a_conflict` -> `a_rule_file_that_isnt_a_plain_file_is_a_conflict` (`tests/sync_integration.rs`)
- `a_rule_replaced_by_another_file_with_identical_text_is_unmanaged` -> `rule_anchor_keeps_the_old_inode_alive_after_the_live_name_is_removed` (`tests/sync_integration.rs`)
- `a_skill_copies_nested_state_and_detects_edits_to_it` -> `a_skill_copies_nested_state_and_detects_edits_to_it` (`tests/sync_integration.rs`)
- `a_write_racing_the_removal_of_a_created_block_file_is_kept` -> `an_edit_at_any_instructions_step_survives_sync_and_retry` (`tests/sync_simulation.rs`)
- `a_write_to_an_early_file_during_the_final_prune_scan_keeps_the_entry` -> `a_write_to_an_early_file_during_the_final_prune_scan_keeps_the_entry` (`tests/sync_recovery.rs`)
- `absent_rule_is_omitted_from_placements_but_removed_from_index` -> `absent_rule_is_omitted_from_placements_but_removed_from_index` (`tests/sync_integration.rs`)
- `adopt_finds_a_renamed_target_by_its_item_id` -> `adopt_finds_a_renamed_target_by_its_item_id` (`tests/sync_adopt.rs`)
- `adopt_owns_only_what_it_copied_and_keeps_what_it_left_out` -> `adopt_owns_only_what_it_copied_and_keeps_what_it_left_out` (`tests/sync_adopt.rs`)
- `adopt_turns_harness_edits_into_draft` -> `adopt_turns_harness_edits_into_draft` (`tests/sync_adopt.rs`)
- `adopting_a_block_entry_is_refused_while_the_managed_block_is_edited_by_hand` -> `adopting_a_block_entry_is_refused_while_the_managed_block_is_edited_by_hand` (`tests/sync_recovery.rs`)
- `adopting_a_filtered_import_copy_keeps_upstream_only_files` -> `adopting_a_filtered_import_copy_keeps_upstream_only_files` (`tests/sync_adopt.rs`)
- `adopting_a_rule_swaps_the_repo_rule_into_recovery` -> `a_crash_at_any_adopt_step_keeps_both_versions_and_allows_retry` (`tests/sync_simulation.rs`), `an_edit_at_any_adopt_step_survives_adopt_and_retry` (`tests/sync_simulation.rs`)
- `adopting_refuses_symlinked_repo_content_parents` -> `adopting_refuses_symlinked_repo_content_parents` (`tests/sync_adopt.rs`)
- `adopting_swaps_the_repo_version_into_recovery` -> `a_crash_at_any_adopt_step_keeps_both_versions_and_allows_retry` (`tests/sync_simulation.rs`), `an_edit_at_any_adopt_step_survives_adopt_and_retry` (`tests/sync_simulation.rs`)
- `an_adoption_interrupted_mid_swap_keeps_the_repo_version_in_recovery` -> `a_crash_at_any_adopt_step_keeps_both_versions_and_allows_retry` (`tests/sync_simulation.rs`; at `AfterRename`: the entry is the repo's, for the repo path, survives a zero-grace prune with the exact repo bytes, then Adopt is retried)
- `an_atomic_rule_save_after_detach_is_not_overwritten` -> `an_atomic_save_or_newcomer_after_the_last_check_stays_live` (`tests/sync_simulation.rs`; `AfterCheck` and `AfterAside`, exact live bytes and conflict), `process_death_after_detach_preserves_both_editor_saves` (`tests/sync_simulation.rs`)
- `an_emptied_block_file_goes_only_if_sync_created_it_and_nobody_touched_it` -> `an_emptied_block_file_goes_only_if_sync_created_it_and_nobody_touched_it` (`tests/sync_integration.rs`)
- `an_emptied_block_file_replaced_or_rechmodded_by_the_user_stays` -> `an_emptied_block_file_goes_only_if_sync_created_it_and_nobody_touched_it` (`tests/sync_integration.rs`)
- `an_unindexed_copy_is_unmanaged_and_never_removed` -> `an_unindexed_copy_is_unmanaged_and_never_removed` (`tests/sync_integration.rs`)
- `an_unindexed_link_is_unmanaged_until_sync_records_it` -> `handmade_repo_links_are_unmanaged_unless_exactly_assigned` (`tests/sync_integration.rs`)
- `assignment_selects_skills_and_rules` -> `assignment_selects_skills_and_rules` (`tests/sync_integration.rs`)
- `bookkeeping_honors_umask_and_preserves_replacements` -> `bookkeeping_honors_umask_and_preserves_replacements` (`tests/sync_index.rs`)
- `bookkeeping_writes_never_follow_a_planted_temp_name` -> `bookkeeping_writes_never_follow_planted_temp_names` (`tests/sync_index.rs`)
- `copied_and_adopted_skills_preserve_symlinks` -> `copied_and_adopted_skills_preserve_symlinks` (`tests/sync_integration.rs`)
- `crash_leftovers_are_ignored_by_planning_and_recovered` -> `crash_leftovers_are_ignored_by_planning_and_recovered` (`tests/sync_recovery.rs`)
- `displaced_content_is_pruned_after_the_grace_period_unless_it_changed` -> `displaced_content_is_pruned_after_the_grace_period_unless_it_changed` (`tests/sync_recovery.rs`)
- `displaced_content_never_goes_back_over_what_appeared_meanwhile` -> `displaced_content_never_goes_back_over_what_appeared_meanwhile` (`tests/sync_recovery.rs`)
- `displaced_content_over_the_size_cap_goes_oldest_unchanged_first` -> `displaced_content_over_the_size_cap_goes_oldest_unchanged_first` (`tests/sync_recovery.rs`)
- `displacing_fails_closed_if_the_move_crosses_filesystems` -> `displacing_fails_closed_if_the_move_crosses_filesystems` (`tests/sync_recovery.rs`)
- `edits_since_placement_conflict_and_forced_swaps_keep_the_backup` -> `edits_since_placement_conflict_and_forced_swaps_keep_the_backup` (`tests/sync_recovery.rs`)
- `handmade_repo_links_are_unmanaged_unless_exactly_assigned` -> `handmade_repo_links_are_unmanaged_unless_exactly_assigned` (`tests/sync_integration.rs`)
- `harness_edit_while_applying_is_never_overwritten` -> `an_edit_at_any_skill_placement_step_survives_sync_and_retry` (`tests/sync_simulation.rs`)
- `harness_edit_while_planning_a_refresh_is_never_overwritten` -> `an_edit_at_any_skill_placement_step_survives_sync_and_retry` (`tests/sync_simulation.rs`)
- `indexed_links_are_removed_when_repo_skill_is_deleted` -> `indexed_links_are_removed_when_repo_skill_is_deleted` (`tests/sync_integration.rs`)
- `instruction_edit_while_applying_is_never_overwritten` -> `an_edit_at_any_instructions_step_survives_sync_and_retry` (`tests/sync_simulation.rs`)
- `late_writes_after_sync_reach_the_recovery_copy_wherever_the_state_dir_is` -> `late_writes_after_sync_reach_the_recovery_copy` (`tests/sync_recovery.rs`), `displacing_fails_closed_if_the_move_crosses_filesystems` (`tests/sync_recovery.rs`)
- `links_to_another_repo_directory_conflict_even_with_force` -> `links_to_another_repo_directory_conflict_even_with_force` (`tests/sync_integration.rs`)
- `migrated_relative_links_are_removed_after_source_deletion_but_unindexed_links_remain` -> `migrated_relative_links_are_removed_after_source_deletion_but_unindexed_links_remain` (`tests/sync_integration.rs`)
- `missing_ids_use_defaults_and_reconcile_without_replacing_bytes` -> `missing_ids_use_defaults_and_reconcile_without_replacing_bytes` (`tests/sync_integration.rs`)
- `ownership_index_round_trips_each_kind` -> `ownership_index_round_trips_and_rejects_unknown_formats` (`src/sync/ownership.rs`)
- `pruning_never_deletes_recursively` -> `pruning_never_deletes_recursively` (`tests/sync_recovery.rs`)
- `pruning_rechecks_queued_entries_and_includes_git_and_modes` -> `pruning_rechecks_queued_entries_and_includes_git_and_modes` (`tests/sync_recovery.rs`)
- `publishing_does_not_restore_an_index_entry_removed_during_the_swap` -> `publishing_does_not_restore_an_index_entry_removed_during_the_swap` (`tests/sync_index.rs`)
- `recording_an_exact_link_preserves_an_intervening_index_edit` -> `recording_an_exact_link_preserves_an_intervening_index_edit` (`tests/sync_index.rs`)
- `recover_adopt_keeps_an_entry_whose_copy_left_something_out` -> `recover_adopt_keeps_an_entry_whose_copy_left_something_out` (`tests/sync_recovery.rs`)
- `recovery_entries_are_found_wherever_they_are` -> `recovery_entries_are_found_wherever_they_are` (`tests/sync_recovery.rs`)
- `recovery_follows_a_rename_and_refuses_a_foreign_repo_or_reused_id` -> `recovery_follows_a_rename_and_refuses_a_foreign_repo_or_reused_id` (`tests/sync_recovery.rs`)
- `recovery_recreates_a_deleted_rule_with_its_placed_id` -> `recovery_recreates_a_deleted_rule_with_its_placed_id` (`tests/sync_recovery.rs`)
- `recovery_refuses_a_symlinked_scratch_root` -> `recovery_refuses_a_symlinked_scratch_root` (`tests/sync_recovery.rs`)
- `repo_link_with_parent_escape_is_unmanaged` -> `repo_link_with_parent_escape_is_unmanaged` (`tests/sync_integration.rs`)
- `restart_finishes_the_first_copy_an_update_and_a_copy_to_link` -> `a_crash_at_any_skill_placement_step_keeps_edits_and_allows_retry` (`tests/sync_simulation.rs`)
- `restart_resolves_a_rule_placement_before_cleaning_stages` -> `a_crash_at_any_instructions_step_keeps_edits_and_allows_retry` (`tests/sync_simulation.rs`), `crash_leftovers_are_ignored_by_planning_and_recovered` (`tests/sync_recovery.rs`)
- `reviewed_copies_reject_source_edits_before_and_during_staging` -> `reviewed_copies_reject_source_edits_before_and_during_staging` (`tests/sync_integration.rs`)
- `rewriting_an_owned_block_leaves_no_recovery_entry` -> `rewriting_an_owned_block_leaves_no_recovery_entry` (`tests/sync_recovery.rs`)
- `rule_adopt_keeps_the_repo_id_and_routing_with_flow_frontmatter` -> `rule_adopt_keeps_the_repo_id_and_routing_with_flow_frontmatter` (`tests/sync_adopt.rs`)
- `rule_anchor_keeps_the_old_inode_alive_after_the_live_name_is_removed` -> `rule_anchor_keeps_the_old_inode_alive_after_the_live_name_is_removed` (`tests/sync_integration.rs`)
- `sizes_take_binary_units` -> `recovery_size_limits_accept_binary_units_and_fall_back_for_invalid_values` (`tests/sync_recovery.rs`)
- `skill_id_is_ignored_in_hashes_and_copies_but_visible_through_links` -> `skill_id_is_ignored_in_hashes_and_copies_but_visible_through_links` (`tests/sync_integration.rs`)
- `source_placement_marker_is_a_conflict_even_with_force` -> `source_placement_marker_is_a_conflict_even_with_force` (`tests/sync_integration.rs`)
- `stale_index_entries_refuse_skill_and_rule_updates` -> `stale_index_entries_refuse_skill_and_rule_updates` (`tests/sync_index.rs`)
- `stale_skill_plan_preserves_retargeted_link` -> `stale_skill_plan_preserves_retargeted_link` (`tests/sync_integration.rs`)
- `stale_skill_plans_preserve_harness_edits` -> `stale_skill_plans_preserve_harness_edits` (`tests/sync_integration.rs`)
- `sync_plan_no_change` -> `sync_plan_no_change` (`tests/sync_integration.rs`)
- `the_harness_fingerprint_follows_what_planning_reads` -> `the_harness_fingerprint_follows_what_planning_reads` (`tests/sync_integration.rs`)
- `the_harness_fingerprint_reads_through_links_and_covers_settings_and_recovery` -> `the_harness_fingerprint_reads_through_links_and_covers_settings_and_recovery` (`tests/sync_integration.rs`)
- `the_recovery_stamp_covers_payloads_beside_instructions` -> `the_recovery_stamp_covers_payloads_beside_instructions` (`tests/sync_recovery.rs`)
- `toggles_follow_item_ids_through_renames_and_recreation` -> `toggles_follow_item_ids_through_renames_and_recreation` (`tests/sync_integration.rs`)
- `unsupported_copy_entries_leave_existing_content_untouched` -> `unsupported_copy_entries_leave_existing_content_untouched` (`tests/sync_integration.rs`)
- `writers_from_independent_repos_serialize_in_one_harness_directory` -> `writers_from_independent_repos_serialize_in_one_harness_directory` (`tests/sync_recovery.rs`)
- `writers_with_open_skill_files_never_lose_edits` -> `an_edit_at_any_skill_placement_step_survives_sync_and_retry` (`tests/sync_simulation.rs`), `adopting_recovered_skill_bytes_keeps_harness_only_sidecars` (`tests/sync_recovery.rs`)
- `writes_to_instruction_files_during_the_swap_are_never_lost` -> `an_edit_at_any_instructions_step_survives_sync_and_retry` (`tests/sync_simulation.rs`), `a_late_instruction_edit_can_be_adopted_from_recovery` (`tests/sync_recovery.rs`)

## `src/sync/ownership.rs`

- `a_write_through_an_open_index_descriptor_keeps_its_archived_version` -> `an_open_index_descriptor_keeps_late_writes_in_the_archive` (`tests/sync_index.rs`)
- `an_index_edit_after_validation_survives_replacement_and_removal` -> `an_index_save_preserves_an_editor_replacement` (`tests/sync_index.rs`)
- `an_interrupted_index_publication_keeps_the_previous_index_readable` -> `recovery_can_read_an_index_whose_publication_was_interrupted` (`tests/sync_index.rs`)
- `independent_opens_contend_on_the_same_directory_lock` -> `writers_from_independent_repos_serialize_in_one_harness_directory` (`tests/sync_recovery.rs`)

## `src/sync/unmanaged.rs`

- `a_user_marker_is_refused_before_grouping_and_after_selection` -> `a_user_marker_is_refused_before_grouping_and_after_selection` (`tests/sync_unmanaged.rs`)
- `adopting_never_overwrites_repo_content_created_while_copying` -> `adopting_never_overwrites_repo_content_created_while_copying` (`tests/sync_unmanaged.rs`)
- `adopts_only_chosen_variant_preserves_originals_in_recovery_and_respects_defaults` -> `adopts_only_chosen_variant_preserves_originals_in_recovery_and_respects_defaults` (`tests/sync_unmanaged.rs`)
- `edits_after_prepare_remain_unmanaged` -> `edits_after_prepare_remain_unmanaged` (`tests/sync_unmanaged.rs`)
- `groups_identical_copies_and_excludes_managed_skills_and_rules` -> `groups_identical_copies_and_excludes_managed_skills_and_rules` (`tests/sync_unmanaged.rs`)
- `only_a_skill_added_in_the_draft_counts_as_adopted` -> `only_a_skill_added_in_the_draft_counts_as_adopted` (`tests/sync_unmanaged.rs`)
- `only_the_variant_matching_the_personal_copy_is_resumable` -> `only_the_variant_matching_the_personal_copy_is_resumable` (`tests/sync_unmanaged.rs`)
- `unmanaged_directory_links_are_hashed_by_content_and_adopted_without_touching_the_target` -> `unmanaged_directory_links_are_hashed_by_content_and_adopted_without_touching_the_target` (`tests/sync_unmanaged.rs`)

## `src/placement.rs`

- `detach_never_replaces_an_occupied_slot` -> `rename_never_replaces_an_occupied_slot` (`tests/sync_index.rs`)

## `src/manifest.rs`

- `old_manifest_explains_what_moved`: kept as a format test, made table-driven, plus `manifest_imports_round_trip_and_default_to_the_source_root_and_head` (golden + round trip).

## `src/real_path.rs`

- `every_name_of_a_file_has_one_key_and_nothing_is_created` -> `every_name_of_a_file_has_one_key_and_nothing_is_created` (`tests/sync_integration.rs`)
