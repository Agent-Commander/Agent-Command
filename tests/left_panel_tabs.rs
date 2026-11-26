use agent_command::{
    inbox::{
        DecisionCard, DecisionInput, DecisionKind, DecisionOption, DecisionReadiness, InboxStore,
    },
    ledger::{Ledger, LedgerDocument, LedgerFilter, TaskKind, TaskStatus},
    tui::tabs::{self, DecisionFilter, InboxKind, LeftPanelMode, ViewTab},
    App, Status, COMMANDER,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

fn render(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    terminal.backend().buffer().clone()
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

fn whole(buffer: &Buffer) -> String {
    text(buffer, buffer.area)
}

fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let width = needle.chars().count();
    (0..buffer.area.height).find_map(|y| {
        let symbols: Vec<&str> = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        (0..symbols.len().saturating_sub(width - 1))
            .find_map(|x| (symbols[x..x + width].concat() == needle).then_some((x as u16, y)))
    })
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.key(KeyEvent::new(code, modifiers));
}

fn crew_app(names: &[&str]) -> App {
    let mut app = App::default();
    for name in names {
        app.input = format!("/spawn {name} reviewer");
        app.submit();
    }
    app.input.clear();
    app
}

#[test]
fn panel_widths_keep_the_canvas_readable_at_every_width() {
    for width in 60..=200u16 {
        for mode in [
            LeftPanelMode::Auto,
            LeftPanelMode::Shown,
            LeftPanelMode::Hidden,
        ] {
            for rail in [true, false] {
                let widths = tabs::panel_widths(width, mode, rail);
                let canvas = width - widths.left - widths.rail;
                if widths.left > 0 || widths.rail > 0 {
                    assert!(canvas >= tabs::MIN_CANVAS_WIDTH, "{width} {mode:?} {rail}");
                }
            }
        }
    }
    assert_eq!(tabs::panel_widths(80, LeftPanelMode::Auto, true).left, 0);
    assert_eq!(tabs::panel_widths(80, LeftPanelMode::Shown, true).left, 0);
    assert_eq!(tabs::panel_widths(110, LeftPanelMode::Auto, true).left, 0);
    assert_eq!(tabs::panel_widths(110, LeftPanelMode::Shown, true).left, 24);
    let mid = tabs::panel_widths(120, LeftPanelMode::Auto, true);
    assert_eq!((mid.left, mid.rail), (26, 30));
    let wide = tabs::panel_widths(140, LeftPanelMode::Auto, true);
    assert_eq!((wide.left, wide.rail), (30, 34));
    assert_eq!(tabs::panel_widths(140, LeftPanelMode::Hidden, true).left, 0);
}

#[test]
fn wide_layout_shows_left_panel_canvas_and_rail_while_narrow_collapses_to_tab_pills() {
    let mut app = crew_app(&["nova"]);
    let wide = render(&mut app, 140, 40);
    let screen = whole(&wide);
    for section in [
        "VIEWS",
        "USAGE",
        "WORKSPACE",
        "THREADS",
        "WORKERS",
        "CONNECTIONS",
    ] {
        assert!(screen.contains(section), "{section} missing:\n{screen}");
    }
    assert!(app.left_panel_visible());
    let top = text(&wide, Rect::new(0, 1, 140, 1));
    assert!(!top.contains(" Inbox "), "{top}");
    assert!(app.transcript_area.x >= 30 && app.transcript_area.right() <= 106);

    let narrow = render(&mut app, 80, 24);
    assert!(!app.left_panel_visible());
    let screen = whole(&narrow);
    assert!(!screen.contains("VIEWS"), "{screen}");
    let top = text(&narrow, Rect::new(0, 1, 80, 1));
    for pill in [" Chat ", " Inbox ", " Ledger "] {
        assert!(top.contains(pill), "{pill} missing: {top}");
    }
    assert!(app.settings_area.right() <= 80 && app.palette_area.width > 0);
    assert!(app.input_area.width == 80 && app.input_area.bottom() < 24);
    assert_eq!(app.transcript_area.x, 0);
    assert_eq!(app.transcript_area.width, 80);
}

#[test]
fn left_panel_toggles_by_key_palette_and_button() {
    let mut app = App::default();
    let _ = render(&mut app, 140, 40);
    assert!(app.left_panel_visible());
    press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    let hidden = render(&mut app, 140, 40);
    assert!(!app.left_panel_visible());
    assert!(text(&hidden, Rect::new(0, 1, 140, 1)).contains(" Inbox "));
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    let _ = render(&mut app, 140, 40);
    assert!(app.left_panel_visible());
    press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    for ch in "Hide Left".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let _ = render(&mut app, 140, 40);
    assert!(!app.left_panel_visible(), "{}", app.notice);
    press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    let shown = render(&mut app, 140, 40);
    let (x, y) = find(&shown, "[F8 ◂]").expect("collapse button");
    app.click(x + 1, y);
    let _ = render(&mut app, 140, 40);
    assert!(!app.left_panel_visible());
    let _ = render(&mut app, 80, 24);
    press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    assert!(app.notice.contains("100+ columns"), "{}", app.notice);
    let _ = render(&mut app, 110, 30);
    assert!(app.left_panel_visible());
}

#[test]
fn tabs_switch_by_alt_digits_focus_cycling_and_mouse_at_every_width() {
    let mut app = App::default();
    let _ = render(&mut app, 140, 40);
    press(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
    assert_eq!(app.view_tab, ViewTab::Inbox);
    press(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
    assert_eq!(app.view_tab, ViewTab::Ledger);
    press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
    assert_eq!(app.view_tab, ViewTab::Chat);

    press(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
    assert!(app.left_focused && !app.editing);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.view_tab, ViewTab::Inbox);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.view_tab, ViewTab::Ledger);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.view_tab, ViewTab::Inbox);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.left_focused && app.editing);

    let wide = render(&mut app, 140, 40);
    let (x, y) = find(&wide, "Ledger").expect("ledger tab row");
    app.click(x, y);
    assert_eq!(app.view_tab, ViewTab::Ledger);

    let narrow = render(&mut app, 80, 24);
    let (x, y) = find(&narrow, " Chat ").expect("chat pill");
    assert_eq!(y, 1);
    app.click(x + 2, y);
    assert_eq!(app.view_tab, ViewTab::Chat);
    let narrow = render(&mut app, 80, 24);
    let (x, y) = find(&narrow, " Inbox ").expect("inbox pill");
    app.click(x + 2, y);
    assert_eq!(app.view_tab, ViewTab::Inbox);
    let inbox = render(&mut app, 80, 24);
    assert!(whole(&inbox).contains("Inbox zero"));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.view_tab, ViewTab::Chat);
}

#[test]
fn inbox_orders_action_needed_first_and_supports_open_read_and_badges() {
    let mut app = crew_app(&["done", "snooze", "fail", "gate"]);
    app.crew[1].status = Status::Done;
    app.crew[2].status = Status::Snoozed;
    app.crew[3].status = Status::Error;
    app.crew[4].status = Status::NeedsApproval;
    let items = app.inbox_items();
    let kinds: Vec<InboxKind> = items.iter().map(|item| item.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            InboxKind::Member(Status::NeedsApproval),
            InboxKind::Member(Status::Error),
            InboxKind::Member(Status::Done),
            InboxKind::Member(Status::Snoozed),
        ]
    );
    assert_eq!(items.iter().filter(|item| item.needs_action()).count(), 2);
    let buffer = render(&mut app, 140, 40);
    assert!(whole(&buffer).contains("Inbox"), "badge row");
    assert!(find(&buffer, " 4 ").is_some(), "{}", whole(&buffer));
    let narrow = render(&mut app, 80, 24);
    assert!(text(&narrow, Rect::new(0, 1, 80, 1)).contains("Inbox (4)"));

    app.set_view_tab(ViewTab::Inbox);
    let view = render(&mut app, 140, 40);
    let screen = whole(&view);
    assert!(
        screen.contains("INBOX · 4 items · 2 need action"),
        "{screen}"
    );
    assert!(screen.contains("gate · needs approval"), "{screen}");
    assert!(
        screen.contains("⏎ Open") && screen.contains("x Read"),
        "{screen}"
    );

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
    assert_eq!(app.inbox_selected, 2);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.view_tab, ViewTab::Chat);
    assert_eq!(app.crew[app.selected].name, "done");
    assert_eq!(app.inbox_count(), 3);

    app.set_view_tab(ViewTab::Inbox);
    let view = render(&mut app, 140, 40);
    let (x, y) = find(&view, " x Read ").expect("read button");
    app.click(x + 1, y);
    assert_eq!(app.inbox_count(), 2);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(app.inbox_count(), 0);
    let empty = render(&mut app, 140, 40);
    assert!(whole(&empty).contains("Inbox zero"));
    assert!(!whole(&empty).contains(" 4 "));

    app.crew[1].status = Status::Running;
    app.crew[1].status = Status::Done;
    app.crew[1].history.push_back(agent_command::Entry {
        at: "12:00:00".into(),
        text: "DONE again".into(),
    });
    assert_eq!(app.inbox_count(), 1, "a new completion is unread again");
}

#[test]
fn inbox_typing_with_a_draft_goes_to_the_composer() {
    let mut app = crew_app(&["nova"]);
    app.crew[1].status = Status::Done;
    app.set_view_tab(ViewTab::Inbox);
    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(app.composer.text(), "hx");
    assert_eq!(app.inbox_count(), 1);
}

#[test]
fn ledger_hydrates_from_crew_filters_and_jumps_to_owner_thread() {
    let mut app = crew_app(&["nova", "ivy", "zed"]);
    app.crew[1].status = Status::Running;
    app.crew[2].status = Status::Error;
    app.crew[3].status = Status::Done;
    let _ = render(&mut app, 140, 40);
    let entries = app.ledger_entries().to_vec();
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|entry| entry.kind == TaskKind::Spawn));
    let status_of = |name: &str| {
        entries
            .iter()
            .find(|entry| entry.owner == name)
            .map(|entry| entry.status)
    };
    assert_eq!(status_of("nova"), Some(TaskStatus::Running));
    assert_eq!(status_of("ivy"), Some(TaskStatus::Error));
    assert_eq!(status_of("zed"), Some(TaskStatus::Done));
    assert_eq!(app.ledger_badge(), "1/3");

    app.crew[1].status = Status::Done;
    let _ = render(&mut app, 140, 40);
    assert_eq!(app.ledger_badge(), "0/3");
    app.crew[1].status = Status::Running;
    let _ = render(&mut app, 140, 40);
    assert_eq!(
        app.ledger_badge(),
        "0/3",
        "finished entries are not revived"
    );

    press(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
    let view = render(&mut app, 140, 40);
    let screen = whole(&view);
    assert!(screen.contains("LEDGER · 0 active · 3 tracked"), "{screen}");
    for heading in ["ID", "STATUS", "OWNER", "SUMMARY", "UPDATED"] {
        assert!(screen.contains(heading), "{heading}: {screen}");
    }
    assert!(
        screen.contains("S-001") && screen.contains("error"),
        "{screen}"
    );

    press(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
    assert_eq!(app.ledger_filter, LedgerFilter::Active);
    assert!(app.ledger_rows().is_empty());
    let active = render(&mut app, 140, 40);
    assert!(whole(&active).contains("No tasks match this filter."));
    press(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
    assert_eq!(app.ledger_filter, LedgerFilter::Attention);
    let rows = app.ledger_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "ivy");
    let attention = render(&mut app, 140, 40);
    let (x, y) = find(&attention, " Done 2 ").expect("done pill");
    app.click(x + 1, y);
    assert_eq!(app.ledger_filter, LedgerFilter::Done);
    assert_eq!(app.ledger_rows().len(), 2);
    let (x, y) = find(&attention, " All 3 ").expect("all pill");
    app.click(x + 1, y);
    assert_eq!(app.ledger_filter, LedgerFilter::All);

    let target = app.ledger_rows()[1].owner.clone();
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.ledger_viewer.is_some());
    let split = render(&mut app, 140, 40);
    assert!(whole(&split).contains("Task Brief"));
    press(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
    assert!(app.ledger_viewer.is_none());
    assert_eq!(app.view_tab, ViewTab::Chat);
    assert_eq!(app.crew[app.selected].name, target);

    press(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
    let view = render(&mut app, 140, 40);
    let first_owner = app.ledger_rows()[0].owner.clone();
    let (_, y) = find(&view, "S-00").expect("ledger row");
    let row_y = (y..40)
        .find(|row| text(&view, Rect::new(0, *row, 140, 1)).contains(&first_owner))
        .unwrap();
    app.click(60, row_y);
    app.click(60, row_y);
    assert!(app.ledger_viewer.is_some());
    let opened = render(&mut app, 140, 40);
    let (jx, jy) = find(&opened, " g Jump to thread ").expect("jump button");
    app.click(jx + 1, jy);
    assert_eq!(app.view_tab, ViewTab::Chat);
    assert_eq!(app.crew[app.selected].name, first_owner);
}

#[test]
fn ledger_view_is_usable_at_80_by_24_and_drops_updated_when_tight() {
    let mut app = crew_app(&["nova"]);
    app.crew[1].status = Status::NeedsApproval;
    app.set_view_tab(ViewTab::Ledger);
    let _ = render(&mut app, 140, 40);
    press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    let tight = render(&mut app, 110, 30);
    assert!(app.left_panel_visible());
    let tight_screen = whole(&tight);
    assert!(tight_screen.contains("needs_approval"), "{tight_screen}");
    assert!(!tight_screen.contains("UPDATED"), "{tight_screen}");
    let buffer = render(&mut app, 80, 24);
    let screen = whole(&buffer);
    assert!(screen.contains("needs_approval"), "{screen}");
    assert!(screen.contains("SUMMARY"), "{screen}");
    assert!(screen.contains("UPDATED"), "{screen}");
    assert!(screen.contains(" Attention 1 "), "{screen}");
    for y in 0..24 {
        let line = text(&buffer, Rect::new(0, y, 80, 1));
        assert!(line.chars().count() <= 80);
    }

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.ledger_viewer.is_some());
    let doc_80 = render(&mut app, 80, 24);
    assert!(whole(&doc_80).contains("Esc Back"));
    for y in 0..24 {
        let line = text(&doc_80, Rect::new(0, y, 80, 1));
        assert!(line.chars().count() <= 80);
    }
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.ledger_viewer.is_none());
}

fn make_decision_card(id: &str, title: &str, question: &str) -> DecisionCard {
    let mut store = InboxStore::default();
    store
        .upsert_card(
            DecisionInput {
                id: id.into(),
                title: title.into(),
                question: question.into(),
                current_state: "Waiting for operator choice.".into(),
                why_it_matters: "Determines next execution stage.".into(),
                recommendation: "approve — lowest risk".into(),
                source_row: "M-001".into(),
                owner_agent_id: "builder".into(),
                owner_name: "Builder".into(),
                project: "Agent-Command".into(),
                group: "v0.3".into(),
                readiness: DecisionReadiness::Ready,
                alternatives: vec![
                    DecisionOption {
                        id: "approve".into(),
                        label: "Approve rollout".into(),
                    },
                    DecisionOption {
                        id: "defer".into(),
                        label: "Defer rollout".into(),
                    },
                ],
                kind: DecisionKind::Standard,
                constraints: Some("Zero new dependencies.".into()),
                example: None,
                detail: None,
                preparation: None,
                canonical_sources: vec!["ledger-docs/active/M-001.md".into()],
                is_current_blocker: true,
                self_serve: None,
                stuck: None,
            },
            1_700_000_000,
        )
        .unwrap()
        .clone()
}

#[test]
fn decision_inbox_modal_enforces_no_auto_open_explain_more_and_retire_at_80x24_and_120x40() {
    let mut app = App::default();
    app.decision_cards = vec![
        make_decision_card("AC-INT-1", "Cache policy", "Enable local cache compaction?"),
        make_decision_card(
            "AC-INT-2",
            "Archive cadence",
            "Run weekly archive compaction?",
        ),
        make_decision_card("AC-INT-3", "Retry budget", "Keep bounded requeue at 1?"),
    ];
    app.set_view_tab(ViewTab::Inbox);
    assert_eq!(app.inbox_count(), 3);

    for (w, h) in [(120, 40), (80, 24)] {
        let buf = render(&mut app, w, h);
        let screen = whole(&buf);
        assert!(screen.contains("AC-INT-1"), "{w}x{h}: {screen}");
        for y in 0..h {
            assert!(text(&buf, Rect::new(0, y, w, 1)).chars().count() <= usize::from(w));
        }
    }

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.inbox_modal.as_ref().map(|m| m.card_id.as_str()),
        Some("AC-INT-1")
    );
    for (w, h) in [(120, 40), (80, 24)] {
        let buf = render(&mut app, w, h);
        let screen = whole(&buf);
        assert!(screen.contains("Where things stand"), "{w}x{h}: {screen}");
        assert!(screen.contains("Owner recommendation"), "{w}x{h}: {screen}");
    }

    press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(app.decision_filter_count(DecisionFilter::Drafts), 1);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.inbox_modal.is_none());
    assert_eq!(app.filtered_decision_cards().len(), 2);
    assert_eq!(app.filtered_decision_cards()[0].id, "AC-INT-2");
    assert_eq!(app.inbox_selected, 0);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.inbox_modal.as_ref().map(|m| m.card_id.as_str()),
        Some("AC-INT-2")
    );
    press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
    for ch in "Clarify schedule".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.inbox_modal.is_none());
    assert_eq!(app.filtered_decision_cards().len(), 1);
    assert_eq!(app.filtered_decision_cards()[0].id, "AC-INT-3");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    assert!(app.inbox_modal.is_none());
    assert!(app.filtered_decision_cards().is_empty());
    assert_eq!(app.past_decision_cards().len(), 2);
}

#[test]
fn ledger_document_viewer_renders_persisted_markdown_at_80x24_and_120x40() {
    let root = std::env::temp_dir().join(format!(
        "agent-command-ledger-int-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let ledger_path = root.join("ledger.json");
    let mut disk_ledger = Ledger::default();
    let id = disk_ledger.record(
        TaskKind::Goal,
        COMMANDER,
        "Ship document linked ledger",
        1_700_000_000,
    );
    let entry = disk_ledger.get(&id).unwrap().clone();
    let doc = LedgerDocument::from_entry(&entry, "Ship document linked ledger");
    disk_ledger.write_document(&root, &id, &doc).unwrap();
    disk_ledger.save(&ledger_path).unwrap();
    let reloaded = Ledger::load(&ledger_path);
    let doc_text = reloaded
        .read_document(&root, &id)
        .expect("persisted ledger markdown document");
    assert!(doc_text.contains("## Task Brief"), "{doc_text}");
    assert!(
        doc_text.contains("Ship document linked ledger"),
        "{doc_text}"
    );
    std::fs::remove_dir_all(&root).unwrap();

    let mut app = App::default();
    app.input = "/goal ship document linked ledger".into();
    app.submit();
    app.set_view_tab(ViewTab::Ledger);
    assert_eq!(app.ledger_rows().len(), 1);

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.ledger_viewer.is_some());
    let wide = render(&mut app, 120, 40);
    let wide_text = whole(&wide);
    assert!(wide_text.contains("Task Brief"), "{wide_text}");
    assert!(wide_text.contains("Jump to thread"), "{wide_text}");
    assert!(wide_text.contains("Esc Back"), "{wide_text}");
    for y in 0..40 {
        assert!(text(&wide, Rect::new(0, y, 120, 1)).chars().count() <= 120);
    }

    app.left_panel = LeftPanelMode::Hidden;
    app.rail_open = false;
    let split = render(&mut app, 120, 40);
    let split_text = whole(&split);
    assert!(split_text.contains("SUMMARY"), "{split_text}");
    assert!(split_text.contains("Task Brief"), "{split_text}");
    assert!(split_text.contains("Jump to thread"), "{split_text}");

    let narrow = render(&mut app, 80, 24);
    let narrow_text = whole(&narrow);
    assert!(narrow_text.contains("Esc Back"), "{narrow_text}");
    for y in 0..24 {
        assert!(text(&narrow, Rect::new(0, y, 80, 1)).chars().count() <= 80);
    }

    press(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
    assert!(app.ledger_viewer.is_none());
    assert_eq!(app.view_tab, ViewTab::Chat);
    assert_eq!(app.crew[app.selected].name, COMMANDER);
}
