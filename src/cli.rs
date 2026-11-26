use crate::{editor, token_burn::InlineFrameUpdate, App};
use crossterm::{
    cursor::MoveTo,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::Command,
    time::Duration,
};

const PASTE_COALESCE_POLL: Duration = Duration::from_millis(8);
const MAX_EVENTS_PER_FRAME: usize = 4096;

fn write_inline_update(stdout: &mut io::Stdout, update: &InlineFrameUpdate) {
    if update.is_empty() {
        return;
    }
    for delete in &update.deletes {
        let _ = stdout.write_all(delete.as_bytes());
    }
    for (x, y, payload) in &update.sequences {
        let _ = execute!(stdout, MoveTo(*x, *y));
        let _ = stdout.write_all(payload.as_bytes());
    }
    let _ = stdout.flush();
}

fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
}

fn install_panic_restore_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

fn run_external_editor(initial: &str) -> io::Result<Option<String>> {
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let edited = editor::edit_in_external_editor(initial);
    let _ = enable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    );
    edited
}

fn key_as_paste_piece(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            Some(c)
        }
        KeyCode::Enter if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            Some('\n')
        }
        KeyCode::Tab if key.modifiers.is_empty() => Some('\t'),
        _ => None,
    }
}

fn strip_bracketed_paste_markers(text: &str) -> String {
    let mut out = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
    if out.starts_with("[200~") && out.ends_with("[201~") {
        out = out
            .strip_prefix("[200~")
            .and_then(|s| s.strip_suffix("[201~"))
            .unwrap_or(&out)
            .to_owned();
    }
    out
}

fn is_multiline_paste_burst(events: &[Event]) -> bool {
    let mut active_keys = Vec::new();
    for ev in events {
        if let Event::Key(k) = ev {
            if k.kind != KeyEventKind::Release {
                active_keys.push(*k);
            }
        } else {
            return false;
        }
    }
    if active_keys.len() < 2 {
        return false;
    }
    if !active_keys.iter().all(|k| key_as_paste_piece(*k).is_some()) {
        return false;
    }
    let enter_positions: Vec<usize> = active_keys
        .iter()
        .enumerate()
        .filter_map(|(idx, k)| (k.code == KeyCode::Enter).then_some(idx))
        .collect();
    if enter_positions.len() >= 2 {
        return true;
    }
    if let Some(&first_enter) = enter_positions.first() {
        return first_enter + 1 < active_keys.len();
    }
    false
}

fn drain_input_batch() -> io::Result<Vec<Event>> {
    let mut batch = vec![event::read()?];
    while batch.len() < MAX_EVENTS_PER_FRAME && event::poll(Duration::ZERO)? {
        batch.push(event::read()?);
    }
    let ends_with_text_or_enter = batch.last().is_some_and(|ev| {
        matches!(
            ev,
            Event::Key(k) if k.kind != KeyEventKind::Release && key_as_paste_piece(*k).is_some()
        )
    });
    if ends_with_text_or_enter && batch.len() >= 2 {
        while batch.len() < MAX_EVENTS_PER_FRAME && event::poll(PASTE_COALESCE_POLL)? {
            batch.push(event::read()?);
            while batch.len() < MAX_EVENTS_PER_FRAME && event::poll(Duration::ZERO)? {
                batch.push(event::read()?);
            }
        }
    }
    Ok(batch)
}

pub(crate) fn dispatch_input_batch(app: &mut App, events: Vec<Event>) {
    if is_multiline_paste_burst(&events) {
        let mut combined = String::with_capacity(events.len());
        for ev in events {
            if let Event::Key(k) = ev {
                if k.kind != KeyEventKind::Release {
                    if let Some(ch) = key_as_paste_piece(k) {
                        combined.push(ch);
                    }
                }
            }
        }
        let cleaned = strip_bracketed_paste_markers(&combined);
        if !cleaned.is_empty() {
            app.paste_text_or_image(&cleaned);
        }
        return;
    }

    for ev in events {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
            Event::Paste(text) => {
                let cleaned = strip_bracketed_paste_markers(&text);
                if cleaned.is_empty() {
                    app.paste_from_clipboard();
                } else {
                    app.paste_text_or_image(&cleaned);
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => app.mouse_down(m.column, m.row),
                MouseEventKind::Drag(MouseButton::Left) => app.mouse_drag(m.column, m.row),
                MouseEventKind::Up(MouseButton::Left) => app.mouse_up(m.column, m.row),
                MouseEventKind::ScrollUp => app.scroll_mouse(m.column, m.row, -3),
                MouseEventKind::ScrollDown => app.scroll_mouse(m.column, m.row, 3),
                _ => {}
            },
            Event::Resize(_, _) => {
                app.full_redraw = true;
            }
            _ => {}
        }
        if app.quit {
            break;
        }
    }
}

pub fn run_cli(args: &[String]) -> io::Result<()> {
    crate::update::cleanup_stale_old_binary();
    match args
        .split_first()
        .map(|(first, rest)| (first.as_str(), rest))
    {
        Some(("auth", rest)) => return crate::auth::run(rest),
        Some(("catalog", rest)) => return crate::catalog::run(rest),
        Some(("connect", rest)) => return crate::connect::run(rest),
        Some(("daemon", rest)) => return crate::daemon::run(rest),
        Some(("init" | "onboard", rest)) => return crate::onboard::run(rest),
        Some(("mcp", rest)) => return crate::mcp::run(rest),
        Some(("model", rest)) => return crate::native::run(rest),
        Some(("specialist", rest)) => return crate::specialists::run(rest),
        Some(("triage", rest)) => return crate::triage::run(rest),
        Some(("update", rest)) => return crate::update::run(rest),
        Some(("vault", rest)) => return crate::vault::run(rest),
        _ => {}
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Agent Command\n\n\
             Usage: agent-command [--snapshot PATH]\n\
             \x20      agent-command auth --help\n\
             \x20      agent-command onboard [--status|--enterprise]\n\
             \x20      agent-command catalog {{show|refresh}}\n\
             \x20      agent-command daemon [--bind 127.0.0.1:47100] [--allow-remote] [--print-service [OS]|--install-service]\n\
             \x20      agent-command connect http://HOST:PORT [--token-env VAR] [--thread NAME] [--once]\n\
             \x20      agent-command mcp [--root PATH] [--scope supervised|read-only]\n\
             \x20      agent-command model {{show|pin PROVIDER PROFILE MODEL [EFFORT]|unpin PROVIDER PROFILE}}\n\
             \x20      agent-command specialist {{list|spawn NAME ROLE [--mode MODE] [TASK]|review NAME [--next-revision] [FOCUS]|status NAME|diff NAME|pr NAME [--commit]|settle NAME|unsettle NAME|remove NAME}}\n\
             \x20      agent-command vault {{list|check NAME...|set NAME --stdin|delete NAME|reveal NAME --confirm}}\n\
             \x20      agent-command update\n\
             \x20      agent-command triage [--json] [--pipe claude|codex|gemini]\n\n\
             Interactive Single-Canvas TUI:\n\
             \x20 Composer: Shift+Enter / Alt+Enter / Ctrl+J newline · Enter send/queue · Ctrl+Enter steer · / commands · @ files/models · ! shell · Ctrl+X e ($EDITOR)\n\
             \x20 Navigation: Ctrl+P / Ctrl+K command palette · Ctrl+B toggle right rail · Ctrl+E expand tool/thinking · Ctrl+Y copy reply · PageUp/PageDown/wheel scroll · d diff viewer\n\
             \x20 Controls: Esc×2 cancel turn · Ctrl+C / Ctrl+Q clean exit · F2 cycle profile · F3 settings · F6 update pill · /help command list\n\
             \x20 Existing claude, codex, gemini, and gh logins are auto-adopted passively on boot."
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("agent-command {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        return crate::snapshot(&PathBuf::from(args.get(i + 1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "--snapshot requires a path")
        })?));
    }
    if !args.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unknown argument. Use agent-command --help",
        ));
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("Run Agent Command in an interactive terminal.");
        std::process::exit(2);
    }

    let launch = crate::settings::LaunchConfig::from_env();
    let path = launch.theme_path.clone();
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_stamp = crate::source_stamp(&source_root);
    let mut reload = false;
    let mut exit_message = None;
    let mut relaunch_executable = None;

    let graphics = crate::tui::graphics::probe_terminal_graphics();
    install_panic_restore_hook();
    let mut terminal = ratatui::init();
    let result = (|| {
        execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
        let workspace = std::env::current_dir()?;
        let mut app = App::live_fast(workspace);
        app.graphics = graphics;
        app.attach_fast(Box::new(crate::panel::TerminalHost), launch);
        app.start_background_probes();

        while !app.quit {
            if !app.mission_running()
                && std::env::var_os("AGENT_COMMAND_DEV").is_some()
                && crate::source_stamp(&source_root) != source_stamp
            {
                reload = true;
                break;
            }
            if let Some(path) = &path {
                app.poll_theme(path);
            }
            app.poll_background_events();
            app.poll_mission();
            app.poll_update();

            if let Some(draft) = app.take_external_editor_request() {
                match run_external_editor(&draft) {
                    Ok(Some(updated)) => app.apply_external_editor_text(updated),
                    Ok(None) => app.notice = "External editor cancelled.".into(),
                    Err(error) => app.notice = format!("External editor failed: {error}"),
                }
                app.full_redraw = true;
            }

            let force_redraw = std::mem::take(&mut app.full_redraw);
            if force_redraw {
                terminal.clear()?;
                app.inline_ring_cache.clear();
            }
            app.pending_graphic_rings.clear();
            if let Some(protocol) = app.graphics.inline_protocol() {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, BeginSynchronizedUpdate);
                let draw_result = terminal.draw(|f| app.draw(f));
                if draw_result.is_ok() {
                    let update = app.inline_ring_cache.diff_and_encode(
                        &app.pending_graphic_rings,
                        protocol,
                        app.graphics.cell_px,
                    );
                    write_inline_update(&mut stdout, &update);
                }
                let _ = execute!(stdout, EndSynchronizedUpdate);
                draw_result?;
            } else {
                terminal.draw(|f| app.draw(f))?;
            }

            if event::poll(Duration::from_millis(33))? {
                let batch = drain_input_batch()?;
                if batch.iter().any(|ev| matches!(ev, Event::Resize(_, _))) {
                    app.graphics
                        .remeasure(crate::tui::graphics::window_metrics());
                    app.inline_ring_cache.clear();
                }
                dispatch_input_batch(&mut app, batch);
            }
        }
        write_inline_update(&mut io::stdout(), &app.inline_ring_cache.release());
        app.interrupt_all_runs();
        exit_message = app.take_exit_message();
        relaunch_executable = app.take_relaunch_executable();
        Ok(())
    })();

    restore_terminal();

    if let Some(message) = exit_message {
        println!("{message}");
    }
    if let Some(exe) = relaunch_executable {
        let status = Command::new(&exe).status()?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        return Ok(());
    }
    if reload {
        std::process::exit(75);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_events_from_str(text: &str) -> Vec<Event> {
        text.chars()
            .map(|c| {
                if c == '\n' {
                    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                } else {
                    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                }
            })
            .collect()
    }

    #[test]
    fn multiline_paste_burst_coalesces_without_submitting_and_collapses_large_pastes() {
        let mut app = App::default();
        dispatch_input_batch(&mut app, key_events_from_str("line1\nline2"));
        assert_eq!(app.composer.text(), "line1\nline2");
        assert_eq!(app.log.len(), 1);

        app.composer.clear();
        let fifteen = (1..=15)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        dispatch_input_batch(&mut app, key_events_from_str(&format!("{fifteen}\n")));
        assert!(app.composer.text().contains("[Pasted text #1"));
        assert_eq!(app.composer.pasted_chunks().len(), 1);
        assert!(app.composer.queued_messages().is_empty());
    }

    #[test]
    fn single_line_command_with_trailing_enter_drains_and_submits_immediately() {
        let mut app = App::default();
        dispatch_input_batch(&mut app, key_events_from_str("/help\n"));
        assert!(app.composer.is_empty());
        assert!(app.notice.contains("Command reference"));
    }
}
