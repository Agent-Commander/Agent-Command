use agent_command::App;
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
fn click(app: &mut App, area: Rect, w: u16, h: u16) {
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
fn crewed() -> App {
    let mut app = App::default();
    app.input = "/spawn Nova reviewer".into();
    app.submit();
    app.selected = 0;
    app
}
fn render(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    terminal.backend().buffer().clone()
}
#[test]
fn regions_and_equivalent_controls_at_supported_sizes() {
    for (w, h) in [(80, 24), (120, 40)] {
        let mut keyboard = crewed();
        let mut mouse = crewed();
        let header = render(&mut mouse, w, h);
        assert!(text(&header, Rect::new(0, 0, w, 1)).contains("Radio    |    PRIVATEER"));
        let names: Vec<String> = mouse
            .crew
            .iter()
            .map(|member| member.name.clone())
            .collect();
        assert_eq!(names, ["Commander", "Nova"]);
        for (selected, name) in names.iter().enumerate() {
            if selected > 0 {
                key(&mut keyboard, KeyCode::Down, KeyModifiers::NONE);
            }
            let row_height = mouse.crew_area.height / 4;
            assert!(row_height > 0, "every crew row must be visible at {w}x{h}");
            mouse.click(
                mouse.crew_area.x,
                mouse.crew_area.y + selected as u16 * row_height + 1,
            );
            assert_eq!(
                mouse.selected, keyboard.selected,
                "crew mouse/keyboard selection"
            );
            assert!(
                text(&render(&mut mouse, w, h), mouse.crew_area).contains(name),
                "crew name in crew region"
            );
        }
        key(&mut keyboard, KeyCode::Char('n'), KeyModifiers::CONTROL);
        let input = mouse.input_area;
        click(&mut mouse, input, w, h);
        for c in "Mission café λ".chars() {
            key(&mut keyboard, KeyCode::Char(c), KeyModifiers::NONE);
            key(&mut mouse, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(text(&render(&mut mouse, w, h), mouse.input_area).contains("Mission café λ"));
        key(&mut keyboard, KeyCode::Enter, KeyModifiers::NONE);
        let send = mouse.send_area;
        click(&mut mouse, send, w, h);
        for paused in [true, false] {
            key(&mut keyboard, KeyCode::Char('p'), KeyModifiers::CONTROL);
            let pause = mouse.pause_area;
            click(&mut mouse, pause, w, h);
            assert_eq!(mouse.crew, keyboard.crew);
            assert_eq!(mouse.log, keyboard.log);
            mouse.log.clear();
            mouse.log_times.clear();
            keyboard.log.clear();
            keyboard.log_times.clear();
            mouse.log.push_back("SYSTEM  Recent-marker".into());
            mouse.log_times.push_back("12:34:56".into());
            keyboard.log = mouse.log.clone();
            keyboard.log_times = mouse.log_times.clone();
            let buffer = render(&mut mouse, w, h);
            let chart = Rect::new(
                mouse.crew_area.right() + 3,
                mouse.crew_area.y,
                mouse.pause_area.right() - mouse.crew_area.right() - 3,
                mouse.pause_area.bottom() - mouse.crew_area.y,
            );
            assert!(
                text(&buffer, chart).contains("Mission café λ"),
                "mission must appear in chart, independently of log"
            );
            assert!(
                text(&buffer, chart).contains(if paused { "PAUSED" } else { "IDLE" }),
                "selected status in chart"
            );
            assert!(text(&buffer, mouse.pause_area).contains(if paused {
                "Resume"
            } else {
                "Pause"
            }));
            assert!(text(&buffer, mouse.send_area).contains("Send"));
            let log = Rect::new(
                mouse.pause_area.right() + 2,
                mouse.crew_area.y,
                w - mouse.pause_area.right() - 3,
                mouse.pause_area.bottom() - mouse.crew_area.y,
            );
            assert!(
                text(&buffer, log).contains("Mission café λ"),
                "selected specialist history in log region"
            );
            let specialist = mouse.selected;
            mouse.click(mouse.crew_area.x, mouse.crew_area.y + 1);
            let buffer = render(&mut mouse, w, h);
            assert!(
                text(&buffer, log).contains("Recent-marker"),
                "Commander feed in log region after returning to Commander"
            );
            mouse.selected = specialist;
        }
    }
}
#[test]
fn resize_retains_unicode_and_selected_state() {
    let mut app = crewed();
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    for c in "Café 航海 🚀".chars() {
        key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    app.input = "Draft λ".into();
    let state = (
        app.selected,
        app.crew.clone(),
        app.input.clone(),
        app.log.clone(),
        app.log_times.clone(),
        app.editing,
    );
    for (w, h) in [(120, 40), (80, 24), (120, 40)] {
        let buffer = render(&mut app, w, h);
        assert!(text(&buffer, app.crew_area).contains("Nova"));
        let chart = Rect::new(
            app.crew_area.right() + 3,
            app.crew_area.y,
            app.pause_area.right() - app.crew_area.right() - 3,
            app.pause_area.bottom() - app.crew_area.y,
        );
        let chart_text = text(&buffer, chart);
        for expected in ["Nova", "PAUSED", "Café", "航", "海", "🚀"] {
            assert!(
                chart_text.contains(expected),
                "resize must retain visible chart content: {expected}"
            );
        }
        assert!(text(&buffer, app.input_area).contains("Draft λ"));
        assert!(text(&buffer, app.pause_area).contains("Resume"));
        assert_eq!(
            (
                app.selected,
                app.crew.clone(),
                app.input.clone(),
                app.log.clone(),
                app.log_times.clone(),
                app.editing
            ),
            state
        );
    }
}

#[test]
fn crew_helper_is_complete_inside_its_region() {
    for (w, h, expected) in [
        (80, 24, "↑ ↓ select\nEnter: edit"),
        (120, 40, "↑ ↓ select an agent\nEnter to inspect"),
    ] {
        let mut app = App::default();
        let buffer = render(&mut app, w, h);
        let helper = Rect::new(
            app.crew_area.x + 1,
            app.input_area.y - 5,
            app.crew_area.width - 1,
            2,
        );
        let visible = text(&buffer, helper);
        let lines = visible
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            lines, expected,
            "crew helper must fit fully inside its region at {w}x{h}"
        );
    }
}
