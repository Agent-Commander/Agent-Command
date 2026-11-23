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
| A decimal speed survives seal, serialization, reopening and digest verification for 999 samples (`float_roundtrip`) | `decimal_speeds_survive_seal_serialize_reopen_and_verify` |
| Four-page free-tier `models/free` and two-page `agents/coding` fixtures publish rows with a null coding index and a present intelligence index, keyed by published identity, from the `/api/v2/language/...` paths | `free_tier_pages_publish_both_endpoints_with_exact_identities` |
| Exact identities: dots to hyphens for Codex, verbatim Claude names, suffixed explicit efforts, base row for no effort; other versions, efforts, creators, `max`/`minimal`/`ultra`, aliases and absent models never borrow a row | `exact_identities_never_borrow_another_version_effort_or_creator`, `exact_identities_derive_from_native_options_without_a_model_table` |
| Routers, dated releases, API-gateway providers, other or crossed agent and model authors are skipped; repeated identities are skipped as ambiguous | `agent_identity_requires_exact_harness_creator_and_release`, `ambiguous_identities_are_skipped_instead_of_chosen` |
| A free-plan 403 on optional `agents/coding` keeps its cache and never suppresses `models/free` or fails the refresh; 401, 429 and 410 keep both | `agents_coding_refusal_never_suppresses_models_free` |
| Malformed pagination, `tier`, index version, row shape or matched metric rejects only that endpoint and keeps its cache | `malformed_pagination_or_metrics_reject_only_that_endpoint` |
| A refresh with no supported rows, or an empty result, never erases a valid cache | `empty_supported_refresh_keeps_a_valid_cache` |
| Deprecated `performance.median_output_tokens_per_second` and the retired field names are never read as measurements | `legacy_and_retired_fields_are_never_read_as_measurements` |
| Refresh redacts the key and stays on the published HTTPS origin | `refresh_redacts_the_key_and_stays_on_the_published_origin` |
| `curl` starts with `-q`, gets the key only on standard input, and does not inherit the key or `CURL_HOME` | `curl_reads_no_implicit_config_and_inherits_no_key` |
| Without a key, `catalog refresh` refuses and `catalog show` reports bundled provenance | `catalog_entry_point_shows_bundled_provenance_and_refuses_refresh_without_key` |
| Latched, exhausted, corrupt and sign-in-refused profiles are excluded; stale, unreported and past-reset readings are not | `routing_gate_excludes_latched_exhausted_corrupt_and_refused_profiles` |
| Hard and standard floors never pick a below-floor or unscored model and name the metric in the reason | `difficulty_floor_never_selects_a_below_floor_or_unscored_model` |
| Keyless `/standard` and `/hard` fail closed with a visible reason; floors are 40 and 55 | `keyless_standard_and_hard_fail_closed_with_a_visible_reason` |
| One metric ranks every candidate (coding index, otherwise intelligence index) and missing values stay null | `one_metric_ranks_every_candidate_and_missing_values_stay_null` |
| A coding agent index never outranks or blocks a coding or intelligence index, and alone leaves the ranking unscored | `coding_index_ranks_ahead_of_an_independent_coding_agent_index`, `intelligence_index_ranks_ahead_of_an_independent_coding_agent_index`, `coding_agent_index_alone_never_scores_a_ranking` |
| Effort-suffixed model names are benchmark variants: refused as pins, never dispatched as `--model`, never matched as a default row, and a cold start with one fails closed | `effort_suffixed_names_are_benchmark_variants_not_native_models`, `configurations_produce_native_execution_arguments`, `exact_identities_never_borrow_another_version_effort_or_creator`, `cold_start_with_an_effort_suffixed_pin_fails_closed`, `model_pins_round_trip_through_versioned_state_and_refuse_aliases` |
| Ordinary policy ranks by the minimum headroom-to-reset rate, with stable ties and unknown headroom last | `ordinary_policy_ranks_by_soonest_expiring_headroom_with_stable_ties` |
| Urgent policy ranks the fastest same-unit measurement first, then headroom; input order does not matter | `urgent_policy_puts_fastest_same_unit_first_and_breaks_ties_by_headroom` |
| Split allocation by headroom, ties and unknown headroom | `split_units_spread_by_headroom_without_ineligible_or_repeated_profiles` |
| Split directives require explicit units | `split_requests_need_explicit_independent_units` |
| Split units run once each, in order, with no concurrent workspace writer and no ineligible profile | `split_mission_sends_each_unit_once_in_sequence_to_allocated_profiles` |
| Native Claude/Codex arguments carry the pinned model and effort (`--model`/`--effort`, `-c model=…`/`-c model_reasoning_effort=…`), the reason names the metric, and a floor miss starts nothing | `routed_dispatch_passes_selected_model_and_effort_to_native_cli`, `configurations_produce_native_execution_arguments` |
| A usage-limit continuation keeps the exact pinned model and effort on the alternate | `pinned_continuation_keeps_the_exact_model_and_effort` |
| Model pins persist in versioned state, accept native efforts and refuse aliases and unknown efforts | `pinned_models_persist_in_versioned_state`, `pins_accept_native_efforts_and_refuse_aliases`, `model_pins_round_trip_through_versioned_state_and_refuse_aliases` |
| The pinned model and the metric-named routing reason are visible with keyboard and with mouse at both 80×24 and 120×40, the real CLI arguments carry the pin, and an invalid pins file shows a notice | `pinned_model_and_ranking_metric_are_visible_with_keyboard_and_mouse` |
| Override persistence, rotation and fallback reasons | `override_persists_and_falls_back_with_visible_reason` |
| The override survives reopening, and the latch fallback and read-error reasons are visible with keyboard and with mouse at both 80×24 and 120×40 | `override_persists_across_reopen_and_shows_fallback_reason_with_keyboard_and_mouse` |
| The profile panel cycles with keyboard and with mouse at both 80×24 and 120×40 | `usage_panel_shows_selected_allowances_and_latch_with_keyboard_and_mouse` |
| Real terminal: at 80×24, then again at 120×40 in a new process that restores the saved override, F2 cycling and then mouse cycling | `sh tests/terminal_full_profile.sh` |
