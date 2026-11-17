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
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Agent Command\n\nUsage: agent-command [--snapshot PATH]");
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
    let updates = crate::update::watch();
    let mut reload = false;
    let mut app = App::default();
    let mut terminal = ratatui::init();
    let result: io::Result<()> = (|| {
        execute!(io::stdout(), EnableMouseCapture)?;
        while !app.quit {
            if std::env::var_os("AGENT_COMMAND_DEV").is_some()
                && crate::source_stamp(&source_root) != source_stamp
            {
                reload = true;
                break;
            }
            if let Some(path) = &path {
                app.poll_theme(path);
            }
            if let Some(latest) = updates.as_ref().and_then(|found| found.try_recv().ok()) {
                app.offer_update(latest);
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
        Ok(())
    })();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if reload {
        std::process::exit(75);
    }
    result?;
    // The terminal is back to normal here, so the command can be read and copied.
    if let Some(latest) = &app.update {
        if !(app.update_accepted && app.self_update) {
            println!("{}", crate::update::notice(latest));
        } else if !crate::update::install(latest) {
            std::process::exit(1);
        }
    }
    Ok(())
}
