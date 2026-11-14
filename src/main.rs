use agent_command::App;
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
fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        return agent_command::snapshot(&PathBuf::from(
            args.get(i + 1).expect("--snapshot requires a path"),
        ));
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("Run Agent Command in an interactive terminal.");
        std::process::exit(2);
    }
    let path = std::env::var_os("AGENT_COMMAND_THEME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("theme.toml"));
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_stamp = agent_command::source_stamp(&source_root);
    let mut reload = false;
    let mut terminal = ratatui::init();
    let result = (|| {
        execute!(io::stdout(), EnableMouseCapture)?;
        let mut app = App::default();
        while !app.quit {
            if std::env::var_os("AGENT_COMMAND_DEV").is_some()
                && agent_command::source_stamp(&source_root) != source_stamp
            {
                reload = true;
                break;
            }
            app.poll_theme(&path);
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
