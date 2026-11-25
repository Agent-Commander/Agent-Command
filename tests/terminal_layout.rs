use agent_command::{
    markdown::{ToolKind, TranscriptEntry},
    update::{self, UpdateState},
    App,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

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
    assert!(
        body.contains("/spawn") && body.contains("Shortcuts"),
        "{body}"
    );
    key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    let top_body = text(&render(&mut app, 120, 40), app.transcript_area);
    assert!(top_body.contains("Slash Commands"), "{top_body}");

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
