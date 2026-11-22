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
| Stable same-provider candidate order with fresh headroom | `rotation_order_uses_fresh_same_provider_allowances` and `a_new_mission_rotates_once_to_the_first_authorized_alternate` |
| Both provider fixtures latch and stop when no alternate is eligible | `provider_limit_stops_without_replaying_mission` |
| Codex readback uses the actual window lengths and has a restart-safe five-minute floor | `codex_profile_probe_is_paced_across_reopen_and_maps_window_lengths` |
| Profile choice, both allowances and latch render in the profile panel | `usage_panel_shows_selected_allowances_and_latch_with_keyboard_and_mouse` |
| A limited mission is checkpointed and continues once; a reread claim is refused, and the record omits raw task text and provider output | `interrupted_mission_checkpoints_once_and_continues_on_the_next_profile` |
| A workspace change after checkpoint creation prevents the next CLI from starting | `changed_workspace_refuses_the_cross_profile_continuation` |
| The continuation receipt identifies both profiles at 80×24 and 120×40, including a repeat mouse click | `continuation_receipt_is_visible_after_keyboard_and_mouse_submission` |
| An observed success result prevents a continuation even if the stream also contains a limit | `a_completed_mission_never_creates_a_continuation` |
