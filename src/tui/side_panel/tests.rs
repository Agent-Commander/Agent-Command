use std::{fs, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

use crate::{
    auth::{Profile, Provider},
    bridge::BackgroundResult,
    ledger::{LedgerFilter, TaskKind, TaskStatus},
    panel::Panel,
    specialists::{Member, Status},
    tui::views::{plan_columns, ActiveTab, InboxAction, InboxKind, TabHit, CENTER_MIN_WIDTH},
    update::{UpdateState, CURRENT},
    usage::Allowance,
    App, Live,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ac-views-{name}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(root.join("work")).unwrap();
        Self(root)
    }

    fn workspace(&self) -> PathBuf {
        self.0.join("work")
    }

    fn profile(&self, provider: Provider, name: &str) -> Profile {
        Profile::named_fixture(
            &self.0.join("profiles"),
            provider,
            name,
            self.0.join("provider"),
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const HOUR: i64 = 3_600;

fn live_app(scratch: &Scratch) -> App {
    let profiles = vec![
        scratch.profile(Provider::Claude, "work"),
        scratch.profile(Provider::Codex, "personal"),
        scratch.profile(Provider::Gemini, "lab"),
    ];
    let root = scratch.0.join("profiles");
    let mut app = App::from_live(Live::new(scratch.workspace(), profiles, Some(root)));
    let live = app.live.as_mut().unwrap();
    live.allowances = vec![
        Allowance {
            five: Some(64.0),
            week: Some(12.0),
            reset_in: Some(2 * HOUR + 26 * 60),
            latched: false,
        },
        Allowance {
            five: Some(9.0),
            week: None,
            reset_in: Some(40 * 60),
            latched: false,
        },
        Allowance::default(),
    ];
    app.views.profile_tiers.insert(
        "claude/work".into(),
        crate::token_burn::normalize_tier(Some("Claude Max")),
    );
    app.views.profile_tiers.insert(
        "codex/personal".into(),
        crate::token_burn::normalize_tier(Some("plus")),
    );
    app.git_snapshot.branch = Some("redesign/v0.3-tui-overhaul".into());
    app.git_snapshot.dirty = true;
    app
}

fn member(name: &str, status: Status, task: &str) -> Member {
    let mut member = Member::new(name, "builder", Some(task), None);
    member.status = status;
    member
}

fn render(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn region(buffer: &Buffer, area: Rect) -> String {
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot(label: &str, buffer: &Buffer) -> String {
    let text = region(buffer, buffer.area);
    println!("===== {label} =====\n{text}\n");
    text
}

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.key(KeyEvent::new(code, modifiers));
}

fn hit_rect(app: &App, wanted: TabHit) -> Rect {
    app.tab_hits()
        .iter()
        .find(|(_, hit)| *hit == wanted)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("no hit for {wanted:?}"))
}

fn click(app: &mut App, rect: Rect) {
    assert!(rect.width > 0 && rect.height > 0);
    app.click(rect.x + rect.width / 2, rect.y + rect.height / 2);
}

fn left_panel_text(buffer: &Buffer) -> String {
    region(buffer, Rect::new(0, 1, 30, buffer.area.height - 6))
}

fn is_braille(symbol: &str) -> bool {
    symbol
        .chars()
        .next()
        .is_some_and(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch))
}

#[test]
fn wide_layout_shows_left_panel_rings_workspace_rail_and_no_update_button() {
    let scratch = Scratch::new("wide");
    let mut app = live_app(&scratch);
    app.crew
        .push(member("nova", Status::Done, "Polish the rail"));
    let buffer = render(&mut app, 140, 40);
    let screen = snapshot("140x40 up to date", &buffer);
    assert!(app.left_panel_visible());
    let left = left_panel_text(&buffer);
    for needle in [
        "VIEWS",
        "▸ Chat",
        "Inbox (1)",
        "Ledger 0/1",
        "USAGE",
        "Claude · work",
        "64% left",
        " MAX ",
        " PLUS ",
        "↻ 2h 26m",
        "5h 64% · 7d 12%",
        "Codex · personal",
        "9% left",
        "WORKSPACE",
        "⎇ redesign/v0.3-tui",
        "⛨ workspace-auto",
        "⛊ ProdGate on",
        "THREADS",
        "Commander",
        "nova",
    ] {
        assert!(left.contains(needle), "missing {needle:?}\n{left}");
    }
    let braille = (0..30)
        .flat_map(|x| (0..40).map(move |y| (x, y)))
        .filter(|&(x, y)| is_braille(buffer[(x, y)].symbol()))
        .count();
    assert!(braille >= 20, "expected braille rings, found {braille}");
    assert!(screen.contains("WORKERS") && screen.contains("CONNECTIONS"));
    assert!(app.crew_area.x > 100);
    assert_eq!(app.update_area, Rect::default());
    assert!(!region(&buffer, Rect::new(0, 39, 140, 1)).contains("Update"));
    assert!(!region(&buffer, Rect::new(0, 0, 140, 1)).contains(" Inbox"));
}

#[test]
fn ring_colors_follow_token_burn_semantics_in_the_panel() {
    let scratch = Scratch::new("colors");
    let mut app = live_app(&scratch);
    let buffer = render(&mut app, 140, 40);
    let deck = app.deck_palette;
    let find = |needle: &str| {
        (0..40u16).find_map(|y| {
            let row = region(&buffer, Rect::new(0, y, 30, 1));
            row.find(needle).map(|byte| {
                let column = row[..byte].chars().count() as u16;
                (column, y)
            })
        })
    };
    let (x, y) = find("64% left").unwrap();
    assert_eq!(buffer[(x, y)].fg, deck.healthy);
    let (x, y) = find("9% left").unwrap();
    assert_eq!(buffer[(x, y)].fg, deck.depleted);
    let (x, y) = find("unreported").unwrap();
    assert_eq!(buffer[(x, y)].fg, deck.unknown);
    let (x, y) = find("↻ 2h 26m").unwrap();
    assert_eq!(buffer[(x, y)].fg, deck.reset_six_hours);
    let (x, y) = find("↻ 40m").unwrap();
    assert_eq!(buffer[(x, y)].fg, deck.reset_hour);
}

#[test]
fn wide_layout_with_a_newer_release_shows_a_clickable_update_button() {
    let scratch = Scratch::new("update");
    let mut app = live_app(&scratch);
    app.update_available = Some("9.9.9".into());
    app.update_state = UpdateState::Available {
        tag: "v9.9.9".into(),
        current: CURRENT.into(),
    };
    let buffer = render(&mut app, 140, 40);
    snapshot("140x40 newer release detected", &buffer);
    let footer = region(&buffer, Rect::new(0, 39, 140, 1));
    assert!(footer.ends_with(" ⬆ Update v9.9.9 (F6) "), "{footer}");
    assert_eq!(app.update_area.right(), 140);
    assert_eq!(app.update_area.y, 39);
    key(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    let keyboard = std::mem::take(&mut app.notice);
    let button = app.update_area;
    click(&mut app, button);
    assert!(!keyboard.is_empty());
    assert_eq!(app.notice, keyboard);
}

#[test]
fn narrow_layout_collapses_left_panel_and_keeps_tab_pills_reachable() {
    let scratch = Scratch::new("narrow");
    let mut app = live_app(&scratch);
    let buffer = render(&mut app, 80, 24);
    snapshot("80x24 collapsed", &buffer);
    assert!(!app.left_panel_visible());
    let top = region(&buffer, Rect::new(0, 0, 80, 1));
    for needle in [" Chat ", " Inbox ", " Ledger 0/0 ", "◕ 64%"] {
        assert!(top.contains(needle), "missing {needle:?}: {top}");
    }
    assert!(app.settings_area.right() <= 80 && app.palette_area.right() <= 80);
    assert!(app.input_area.width == 80 && app.input_area.bottom() < 24);
    let pill = hit_rect(&app, TabHit::Tab(ActiveTab::Ledger));
    assert!(pill.right() <= app.palette_area.x);
    click(&mut app, pill);
    assert_eq!(app.active_tab(), ActiveTab::Ledger);
    let buffer = render(&mut app, 80, 24);
    snapshot("80x24 ledger via pill", &buffer);
    assert!(region(&buffer, Rect::new(0, 1, 80, 22)).contains("No tracked tasks yet."));
    let chat = hit_rect(&app, TabHit::Tab(ActiveTab::Chat));
    click(&mut app, chat);
    assert_eq!(app.active_tab(), ActiveTab::Chat);
    assert!(app.editing);
}

#[test]
fn column_plan_keeps_the_center_readable_at_every_width() {
    let both = plan_columns(140, true, true, false);
    assert_eq!((both.left, both.rail), (30, 34));
    for width in 100..=160 {
        for prefer_rail in [false, true] {
            let plan = plan_columns(width, true, true, prefer_rail);
            assert!(
                width - plan.left - plan.rail >= CENTER_MIN_WIDTH,
                "{width}: {plan:?}"
            );
            assert!(plan.left > 0 || plan.rail > 0);
        }
    }
    assert_eq!(plan_columns(99, true, true, false).left, 0);
    assert_eq!(plan_columns(99, true, true, false).rail, 0);
    assert_eq!(plan_columns(110, true, true, false).left, 26);
    assert_eq!(plan_columns(110, true, true, false).rail, 28);
    assert_eq!(plan_columns(110, false, true, false).rail, 34);
    let scratch = Scratch::new("mid");
    let mut app = live_app(&scratch);
    render(&mut app, 110, 30);
    assert!(!app.left_panel_visible());
    key(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    let buffer = render(&mut app, 110, 30);
    assert!(app.left_panel_visible());
    assert!(app.transcript_area.width >= CENTER_MIN_WIDTH);
    assert!(region(&buffer, Rect::new(0, 0, 110, 30)).contains("WORKERS"));
}

#[test]
fn left_panel_toggles_from_keys_palette_and_mouse() {
    let scratch = Scratch::new("toggle");
    let mut app = live_app(&scratch);
    render(&mut app, 140, 40);
    assert!(app.left_panel_visible());
    key(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    let buffer = render(&mut app, 140, 40);
    assert!(!app.left_panel_visible());
    assert!(region(&buffer, Rect::new(0, 0, 140, 1)).contains(" Inbox "));
    key(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    render(&mut app, 140, 40);
    assert!(app.left_panel_visible());
    let toggle = hit_rect(&app, TabHit::ToggleLeftPanel);
    click(&mut app, toggle);
    render(&mut app, 140, 40);
    assert!(!app.left_panel_visible());
    app.open_command_palette();
    app.palette.close();
    app.execute_palette_action("view.left_panel");
    render(&mut app, 140, 40);
    assert!(app.left_panel_visible());
    let items = crate::tui::build_palette_items(
        &app.crew,
        &[],
        crate::tui::PaletteToggles {
            rail_open: true,
            left_panel_open: true,
        },
        None,
    );
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
    for wanted in [
        "Hide Left Panel",
        "Switch to Chat",
        "Switch to Inbox",
        "Switch to Ledger",
    ] {
        assert!(labels.contains(&wanted), "{labels:?}");
    }
    key(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    render(&mut app, 80, 24);
    key(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    render(&mut app, 80, 24);
    assert!(!app.left_panel_visible());
    assert!(app.notice.contains("needs 100+ columns"), "{}", app.notice);
}

#[test]
fn tabs_switch_with_alt_digits_palette_and_left_panel_focus() {
    let scratch = Scratch::new("tabs");
    let mut app = live_app(&scratch);
    render(&mut app, 140, 40);
    key(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
    assert_eq!(app.active_tab(), ActiveTab::Inbox);
    assert!(app.view_focused() && !app.editing);
    key(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
    assert_eq!(app.active_tab(), ActiveTab::Ledger);
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.active_tab(), ActiveTab::Chat);
    assert!(app.editing);
    app.execute_palette_action("tab.inbox");
    assert_eq!(app.active_tab(), ActiveTab::Inbox);
    assert!(!app.editing);
    key(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
    render(&mut app, 140, 40);
    key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert!(app.rail_focused);
    key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert!(app.left_panel_focused());
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active_tab(), ActiveTab::Inbox);
    key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
    assert_eq!(app.active_tab(), ActiveTab::Ledger);
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active_tab(), ActiveTab::Chat);
    key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active_tab(), ActiveTab::Ledger);
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.view_focused() && !app.left_panel_focused());
    let inbox = hit_rect(&app, TabHit::Tab(ActiveTab::Inbox));
    click(&mut app, inbox);
    assert_eq!(app.active_tab(), ActiveTab::Inbox);
    key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert!(app.editing && !app.view_focused());
    assert_eq!(app.composer.text(), "h");
}

fn wake(worker: &str, is_error: bool) -> BackgroundResult {
    BackgroundResult {
        worker_id: worker.into(),
        role: "builder".into(),
        status: if is_error { "failed" } else { "completed" }.into(),
        branch: None,
        workspace: "/tmp/work".into(),
        summary: format!("{worker} summary"),
        is_error,
        finished_at: "2026-11-25T21:00:00Z".into(),
    }
}

fn inbox_app(scratch: &Scratch) -> App {
    let mut app = live_app(scratch);
    app.crew
        .push(member("atlas", Status::Done, "Write the docs"));
    app.crew
        .push(member("nova", Status::Error, "Fix the build"));
    app.crew
        .push(member("orion", Status::NeedsApproval, "Push to prod"));
    app.crew.push(member("vega", Status::Running, "Still busy"));
    app.crew
        .push(member("lyra", Status::Snoozed, "Waiting on limits"));
    app.record_wake(wake("scout", false));
    app.live
        .as_mut()
        .unwrap()
        .pending_prod_gates
        .insert("req-1".into(), "Commander".into());
    app
}

#[test]
fn inbox_aggregates_attention_items_in_priority_order() {
    let scratch = Scratch::new("inbox-order");
    let mut app = inbox_app(&scratch);
    let items = app.inbox_items();
    let order: Vec<(u8, &str)> = items
        .iter()
        .map(|item| (item.priority(), item.thread.as_str()))
        .collect();
    assert_eq!(
        order,
        [
            (0, "Commander"),
            (2, "orion"),
            (3, "nova"),
            (4, "atlas"),
            (5, "lyra"),
            (6, "scout"),
        ]
    );
    assert!(matches!(items[0].kind, InboxKind::Approval { .. }));
    assert_eq!(
        items[0].actions(),
        [InboxAction::Approve, InboxAction::Deny, InboxAction::Jump]
    );
    assert!(!items.iter().any(|item| item.thread == "vega"));
    app.switch_tab(ActiveTab::Inbox);
    let buffer = render(&mut app, 140, 40);
    let screen = snapshot("140x40 inbox with items", &buffer);
    assert!(screen.contains("INBOX · 6 need attention"), "{screen}");
    assert!(screen.contains("Inbox (6)"));
    let rows: Vec<usize> = [
        "ProdGate approval",
        "orion · needs approval",
        "nova · error",
        "atlas · done",
        "lyra · snoozed",
        "scout finished",
    ]
    .iter()
    .map(|needle| {
        screen
            .find(needle)
            .unwrap_or_else(|| panic!("{needle}\n{screen}"))
    })
    .collect();
    assert!(rows.windows(2).all(|pair| pair[0] < pair[1]), "{rows:?}");
    for label in [" Approve ", " Deny ", " Jump ", " x Dismiss "] {
        assert!(screen.contains(label), "{label}");
    }
}

#[test]
fn inbox_keyboard_and_mouse_actions_resolve_and_dismiss_items() {
    let scratch = Scratch::new("inbox-actions");
    let mut app = inbox_app(&scratch);
    render(&mut app, 140, 40);
    key(&mut app, KeyCode::Char('2'), KeyModifiers::ALT);
    render(&mut app, 140, 40);
    let approve = hit_rect(&app, TabHit::InboxAction(0, InboxAction::Approve));
    click(&mut app, approve);
    assert!(app.live.as_ref().unwrap().pending_prod_gates.is_empty());
    assert_eq!(app.notice, "Allowed ProdGate action.");
    assert_eq!(app.inbox_count(), 5);

    render(&mut app, 140, 40);
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.view_selection(), 2);
    key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(app.inbox_count(), 4);
    assert!(!app.inbox_items().iter().any(|item| item.thread == "atlas"));
    let buffer = render(&mut app, 140, 40);
    assert!(region(&buffer, Rect::new(0, 0, 140, 40)).contains("Inbox (4)"));

    key(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.active_tab(), ActiveTab::Chat);
    assert_eq!(app.crew[app.selected].name, "nova");
    assert_eq!(app.inbox_count(), 3);

    app.switch_tab(ActiveTab::Inbox);
    render(&mut app, 140, 40);
    let row = hit_rect(&app, TabHit::InboxRow(1));
    click(&mut app, row);
    assert_eq!(app.view_selection(), 1);
    let jump = hit_rect(&app, TabHit::InboxAction(0, InboxAction::Jump));
    click(&mut app, jump);
    assert_eq!(app.crew[app.selected].name, "orion");

    app.crew.retain(|m| m.name == "Commander");
    app.views.wakes.clear();
    app.switch_tab(ActiveTab::Inbox);
    let buffer = render(&mut app, 140, 40);
    let screen = snapshot("140x40 inbox empty", &buffer);
    assert!(screen.contains("Inbox zero"), "{screen}");
    assert!(screen.contains("INBOX · all clear"));
    assert!(!screen.contains("Inbox ("));
}

#[test]
fn ledger_hydrates_from_crew_goals_and_missions_and_filters() {
    let scratch = Scratch::new("ledger");
    let mut app = live_app(&scratch);
    app.crew
        .push(member("atlas", Status::Done, "Write the docs"));
    let mut nova = member("nova", Status::Error, "Fix the build");
    nova.workspace = Some(crate::specialists::Workspace {
        path: scratch.0.join("nova"),
        isolation: crate::specialists::Isolation::Directory,
        branch: Some(crate::branches::Branch {
            name: "agent/nova-fix".into(),
            base: "main".into(),
            base_commit: "0000000".into(),
        }),
    });
    app.crew.push(nova);
    app.crew
        .push(member("orion", Status::NeedsApproval, "Push to prod"));
    app.crew
        .push(member("vega", Status::Idle, "Plan the release"));
    let mut reviewed = member("lyra", Status::Running, "Review the API");
    reviewed.review.rounds = 1;
    app.crew.push(reviewed);
    app.goal = Some(crate::goal::Goal::new("nova", "ship the fix", 5));
    render(&mut app, 140, 40);
    let ledger = app.ledger();
    let status_of = |key: &str| ledger.get(key).map(|entry| entry.status);
    assert_eq!(status_of("spawn:atlas"), Some(TaskStatus::Done));
    assert_eq!(status_of("spawn:nova"), Some(TaskStatus::Error));
    assert_eq!(status_of("spawn:orion"), Some(TaskStatus::NeedsApproval));
    assert_eq!(status_of("spawn:vega"), Some(TaskStatus::Queued));
    assert_eq!(status_of("review:lyra:1"), Some(TaskStatus::Running));
    let goal = ledger
        .entries()
        .iter()
        .find(|entry| entry.kind == TaskKind::Goal)
        .unwrap();
    assert_eq!(goal.owner, "nova");
    assert_eq!(goal.title, "Goal: ship the fix (0/5)");
    let goal_key = goal.key.clone();

    app.goal = None;
    app.views.goal_cancelled = true;
    render(&mut app, 140, 40);
    assert_eq!(
        app.ledger().get(&goal_key).unwrap().status,
        TaskStatus::Cancelled
    );

    app.switch_tab(ActiveTab::Ledger);
    let buffer = render(&mut app, 140, 40);
    let screen = snapshot("140x40 ledger all", &buffer);
    assert!(screen.contains("LEDGER · 4 active / 7 total"), "{screen}");
    for needle in [
        " All 7 ",
        " Active 4 ",
        " Attention 2 ",
        " Done 2 ",
        "STATUS",
        "OWNER",
        "BRANCH",
    ] {
        assert!(screen.contains(needle), "{needle}\n{screen}");
    }
    let attention = hit_rect(&app, TabHit::LedgerFilter(LedgerFilter::Attention));
    click(&mut app, attention);
    assert_eq!(app.ledger_filter(), LedgerFilter::Attention);
    let rows: Vec<String> = app.ledger_rows().iter().map(|e| e.owner.clone()).collect();
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&"nova".to_owned()) && rows.contains(&"orion".to_owned()));
    let buffer = render(&mut app, 140, 40);
    snapshot("140x40 ledger attention filter", &buffer);
    key(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
    assert_eq!(app.ledger_filter(), LedgerFilter::Done);
    let buffer = render(&mut app, 140, 40);
    snapshot("140x40 ledger done filter", &buffer);
    assert_eq!(app.ledger_rows().len(), 2);
    key(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('f'), KeyModifiers::NONE);
    assert_eq!(app.ledger_filter(), LedgerFilter::Active);
    render(&mut app, 140, 40);
    let target = app
        .ledger_rows()
        .iter()
        .position(|entry| entry.owner == "orion")
        .unwrap();
    let row = hit_rect(&app, TabHit::LedgerRow(target));
    click(&mut app, row);
    assert_eq!(app.view_selection(), target);
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.active_tab(), ActiveTab::Chat);
    assert_eq!(app.crew[app.selected].name, "orion");
}

#[test]
fn ledger_tracks_commander_missions_and_persists_per_workspace() {
    let scratch = Scratch::new("ledger-persist");
    let mut app = live_app(&scratch);
    app.crew
        .push(member("atlas", Status::Done, "Write the docs"));
    app.crew[0].mission = "Refactor the router".into();
    let (mission, _events) = crate::mission::Mission::held();
    app.live
        .as_mut()
        .unwrap()
        .runs
        .push(crate::Run::new(crate::COMMANDER, mission, false));
    render(&mut app, 140, 40);
    let mission_entry = app
        .ledger()
        .entries()
        .iter()
        .find(|entry| entry.kind == TaskKind::Mission)
        .cloned()
        .unwrap();
    assert_eq!(mission_entry.owner, "Commander");
    assert_eq!(mission_entry.title, "Refactor the router");
    assert_eq!(mission_entry.status, TaskStatus::Running);
    app.live.as_mut().unwrap().runs.clear();
    render(&mut app, 140, 40);
    assert_eq!(
        app.ledger().get(&mission_entry.key).unwrap().status,
        TaskStatus::Done
    );
    let path = app.views.ledger_path.clone().unwrap();
    assert!(path.starts_with(&scratch.0));
    assert!(path.exists());
    let saved = app.ledger().entries().to_vec();
    drop(app);

    let mut reopened = live_app(&scratch);
    render(&mut reopened, 140, 40);
    for entry in &saved {
        assert_eq!(reopened.ledger().get(&entry.key), Some(entry));
    }

    let other = scratch.0.join("other-work");
    fs::create_dir_all(&other).unwrap();
    let mut elsewhere = App::from_live(Live::new(
        other,
        Vec::new(),
        Some(scratch.0.join("profiles")),
    ));
    render(&mut elsewhere, 140, 40);
    assert!(elsewhere.ledger().entries().is_empty());
}

#[test]
fn clicking_a_usage_ring_tile_opens_settings_for_that_subscription() {
    let scratch = Scratch::new("tile-click");
    let mut app = live_app(&scratch);
    render(&mut app, 140, 40);
    let tiles = app.deck_hits();
    assert_eq!(tiles.len(), 3);
    assert!(tiles.iter().all(|(_, rect)| rect.right() <= 30));
    let (profile, rect) = tiles[1];
    assert_eq!(profile, 1);
    click(&mut app, rect);
    assert!(matches!(app.panel, Some(Panel::Settings(_))));
    assert_eq!(
        app.focused_action(),
        Some(crate::panel::Action::Reconnect(1))
    );
}

#[test]
fn preview_left_panel_shows_real_empty_usage_state() {
    let mut app = App::default();
    let buffer = render(&mut app, 140, 40);
    let left = left_panel_text(&buffer);
    assert!(left.contains("No subscription connected"), "{left}");
    assert!(app.deck_hits().is_empty());
    let empty = hit_rect(&app, TabHit::UsageTile(usize::MAX));
    click(&mut app, empty);
    assert!(matches!(app.panel, Some(Panel::Settings(_))));
}
