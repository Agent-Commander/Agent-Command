use crate::{editor, App};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::Command,
    time::Duration,
};

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
        Some(("update", rest)) => return crate::update::run(rest),
        Some(("triage", rest)) => return crate::triage::run(rest),
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

    install_panic_restore_hook();
    let mut terminal = ratatui::init();
    let result = (|| {
        execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
        let workspace = std::env::current_dir()?;
        let mut app = App::live_fast(workspace);
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

            if std::mem::take(&mut app.full_redraw) {
                terminal.clear()?;
            }
            terminal.draw(|f| app.draw(f))?;

            if event::poll(Duration::from_millis(33))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
                    Event::Paste(text) => app.paste(&text),
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => app.click(m.column, m.row),
                        MouseEventKind::ScrollUp => app.scroll_mouse(m.column, m.row, -3),
                        MouseEventKind::ScrollDown => app.scroll_mouse(m.column, m.row, 3),
                        _ => {}
                    },
                    Event::Resize(_, _) => {
                        app.full_redraw = false;
                    }
                    _ => {}
                }
            }
        }
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
