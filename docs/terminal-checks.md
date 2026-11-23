# Terminal layout checks

`tests/terminal_layout.rs` uses Ratatui TestBackend and the real App key/click handlers.

| Behavior | Check |
| --- | --- |
| All four crew names and selection at 80×24 and 120×40 | `regions_and_equivalent_controls_at_supported_sizes` |
| Selected mission/status in chart independently of feed | Same test clears feed, inserts a unique log marker, then checks separate rectangles |
| Pause/Resume and Send visible, clickable and in bounds | Same test clicks centers and compares selected state, missions and log against keyboard actions |
| Input and recent log visible in their own regions | Same test |
| Complete crew helper within its region at both sizes | `crew_helper_is_complete_inside_its_region` |
| Header labels separated | Same test asserts explicit separator in header row |
| Selection, Unicode mission/draft, pause, feed and edit mode survive resize | `resize_retains_unicode_and_selected_state`, 120×40 → 80×24 → 120×40 |

The compact layout omits decorative summary rows, uses two-line crew rows and reserves mission-button width. Paragraphs stay bounded to their assigned regions. Longer content may truncate to available space. No minimum-size increase or glyph substitution is made.

These checks do not establish physical keyboard/IME delivery, terminal font coverage, live terminal restoration or cross-platform acceptance. Independent Tester acceptance and Screen Driver rerun remain required.

| Subscription usage scenario | Executable check |
|---|---|
| Limit survives reopening and expires at the provider reset | `forced_latch_survives_reload_and_never_shortens` |
| Stable same-provider candidate order with stale or unreported allowances | `rotation_order_accepts_stale_and_unreported_same_provider_allowances` and `a_new_mission_rotates_once_to_the_first_authorized_alternate` |
| Both provider fixtures latch and stop when no alternate is eligible | `provider_limit_stops_without_replaying_mission` |
| Codex weekly-only readback shows a current weekly window and an unavailable five-hour window | `codex_weekly_only_readback_is_current_and_eligible` |
| Codex readback uses the actual window lengths and has a restart-safe five-minute floor | `codex_profile_probe_is_paced_across_reopen_and_maps_window_lengths` |
| Profile choice, both allowances and latch render in the profile panel | `usage_panel_shows_selected_allowances_and_latch_with_keyboard_and_mouse` |
| A limited mission is checkpointed and continues once; a reread claim is refused, and the record omits raw task text and provider output | `stale_weekly_only_alternate_continues_once` |
| A workspace change after checkpoint creation prevents the next CLI from starting | `changed_workspace_refuses_the_cross_profile_continuation` |
| The continuation receipt identifies both profiles at 80×24 and 120×40, including a repeat mouse click | `continuation_receipt_is_visible_after_keyboard_and_mouse_submission` |
| An observed success result prevents a continuation even if the stream also contains a limit | `a_completed_mission_never_creates_a_continuation` |

| Stage 2 rework | Executable check |
|---|---|
| A backward wall-clock jump makes a future observation stale with unknown age | `backward_wall_clock_never_makes_a_future_observation_fresh` |
| A past limit expires and a valid future limit replaces an unknown limit | `expired_limit_needs_fresh_usage_and_future_limits_repair_unknown` |
| A failed Codex readback cannot be retried early after a forward wall-clock jump or reopening | `failed_codex_probe_respects_monotonic_backoff_after_clock_jump_and_reopen` |
| A 64-character profile, unreported five-hour window, and stale weekly usage remain visible at both sizes in a real terminal | `sh tests/terminal_full_profile.sh` after building, with tmux installed |
| A separate process displays a pending claim and can resume once or abandon it | `separate_process_shows_and_resumes_pending_claim_once_or_abandons_it` |
| A rejected continuation latches the alternate and leaves the third eligible profile idle | `rejected_unobserved_continuation_latches_and_does_not_try_third_profile` |
| An alternate with active latch, exhausted window, or corrupt state never starts | `exhausted_latched_or_corrupt_alternates_never_start` |
| Malformed usage state prevents selection and polling | `corrupt_usage_fails_closed_before_poll_or_selection` |

| Continuation manifest case | Executable check |
|---|---|
| Index edit survives restoring the working copy to HEAD | `staged_edit_survives_worktree_restore_in_manifest_and_digest` |
| Staged deletion carries the path and two absent hashes | `staged_deletion_shows_both_states_absent` |
| Staged rename carries the old and new paths | `staged_rename_records_removed_and_added_paths` |
| Intent to add carries a working-copy hash and no staged blob | `intent_to_add_records_worktree_without_an_index_blob` |
| Working-copy edit carries the base blob and edited file hashes | `worktree_only_edit_reports_head_blob_and_worktree_hash` |
| Non-UTF-8 path bytes and a staged object-type change are preserved | `non_utf8_paths_round_trip_as_hex` and `staged_type_change_hashes_link_blob_without_following_it` |

| Unsupported workspace entry | Executable check |
|---|---|
| Nested Git repository and mutation after checkpointing on the prior behavior | `nested_repository_mutation_cannot_authorize_a_reopened_claim` |
| Nested repository added after a valid claim | `reopened_claim_refuses_a_new_nested_repository` |
| Terminal directory, FIFO, Unix socket, or device | `terminal_directory_is_not_a_missing_worktree_file`, `fifo_omitted_by_git_is_refused_by_path`, `unix_socket_omitted_by_git_is_refused_by_path`, `character_device_is_refused_by_path_before_open` |
| Unreadable file, staged gitlink, or working symlink | `unreadable_regular_file_is_refused_by_path`, `staged_gitlink_is_refused_by_path`, `working_symlink_is_refused_by_path` |

| Stage 3 catalog and routing | Executable check |
|---|---|
| Bundled snapshots are dated, sourced, and contain no unverified values | `bundled_snapshots_are_dated_sourced_and_carry_no_unverified_values` |
| Per-endpoint SHA-256, scope, schema, version, range, unit and size checks fall back independently | `cache_integrity_falls_back_independently_per_endpoint` |
| Refresh uses an injected fetcher, enforces the same HTTPS origin, pagination and version, redacts the key, and publishes only validated endpoints | `refresh_publishes_only_fully_validated_same_origin_pages` |
| Without a key, `catalog refresh` refuses and `catalog show` reports bundled provenance | `catalog_entry_point_shows_bundled_provenance_and_refuses_refresh_without_key` |
| Latched, exhausted, corrupt and sign-in-refused profiles are excluded; stale, unreported and past-reset readings are not | `routing_gate_excludes_latched_exhausted_corrupt_and_refused_profiles` |
| Hard and standard floors never pick a below-floor or unscored model and give the reason | `difficulty_floor_never_selects_a_below_floor_or_unscored_model` |
| Ordinary policy ranks by the minimum headroom-to-reset rate, with stable ties and unknown headroom last | `ordinary_policy_ranks_by_soonest_expiring_headroom_with_stable_ties` |
| Urgent policy ranks the fastest same-unit measurement first, then headroom; input order does not matter | `urgent_policy_puts_fastest_same_unit_first_and_breaks_ties_by_headroom` |
| Default policy prefers the comparable exact coding index | `default_policy_prefers_comparable_exact_coding_index` |
| Split allocation by headroom, ties and unknown headroom | `split_units_spread_by_headroom_without_ineligible_or_repeated_profiles` |
| Split directives require explicit units | `split_requests_need_explicit_independent_units` |
| Split units run once each, in order, with no concurrent workspace writer and no ineligible profile | `split_mission_sends_each_unit_once_in_sequence_to_allocated_profiles` |
| Native Claude/Codex arguments carry the routed model and effort; a floor miss starts nothing | `routed_dispatch_passes_selected_model_and_effort_to_native_cli`, `configurations_produce_native_execution_arguments` |
| Override persistence, rotation and fallback reasons | `override_persists_and_falls_back_with_visible_reason` |
| The override survives reopening, and the latch fallback and read-error reasons are visible with keyboard at 80×24 and mouse at 120×40 | `override_persists_across_reopen_and_shows_fallback_reason_with_keyboard_and_mouse` |
| Real terminal: Auto, then the override with F2 at 80×24, persisted across a new process, then mouse cycling at 120×40 | `sh tests/terminal_full_profile.sh` |
