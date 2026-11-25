use crate::App;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton,
        MouseEventKind,
    },
    execute,
};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    time::Duration,
};
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
        println!("Agent Command\n\nUsage: agent-command [--snapshot PATH]\n       agent-command auth --help\n       agent-command catalog {{show|refresh}}\n       agent-command daemon [--bind 127.0.0.1:47100] [--allow-remote] [--print-service [OS]|--install-service]\n       agent-command connect http://HOST:PORT [--token-env VAR] [--thread NAME] [--once]\n       agent-command mcp [--root PATH] [--scope supervised|read-only]\n       agent-command model {{show|pin PROVIDER PROFILE MODEL [EFFORT]|unpin PROVIDER PROFILE}}\n       agent-command specialist {{list|spawn NAME ROLE [--mode MODE] [TASK]|review NAME [--next-revision] [FOCUS]|status NAME|diff NAME|pr NAME [--commit]|settle NAME|unsettle NAME|remove NAME}}\n       agent-command update\n       agent-command triage [--json] [--pipe claude|codex]\n\nTerminal UI: /spawn NAME ROLE [--mode MODE] [TASK], /ask NAME TASK, /review NAME [--next-revision] [FOCUS], /roster, /diff [NAME], /pr [NAME] [--commit], /settle NAME, /unsettle NAME, /settled, /remove NAME, /entry PROVIDER/PROFILE|auto, /settings, /connect, /disconnect [PROVIDER[/PROFILE]], /reconnect [PROVIDER[/PROFILE]], /update, /mode MODE, /btw QUESTION (/side), /goal CRITERIA|cancel\nKeys: F2 cycles the Commander profile, F3 opens Settings, F6 or the bottom-right Update button installs a newer release, F7 shows or hides settled specialists. Click a subscription card to reconnect or disconnect it.\nModes: read-only, supervised, auto-edits, worktree-auto, workspace-auto (Commander default). Target one delegation with @opus, @fable, @astra, @sol, @claude or @codex.\nFirst run with no signed-in profile opens Connect providers.\n\nThe launch update check can be turned off in Settings (settings.toml); AGENT_COMMAND_NO_UPDATE_CHECK=1 always skips it.");
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
    let mut terminal = ratatui::init();
    let result = (|| {
        execute!(io::stdout(), EnableMouseCapture)?;
        let mut app = App::live(std::env::current_dir()?);
        app.attach(Box::new(crate::panel::TerminalHost), launch);
        app.watch_updates(crate::update::watch(app.launch_update_check()));
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
            app.poll_mission();
            app.poll_update();
            if std::mem::take(&mut app.full_redraw) {
                terminal.clear()?;
            }
            terminal.draw(|f| app.draw(f))?;
            if event::poll(Duration::from_millis(150))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
                    Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                        app.click(m.column, m.row)
                    }
                    _ => {}
                }
            }
        }
        exit_message = app.take_exit_message();
        Ok(())
    })();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some(message) = exit_message {
        println!("{message}");
    }
    if reload {
        std::process::exit(75);
    }
    result
}
