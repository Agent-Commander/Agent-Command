use std::{env, fs, path::PathBuf};

use agent_command::{
    auth::{Profile, Provider},
    markdown::{ToolKind, TranscriptEntry},
    tui::{
        graphics::{GraphicsMode, InlineProtocol},
        tabs::{VaultModalMode, VaultModalState},
    },
    update::{self, UpdateState},
    vault::VaultMeta,
    App,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, style::Color, Terminal};

fn find_in_buffer(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let width = needle.chars().count();
    if width == 0 {
        return None;
    }
    (0..buffer.area.height).find_map(|y| {
        let symbols: Vec<&str> = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        (0..=symbols.len().saturating_sub(width))
            .find_map(|x| (symbols[x..x + width].concat() == needle).then_some((x as u16, y)))
    })
}

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.key(KeyEvent::new(code, modifiers));
}

fn text(buffer: &Buffer, area: Rect) -> String {
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    terminal.backend().buffer().clone()
}

fn click_center(app: &mut App, area: Rect, w: u16, h: u16) {
    assert!(
        area.width > 0 && area.height > 0,
        "control must be clickable"
    );
    assert!(
        area.right() <= w && area.bottom() <= h,
        "control must be in bounds"
    );
    app.click(area.x + area.width / 2, area.y + area.height / 2);
}

#[test]
fn single_canvas_renders_cleanly_at_supported_and_narrow_sizes() {
    for (w, h) in [(60, 20), (80, 24), (120, 40)] {
        let mut app = App::default();
        let buffer = render(&mut app, w, h);
        let top = text(&buffer, Rect::new(0, 0, w, 1));
        assert!(top.contains("agent-command"), "{w}x{h}: {top}");
        assert!(!top.contains("PRIVATEER"), "{w}x{h}: {top}");
        assert!(!top.contains("Ship's Log"), "{w}x{h}: {top}");
        assert!(app.settings_area.width > 0 && app.settings_area.right() <= w);
        assert!(app.palette_area.width > 0 && app.palette_area.right() <= w);
        assert!(app.input_area.width > 0 && app.input_area.bottom() < h);
        assert_eq!(app.update_area, Rect::default(), "{w}x{h}");
        assert!(!text(&buffer, Rect::new(0, h - 1, w, 1)).contains("Update"));
        assert!(app.editing);
        if w >= 100 {
            assert!(app.crew_area.width > 0);
            let whole = text(&buffer, Rect::new(0, 0, w, h));
            assert!(whole.contains("WORKERS"), "{whole}");
            assert!(whole.contains("CONNECTIONS"), "{whole}");
        } else {
            assert_eq!(app.crew_area.width, 0);
        }
    }
}

#[test]
fn right_rail_toggles_and_mouse_keyboard_select_workers_equivalently() {
    let mut app = App::default();
    app.input = "/spawn Nova reviewer".into();
    app.submit();
    assert_eq!(app.crew.len(), 2);
    assert_eq!(app.selected, 1);

    let buffer = render(&mut app, 120, 40);
    let whole = text(&buffer, Rect::new(0, 0, 120, 40));
    assert!(whole.contains("Nova"));
    assert!(whole.contains("Focused: Nova"));

    key(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(!app.rail_open);
    let _ = render(&mut app, 120, 40);
    assert_eq!(app.crew_area.width, 0);

    key(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(app.rail_open);
    let _ = render(&mut app, 120, 40);
    assert!(app.crew_area.width > 0);

    app.click(app.crew_area.x + 1, app.crew_area.y);
    assert_eq!(app.selected, 0);

    key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert!(app.rail_focused);
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.selected, 1);
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.rail_focused);
    assert!(app.editing);
}

#[test]
fn resize_retains_unicode_multiline_draft_and_paste_chips() {
    let mut app = App::default();
    for ch in "Café 航海 🚀".chars() {
        key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    for ch in "line two λ".chars() {
        key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut app, KeyCode::Char('\x7f'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('λ'), KeyModifiers::NONE);
    assert_eq!(app.composer.text(), "Café 航海 🚀\nline two λ");

    let big_paste = (1..=14)
        .map(|i| format!("row {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.paste(&big_paste);
    assert!(app.composer.text().contains("[Pasted text #1 (14 lines)]"));
    assert_eq!(app.composer.pasted_chunks().len(), 1);

    for (w, h) in [(120, 40), (80, 24), (60, 20), (120, 40)] {
        let buffer = render(&mut app, w, h);
        let composer_view = text(&buffer, app.input_area);
        assert!(
            composer_view.contains("Pasted text #1") || composer_view.contains("Café"),
            "{w}x{h}:\n{composer_view}"
        );
    }
    let expanded = app.composer.take_submission().unwrap();
    assert!(expanded.contains("Café 航海 🚀\nline two λ"));
    assert!(expanded.contains("row 14"));
}

#[test]
fn slash_help_and_unknown_typo_guard_never_dispatch_to_llm() {
    let mut app = App::default();
    for ch in "/help".chars() {
        key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let buffer = render(&mut app, 120, 40);
    let body = text(&buffer, app.transcript_area);
    assert!(body.contains("Shortcuts"), "{body}");
    key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    let top_body = text(&render(&mut app, 120, 40), app.transcript_area);
    assert!(
        top_body.contains("Slash Commands") && top_body.contains("/spawn"),
        "{top_body}"
    );

    let before_log = app.log.len();
    for ch in "/spwn".chars() {
        key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.log.len(), before_log);
    assert!(
        app.notice.contains("Unknown command") && app.notice.contains("/spawn"),
        "{}",
        app.notice
    );
}

#[test]
fn transcript_scrolling_and_sticky_bottom_follow_work() {
    let mut app = App::default();
    for i in 0..25 {
        app.transcript.push_user(format!("Message {i}"), None);
        app.transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Commander".into(),
            markdown: format!("Reply **{i}**\n\n```rust\nlet x = {i};\n```"),
            streaming: false,
        });
    }
    let _ = render(&mut app, 80, 24);
    assert!(app.scroll.is_sticky_bottom());
    let bottom = app.scroll.offset();
    assert!(bottom > 0);

    key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(!app.scroll.is_sticky_bottom());
    assert!(app.scroll.offset() < bottom);

    let scrolled_pos = app.scroll.offset();
    app.scroll_mouse(app.transcript_area.x + 2, app.transcript_area.y + 2, -3);
    assert!(app.scroll.offset() <= scrolled_pos);

    app.scroll.jump_to_bottom();
    assert!(app.scroll.is_sticky_bottom());
}

#[test]
fn command_palette_and_diff_viewer_work_via_keyboard_and_mouse() {
    let mut app = App::default();
    let _ = render(&mut app, 120, 40);

    key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert!(app.palette.is_open());
    let buf = render(&mut app, 120, 40);
    assert!(text(&buf, Rect::new(0, 0, 120, 40)).contains("Command Palette"));
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.palette.is_open());
    assert!(app.editing);

    let palette_btn = app.palette_area;
    click_center(&mut app, palette_btn, 120, 40);
    assert!(app.palette.is_open());
    for ch in "diff".chars() {
        key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.palette.is_open());
    assert!(app.diff_modal.is_open());

    let diff_buf = render(&mut app, 80, 24);
    assert!(text(&diff_buf, Rect::new(0, 0, 80, 24)).contains("Unified Diff"));
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.diff_modal.is_open());
    assert!(app.editing);
}

#[test]
fn update_button_is_hidden_unless_a_newer_release_is_detected() {
    let mut app = App::default();
    for state in [
        UpdateState::Idle,
        UpdateState::Checking,
        UpdateState::UpToDate {
            current: update::CURRENT.to_owned(),
        },
        UpdateState::Failed("offline".into()),
    ] {
        app.update_state = state.clone();
        app.update_available = None;
        for (w, h) in [(60, 20), (80, 24), (120, 40), (140, 40)] {
            let buf = render(&mut app, w, h);
            assert_eq!(app.update_area, Rect::default(), "{state:?} {w}x{h}");
            let footer = text(&buf, Rect::new(0, h - 1, w, 1));
            assert!(!footer.contains("Update"), "{state:?} {w}x{h}: {footer}");
            assert!(!footer.contains('↻'), "{state:?} {w}x{h}: {footer}");
            app.notice.clear();
            app.click(w - 2, h - 1);
            assert!(app.notice.is_empty(), "{state:?} {w}x{h}: {}", app.notice);
            assert!(!app.quit);
        }
    }
}

#[test]
fn update_button_is_visible_and_clickable_once_a_newer_release_is_detected() {
    let mut app = App::default();
    let footer_bg = app.ui_palette.surface;
    for (state, expected_narrow, expected_wide) in [
        (
            UpdateState::Downloading { pct: 50 },
            "⬇ Updating 50%",
            "⬇ Updating 50%",
        ),
        (
            UpdateState::Available {
                tag: "v0.9.0".into(),
                current: update::CURRENT.to_owned(),
            },
            "⬆ Update v0.9.0",
            "⬆ Update v0.9.0 (F6)",
        ),
    ] {
        app.update_state = state;
        for (w, h) in [(60, 20), (80, 24), (120, 40), (140, 40)] {
            let buf = render(&mut app, w, h);
            assert!(app.update_area.width > 0);
            assert_eq!(app.update_area.right(), w);
            assert_eq!(app.update_area.y, h - 1);
            let pill = text(&buf, app.update_area);
            let expected = if w >= 100 {
                expected_wide
            } else {
                expected_narrow
            };
            assert_eq!(pill.trim(), expected, "{w}x{h}: {pill}");
            let cell = &buf[(app.update_area.x, app.update_area.y)];
            assert_ne!(cell.bg, footer_bg, "{w}x{h} button must contrast footer bg");
            app.notice.clear();
            let button = app.update_area;
            click_center(&mut app, button, w, h);
            assert!(
                !app.notice.is_empty(),
                "{w}x{h} click must reach the updater"
            );
        }
    }
    app.update_state = UpdateState::Idle;
    app.update_available = Some("9.9.9".into());
    let buf = render(&mut app, 140, 40);
    assert_eq!(text(&buf, app.update_area).trim(), "⬆ Update v9.9.9 (F6)");
}

#[test]
fn ctrl_c_and_ctrl_q_never_trap_and_thinking_tool_cards_expand() {
    let mut app = App::default();
    app.transcript.push(TranscriptEntry::ThinkingBlock {
        text: "Hidden chain of thought".into(),
        duration_ms: 1500,
        collapsed: true,
    });
    app.transcript.push(TranscriptEntry::ToolCallCard {
        call_id: "t1".into(),
        kind: ToolKind::Bash,
        name: "Bash".into(),
        summary: "cargo test".into(),
        output: "all tests ok".into(),
        ok: Some(true),
        exit_code: Some(0),
        duration_ms: Some(400),
        collapsed: true,
    });
    let before = text(&render(&mut app, 100, 30), app.transcript_area);
    assert!(!before.contains("all tests ok"));
    key(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
    let after = text(&render(&mut app, 100, 30), app.transcript_area);
    assert!(after.contains("all tests ok"), "{after}");

    for ch in "some draft".chars() {
        key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!app.quit);
    assert!(app.composer.is_empty());
    key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit);

    let mut app2 = App::default();
    key(&mut app2, KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(app2.quit);
}

#[test]
fn top_bar_settings_has_clean_spacing_bottom_padding_and_empty_thinking_expands() {
    let mut app = App::default();
    app.transcript.clear();
    app.transcript.push(TranscriptEntry::ThinkingBlock {
        text: String::new(),
        duration_ms: 12_700,
        collapsed: true,
    });
    let buf = render(&mut app, 120, 30);
    let top = text(&buf, Rect::new(0, 0, 120, 1));
    assert!(top.contains("⚙  Settings"), "{top}");

    key(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
    let expanded_buf = render(&mut app, 120, 30);
    let body = text(&expanded_buf, app.transcript_area);
    assert!(body.contains("▾ Thought for 12.7s"), "{body}");
    assert!(
        body.contains("(No reasoning text streamed by provider)"),
        "{body}"
    );

    let mut tall = App::default();
    tall.transcript.clear();
    for idx in 0..30 {
        tall.transcript
            .push_user(format!("Prompt line {idx}"), None);
    }
    let tall_buf = render(&mut tall, 100, 20);
    let last_transcript_row = tall.transcript_area.bottom() - 1;
    let bottom_row_text = text(
        &tall_buf,
        Rect::new(
            tall.transcript_area.x,
            last_transcript_row,
            tall.transcript_area.width.saturating_sub(1),
            1,
        ),
    );
    assert!(
        bottom_row_text.trim().is_empty(),
        "expected blank bottom padding row, got: {bottom_row_text:?}"
    );
}

#[test]
fn markdown_table_drag_selection_and_quote_popover_work_at_80x24_and_120x40() {
    for (w, h, use_mouse_quote) in [(80, 24, true), (120, 40, false)] {
        let mut app = App::default();
        app.transcript.clear();
        app.transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Commander".into(),
            markdown: "| Crate | Status | Notes |\n| --- | --- | --- |\n| clipboard | ready | pure rust png |".into(),
            streaming: false,
        });

        let buf = render(&mut app, w, h);
        let needle = "pure rust png";
        let (start_x, row_y) = find_in_buffer(&buf, needle).unwrap_or_else(|| {
            panic!(
                "missing table cell {needle:?} at {w}x{h}:\n{}",
                text(&buf, app.transcript_area)
            )
        });
        let end_x = start_x + needle.chars().count() as u16 - 1;

        app.mouse_down(start_x, row_y);
        app.mouse_drag(end_x, row_y);
        app.mouse_up(end_x, row_y);

        assert!(app.scroll.popover_open(), "{w}x{h} popover must open");
        assert_eq!(
            app.scroll.extract_selection_text().as_deref(),
            Some(needle),
            "{w}x{h} exact table substring must be selected"
        );

        let selected_buf = render(&mut app, w, h);
        assert_eq!(
            selected_buf[(start_x, row_y)].bg,
            app.ui_palette.selection_bg,
            "{w}x{h} highlighted cell must use selection_bg"
        );
        let rects = app.scroll.popover_rects();
        assert!(
            rects.container.width > 0
                && rects.container.height > 0
                && rects.container.right() <= w
                && rects.container.bottom() <= h,
            "{w}x{h} popover out of bounds: {:?}",
            rects.container
        );
        let popover_text = text(&selected_buf, rects.container);
        assert!(
            popover_text.contains("Quote (q)") && popover_text.contains("Copy (c)"),
            "{w}x{h} popover text missing buttons: {popover_text:?}"
        );

        if use_mouse_quote {
            click_center(&mut app, rects.quote, w, h);
        } else {
            key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        }

        assert!(
            !app.scroll.popover_open(),
            "{w}x{h} popover must close after Quote"
        );
        assert!(app.editing, "{w}x{h} Quote must focus composer");
        assert!(
            app.composer.text().contains("> pure rust png"),
            "{w}x{h} composer must contain quoted excerpt, got: {:?}",
            app.composer.text()
        );

        for ch in "confirm".chars() {
            key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert!(
            app.composer.text().ends_with("confirm"),
            "{w}x{h} user must be able to type immediately after Quote: {:?}",
            app.composer.text()
        );
    }
}

#[test]
fn composer_image_chip_and_vault_modal_render_safely_at_80x24_and_120x40() {
    let mut app = App::default();
    app.composer
        .attach_image(PathBuf::from("shot.png"), "image/png", "shot.png");

    for (w, h) in [(80, 24), (120, 40)] {
        let buf = render(&mut app, w, h);
        assert!(app.input_area.right() <= w && app.input_area.bottom() < h);
        let composer_view = text(&buf, app.input_area);
        assert!(
            composer_view.contains("[Image #1 · shot.png]"),
            "{w}x{h} missing image chip:\n{composer_view}"
        );
    }

    app.editing = false;
    app.vault_modal = Some(VaultModalState {
        mode: VaultModalMode::List,
        entries: vec![VaultMeta {
            name: "OPENAI_KEY".into(),
            namespace: "global".into(),
            description: "API token".into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            value_bytes: 32,
            sha256_8: "deadbeef".into(),
        }],
        selected: 0,
        request_id: None,
        name: String::new(),
        namespace: "global".into(),
        description: String::new(),
        secret_input: String::new(),
        error: None,
    });

    for (w, h) in [(80, 24), (120, 40)] {
        let buf = render(&mut app, w, h);
        let screen = text(&buf, Rect::new(0, 0, w, h));
        assert!(
            screen.contains("Vault Secrets (metadata only)"),
            "{w}x{h}:\n{screen}"
        );
        assert!(screen.contains("global/OPENAI_KEY"), "{w}x{h}:\n{screen}");
        assert!(screen.contains("sha256:deadbeef"), "{w}x{h}:\n{screen}");
    }

    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.vault_modal.as_ref().map(|m| m.mode),
        Some(VaultModalMode::PromptSecret)
    );
    let raw_secret = "sk-super-secret-4242";
    for ch in raw_secret.chars() {
        key(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }

    let expected_mask = "•".repeat(raw_secret.chars().count());
    for (w, h) in [(80, 24), (120, 40)] {
        let buf = render(&mut app, w, h);
        let screen = text(&buf, Rect::new(0, 0, w, h));
        assert!(
            screen.contains("Vault · Store Secret"),
            "{w}x{h}:\n{screen}"
        );
        assert!(screen.contains(&expected_mask), "{w}x{h}:\n{screen}");
        assert!(
            !screen.contains(raw_secret)
                && !screen.contains("sk-super")
                && !screen.contains("4242"),
            "{w}x{h} vault modal leaked plaintext secret:\n{screen}"
        );
    }

    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.vault_modal.is_none());
    assert!(app.editing);
}

fn mark_graphics_displayed(app: &mut App) {
    let slots = app.pending_graphic_rings.clone();
    let cell_px = app.graphics.cell_px;
    app.inline_ring_cache
        .diff_and_encode(&slots, InlineProtocol::Sixel, cell_px);
}

#[test]
fn sixel_auto_needs_measured_cell_pixels_before_queuing_rings() {
    let mut app = App::default();
    app.graphics.sixel = true;
    app.graphics.mode = GraphicsMode::Auto;
    assert!(!app.graphics.use_inline_graphics());
    app.graphics.cell_px_measured = true;
    assert!(app.graphics.use_inline_graphics());
}

#[test]
fn sixel_ring_cells_paint_first_then_skip_and_repaint_when_overlays_open() {
    let root = env::temp_dir().join(format!(
        "agent-command-sixel-layout-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let workspace = root.join("work");
    let state = root.join("state");
    fs::create_dir_all(&workspace).unwrap();
    let profile = Profile::new(state.clone(), Provider::Claude, "sixelring").unwrap();
    profile.prepare_isolated_with_env().unwrap();

    let prev_sub_auth = env::var_os("SUB_AUTH_DIR");
    env::set_var("SUB_AUTH_DIR", &state);
    let mut app = App::live(workspace);
    match prev_sub_auth {
        Some(value) => env::set_var("SUB_AUTH_DIR", value),
        None => env::remove_var("SUB_AUTH_DIR"),
    }

    assert!(
        !app.usage_cards().is_empty(),
        "isolated profile must populate left-panel usage cards"
    );

    app.graphics.sixel = false;
    app.graphics.mode = GraphicsMode::Auto;
    let no_sixel_buf = render(&mut app, 120, 40);
    assert!(app.pending_graphic_rings.is_empty());

    app.graphics.sixel = true;
    app.graphics.cell_px_measured = true;
    let _ = render(&mut app, 80, 24);
    assert!(
        app.pending_graphic_rings.is_empty(),
        "narrow 80x24 layout hides left panel and must not queue Sixel rings"
    );

    let wide_buf = render(&mut app, 120, 40);
    assert!(
        !app.pending_graphic_rings.is_empty(),
        "120x40 layout with Sixel enabled must queue ring slots"
    );
    let ring_rect = app.pending_graphic_rings[0].rect;
    assert!(
        app.pending_graphic_rings
            .iter()
            .all(|slot| slot.fits_without_scroll(Rect::new(0, 0, 120, 40))),
        "all Sixel ring slots must fit without scrolling"
    );
    assert_eq!(
        no_sixel_buf[(ring_rect.x, ring_rect.y)].bg,
        app.ui_palette.surface,
        "ring cells must flush surface bg when Sixel is disabled"
    );
    for y in ring_rect.y..ring_rect.bottom() {
        for x in ring_rect.x..ring_rect.right() {
            assert_eq!(
                wide_buf[(x, y)].bg,
                app.ui_palette.surface,
                "newly placed ring cell ({x},{y}) must paint the surface before the graphic"
            );
            assert_eq!(wide_buf[(x, y)].symbol(), " ");
        }
    }
    mark_graphics_displayed(&mut app);
    let steady_buf = render(&mut app, 120, 40);
    for y in ring_rect.y..ring_rect.bottom() {
        for x in ring_rect.x..ring_rect.right() {
            assert_eq!(
                steady_buf[(x, y)].bg,
                Color::Reset,
                "displayed ring cell ({x},{y}) must be skipped on steady-state frames"
            );
        }
    }
    assert_eq!(
        wide_buf[(ring_rect.right(), ring_rect.y)].bg,
        app.ui_palette.surface,
        "left-panel cell next to Sixel ring must still flush normally"
    );

    app.open_diff_viewer(None);
    assert!(app.diff_modal.is_open());
    let occluded_buf = render(&mut app, 120, 40);
    mark_graphics_displayed(&mut app);
    assert!(
        app.pending_graphic_rings.is_empty(),
        "Diff overlay covering left panel must occlude pending Sixel rings"
    );
    for y in ring_rect.y..ring_rect.bottom() {
        for x in ring_rect.x..ring_rect.right() {
            assert_ne!(
                occluded_buf[(x, y)].bg,
                Color::Reset,
                "occluded ring cell ({x},{y}) must flush normally under Diff overlay"
            );
        }
    }

    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.diff_modal.is_open());
    let resumed_buf = render(&mut app, 120, 40);
    assert!(
        !app.pending_graphic_rings.is_empty(),
        "Sixel rings must resume once Diff overlay closes"
    );
    assert_eq!(
        resumed_buf[(ring_rect.x, ring_rect.y)].bg,
        app.ui_palette.surface,
        "uncovered ring must repaint the surface before the graphic returns"
    );
    mark_graphics_displayed(&mut app);
    let resumed_steady = render(&mut app, 120, 40);
    assert_eq!(resumed_steady[(ring_rect.x, ring_rect.y)].bg, Color::Reset);

    let settings_btn = app.settings_area;
    click_center(&mut app, settings_btn, 120, 40);
    assert!(app.panel_open());
    let settings_buf = render(&mut app, 120, 40);
    assert!(
        app.pending_graphic_rings.is_empty(),
        "Settings overlay must occlude all pending Sixel rings"
    );
    for y in ring_rect.y..ring_rect.bottom() {
        for x in ring_rect.x..ring_rect.right() {
            assert_ne!(
                settings_buf[(x, y)].bg,
                Color::Reset,
                "occluded ring cell ({x},{y}) must flush normally under Settings overlay"
            );
        }
    }

    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.panel_open());
    let final_buf = render(&mut app, 120, 40);
    assert!(
        !app.pending_graphic_rings.is_empty(),
        "Sixel rings must resume once Settings overlay closes"
    );
    assert_eq!(final_buf[(ring_rect.x, ring_rect.y)].bg, Color::Reset);

    let _ = fs::remove_dir_all(&root);
}
