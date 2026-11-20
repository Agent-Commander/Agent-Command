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
