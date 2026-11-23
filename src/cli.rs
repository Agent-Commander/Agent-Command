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
    match args
        .split_first()
        .map(|(first, rest)| (first.as_str(), rest))
    {
        Some(("auth", rest)) => return crate::auth::run(rest),
        Some(("catalog", rest)) => return crate::catalog::run(rest),
        Some(("model", rest)) => return crate::native::run(rest),
        Some(("specialist", rest)) => return crate::specialists::run(rest),
        Some(("update", rest)) => return crate::update::run(rest),
        _ => {}
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Agent Command\n\nUsage: agent-command [--snapshot PATH]\n       agent-command auth --help\n       agent-command catalog {{show|refresh}}\n       agent-command model {{show|pin PROVIDER PROFILE MODEL [EFFORT]|unpin PROVIDER PROFILE}}\n       agent-command specialist {{list|spawn NAME ROLE [TASK]|status NAME|remove NAME}}\n       agent-command update\n\nTerminal UI: /spawn NAME ROLE [TASK], /ask NAME TASK, /roster, /remove NAME, /entry PROVIDER/PROFILE|auto, F6 dismisses the update banner\n\nSet AGENT_COMMAND_NO_UPDATE_CHECK=1 to skip the launch update check.");
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
    let path = std::env::var_os("AGENT_COMMAND_THEME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("AGENT_COMMAND_DEV")
                .map(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("theme.toml"))
        })
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .map(|p| p.join("agent-command/theme.toml"))
        })
        .or_else(|| {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .map(|p| p.join("agent-command/theme.toml"))
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|p| p.join(".config/agent-command/theme.toml"))
        });

    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_stamp = crate::source_stamp(&source_root);
    let mut reload = false;
    let mut terminal = ratatui::init();
    let result = (|| {
        execute!(io::stdout(), EnableMouseCapture)?;
        let mut app = App::live(std::env::current_dir()?);
        app.watch_updates(crate::update::watch());
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
        Ok(())
    })();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if reload {
        std::process::exit(75);
    }
    result
}
