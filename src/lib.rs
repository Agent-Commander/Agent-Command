mod cli;
pub use cli::run_cli;

use std::{collections::VecDeque, fs, io, path::Path, time::SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};
use serde::Deserialize;

pub const NAMES: [&str; 4] = ["Atlas", "Nova", "Echo", "Orion"];
const STATUSES: [&str; 4] = ["BUILDING", "REVIEWING", "READY", "READY"];
const MISSIONS: [&str; 4] = [
    "Implementing the command center",
    "Reviewing interface components",
    "Standing by for a new mission",
    "Standing by for a new mission",
];

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub background: String,
    pub panel: String,
    pub foreground: String,
    pub muted: String,
    pub accent: String,
    pub ivory: String,
    pub border: String,
    pub selection: String,
    pub amber: String,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "111419".into(),
            panel: "14191f".into(),
            foreground: "c0c6cd".into(),
            muted: "929eaa".into(),
            accent: "9eb8cc".into(),
            ivory: "c9c7ba".into(),
            border: "414d5b".into(),
            selection: "293744".into(),
            amber: "d6ac6a".into(),
        }
    }
}
impl Theme {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let theme: Self = toml::from_str(&text).map_err(|e| e.to_string())?;
        for value in [
            &theme.background,
            &theme.panel,
            &theme.foreground,
            &theme.muted,
            &theme.accent,
            &theme.ivory,
            &theme.border,
            &theme.selection,
            &theme.amber,
        ] {
            if value.len() != 6 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Theme colors must contain six hexadecimal digits".into());
            }
        }
        Ok(theme)
    }
}
pub fn color(hex: &str) -> Color {
    let n = u32::from_str_radix(hex, 16).unwrap_or(0xc0c6cd);
    Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

pub struct App {
    pub selected: usize,
    pub paused: [bool; 4],
    pub missions: [String; 4],
    pub input: String,
    pub editing: bool,
    pub quit: bool,
    pub log: VecDeque<String>,
    pub log_times: VecDeque<String>,
    pub theme: Theme,
    pub notice: String,
    theme_stamp: Option<SystemTime>,
    pub crew_area: Rect,
    pub input_area: Rect,
    pub new_area: Rect,
    pub pause_area: Rect,
    pub send_area: Rect,
}
impl Default for App {
    fn default() -> Self {
        Self {
            selected: 0,
            paused: [false; 4],
            missions: MISSIONS.map(str::to_owned),
            input: String::new(),
            editing: false,
            quit: false,
            log: VecDeque::from([
                "SYSTEM  Command center initialized.".into(),
                "ATLAS   Building the operations console.".into(),
                "NOVA    Interface review in progress.".into(),
                "SYSTEM  Ready for your next command.".into(),
            ]),
            log_times: VecDeque::from(vec![chrono::Local::now().format("%H:%M:%S").to_string(); 4]),
            theme: Theme::default(),
            notice: String::new(),
            theme_stamp: None,
            crew_area: Rect::default(),
            input_area: Rect::default(),
            new_area: Rect::default(),
            pause_area: Rect::default(),
            send_area: Rect::default(),
        }
    }
}
impl App {
    pub fn poll_theme(&mut self, path: &Path) {
        let stamp = fs::metadata(path).and_then(|m| m.modified()).ok();
        if stamp != self.theme_stamp {
            self.theme_stamp = stamp;
            match Theme::load(path) {
                Ok(theme) => {
                    self.theme = theme;
                    self.notice.clear();
                }
                Err(e) => self.notice = format!("Theme unchanged: {e}"),
            }
        }
    }
    fn push_log(&mut self, text: String) {
        self.log.push_back(text);
        self.log_times
            .push_back(chrono::Local::now().format("%H:%M:%S").to_string());
        if self.log.len() > 200 {
            self.log.pop_front();
            self.log_times.pop_front();
        }
    }
    pub fn toggle_pause(&mut self) {
        self.paused[self.selected] = !self.paused[self.selected];
        self.push_log(format!(
            "{}  {} in preview.",
            NAMES[self.selected],
            if self.paused[self.selected] {
                "Paused"
            } else {
                "Resumed"
            }
        ));
    }
    pub fn submit(&mut self) {
        let command = self.input.trim().to_owned();
        if !command.is_empty() {
            self.missions[self.selected] = command.clone();
            self.push_log(format!("YOU -> {}  {command}", NAMES[self.selected]));
            self.input.clear();
            self.notice = "Preview mission only; no external dispatch.".into();
        }
    }
    pub fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q' | 'c') => self.quit = true,
                KeyCode::Char('n') => self.editing = true,
                KeyCode::Char('p') => self.toggle_pause(),
                KeyCode::Char('l') => {
                    self.log.clear();
                    self.log_times.clear();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Tab => self.editing = !self.editing,
            KeyCode::Esc => self.editing = false,
            KeyCode::Enter if self.editing => self.submit(),
            KeyCode::Enter => self.editing = true,
            KeyCode::Backspace if self.editing => {
                self.input.pop();
            }
            KeyCode::Char(c) if self.editing => self.input.push(c),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(3),
            _ => {}
        }
    }
    pub fn click(&mut self, x: u16, y: u16) {
        let point = ratatui::layout::Position::new(x, y);
        if self.new_area.contains(point) || self.input_area.contains(point) {
            self.editing = true;
        } else if self.pause_area.contains(point) {
            self.toggle_pause();
        } else if self.send_area.contains(point) {
            self.submit();
        } else if self.crew_area.contains(point) {
            self.selected = ((y.saturating_sub(self.crew_area.y))
                / (self.crew_area.height / 4).max(1))
            .min(3) as usize;
            self.editing = false;
        }
    }
    pub fn draw(&mut self, f: &mut Frame) {
        let t = self.theme.clone();
        let a = f.area();
        let base = Style::default()
            .fg(color(&t.foreground))
            .bg(color(&t.background));
        f.render_widget(Block::default().style(base), a);
        let r = |x: u16, y: u16, w: u16, h: u16| {
            Rect::new(
                x.min(a.width),
                y.min(a.height),
                w.min(a.width.saturating_sub(x)),
                h.min(a.height.saturating_sub(y)),
            )
        };
        f.render_widget(
            Paragraph::new(
                "  ▣  AGENT COMMAND     File   Crew   Missions   Radio    |    PRIVATEER / 01",
            )
            .style(
                Style::default()
                    .fg(color("272c32"))
                    .bg(color(&t.ivory))
                    .bold(),
            ),
            r(0, 0, a.width, 1),
        );
        let compact = a.height < 36 || a.width < 105;
        if !compact {
            let banner = if a.width >= 105 {
                banner()
            } else {
                "AGENT COMMAND".into()
            };
            f.render_widget(
                Paragraph::new(banner).style(Style::default().fg(color("dad8ca")).bold()),
                r(3, 2, a.width.saturating_sub(41), 3),
            );
            f.render_widget(
            Paragraph::new(
                "PRIVATEER RADIO  /  LOCAL\n───┤ ◆ ├───  SIGNAL CLEAR\n1200 BAUD  ·  UI PREVIEW",
            )
            .alignment(ratatui::layout::Alignment::Right)
            .style(Style::default().fg(color(&t.muted))),
            r(a.width.saturating_sub(38), 2, 35, 3),
        );
            let mut stripe = ["89a76f", "dbc77b", "cc955d", "b86d67", "9c829f", "7d9fb6"]
                .map(|c| Span::styled("━━", Style::default().fg(color(c))))
                .to_vec();
            stripe.push(Span::raw("  INDEPENDENT OPERATORS. UNCHARTED WATERS."));
            f.render_widget(
                Paragraph::new(Line::from(stripe)).style(Style::default().fg(color("818d99"))),
                r(3, 6, a.width.saturating_sub(6), 1),
            );
            let ma = r(3, 8, a.width.saturating_sub(6), 3);
            f.render_widget(
                Block::default().style(Style::default().bg(color("191e25"))),
                ma,
            );
            let metrics = Layout::horizontal([Constraint::Fill(1); 4]).split(ma);
            for (i, label) in [
                "04  AGENTS ONLINE",
                "02  ACTIVE MISSIONS",
                "00  NEED ATTENTION",
                "UI PROTOTYPE  /  SAMPLE DATA",
            ]
            .iter()
            .enumerate()
            {
                f.render_widget(
                    Paragraph::new(*label)
                        .alignment(ratatui::layout::Alignment::Center)
                        .style(Style::default().fg(color(if i == 3 {
                            &t.amber
                        } else {
                            &t.foreground
                        }))),
                    Rect::new(metrics[i].x, metrics[i].y + 1, metrics[i].width, 1),
                );
            }
        }
        let main = if compact {
            r(1, 2, a.width.saturating_sub(2), a.height.saturating_sub(8))
        } else {
            r(
                3,
                12,
                a.width.saturating_sub(6),
                a.height.saturating_sub(18),
            )
        };
        let cols = Layout::horizontal([
            Constraint::Length(if compact { 18 } else { 25 }),
            Constraint::Min(32),
            Constraint::Fill(1),
        ])
        .spacing(1)
        .split(main);
        for (i, title) in ["01 / CREW", "02 / CHART ROOM", "03 / SHIP'S LOG"]
            .iter()
            .enumerate()
        {
            f.render_widget(
                Block::bordered()
                    .style(Style::default().bg(color(&t.panel)))
                    .border_style(Style::default().fg(color(&t.border))),
                cols[i],
            );
            f.render_widget(
                Paragraph::new(*title).style(Style::default().fg(color("cbc9bc")).bold()),
                r(
                    cols[i].x + 2,
                    cols[i].y + 1,
                    cols[i].width.saturating_sub(4),
                    1,
                ),
            );
        }
        self.crew_area = r(
            cols[0].x + 2,
            cols[0].y + 3,
            cols[0].width.saturating_sub(4),
            if compact {
                8.min(cols[0].height.saturating_sub(4))
            } else {
                16.min(cols[0].height.saturating_sub(7))
            },
        );
        let stride = (self.crew_area.height / 4).max(1);
        for (i, name) in NAMES.iter().enumerate() {
            let y = self.crew_area.y + i as u16 * stride;
            if y + stride > self.crew_area.bottom() {
                break;
            }
            let row = r(self.crew_area.x, y, self.crew_area.width, stride);
            if i == self.selected {
                f.render_widget(
                    Block::default().style(Style::default().bg(color(&t.selection))),
                    row,
                );
            }
            let style = if i == self.selected {
                Style::default().fg(color("d1dce4")).bold()
            } else {
                Style::default()
            };
            let lines = vec![
                Line::from(vec![
                    Span::styled(
                        " ●",
                        Style::default().fg(color(if i == 1 { &t.amber } else { &t.accent })),
                    ),
                    Span::styled(format!(" {name}  "), style),
                    Span::styled(
                        format!("/{:02}", i + 1),
                        Style::default().fg(color(if i == self.selected {
                            "97a3ad"
                        } else {
                            "5f646b"
                        })),
                    ),
                ]),
                Line::styled(format!("   {}", STATUSES[i].to_lowercase()), style),
            ];
            f.render_widget(
                Paragraph::new(lines),
                r(row.x, row.y + u16::from(!compact), row.width, 2),
            );
        }
        f.render_widget(
            Paragraph::new(if compact {
                "↑ ↓ select\nEnter: edit"
            } else {
                "↑ ↓ select an agent\nEnter to inspect"
            })
            .style(Style::default().fg(color("666f78"))),
            r(
                cols[0].x + 3,
                cols[0].bottom().saturating_sub(4),
                cols[0].width.saturating_sub(5),
                2,
            ),
        );
        let x = cols[1].x + 2;
        let y = cols[1].y + 3;
        let w = cols[1].width.saturating_sub(4);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{}  ", NAMES[self.selected]),
                    Style::default().bold(),
                ),
                Span::styled(
                    format!(
                        "● {}",
                        if self.paused[self.selected] {
                            "PAUSED"
                        } else {
                            STATUSES[self.selected]
                        }
                    ),
                    Style::default()
                        .fg(color(if self.selected == 1 {
                            &t.amber
                        } else {
                            &t.accent
                        }))
                        .bold(),
                ),
            ])),
            r(x, y, w, 1),
        );
        let elapsed = if self.selected == 0 {
            "12m 34s"
        } else if self.selected == 1 {
            "04m 18s"
        } else {
            "—"
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    self.missions[self.selected].clone(),
                    Style::default().fg(color("d4d9df")),
                ),
                Line::styled(
                    format!("AGENT {:02}  /  ELAPSED {elapsed}", self.selected + 1),
                    Style::default().fg(color(&t.muted)),
                ),
            ])
            .wrap(Wrap { trim: false }),
            r(x, y + 2, w, 4),
        );
        if cols[1].height > 22 {
            f.render_widget(
                Paragraph::new(vec![
                    Line::raw("WORKSPACE"),
                    Line::raw("Agent-Commander UI"),
                    Line::raw(""),
                    Line::raw("CURRENT STEP"),
                    Line::raw("Refining layout and interaction"),
                    Line::raw(""),
                    Line::raw("PROGRESS"),
                    Line::from(vec![
                        Span::styled("━━━━━━━━━━━━━━━━", Style::default().fg(color(&t.accent))),
                        Span::styled("━━━━━━━━", Style::default().fg(color("273335"))),
                        Span::raw("  68%"),
                    ]),
                ])
                .style(Style::default().fg(color(&t.muted))),
                r(x, y + 6, w, 8),
            );
        }
        let by = cols[1].bottom().saturating_sub(5);
        self.new_area = r(x, by, 15.min(w), 3);
        self.pause_area = r(x + 16, by, 12.min(w.saturating_sub(16)), 3);
        fn button(text: &str, primary: bool, t: &Theme) -> Paragraph<'static> {
            Paragraph::new(format!(" {text}"))
                .block(Block::default().padding(ratatui::widgets::Padding::new(0, 0, 1, 0)))
                .style(if primary {
                    Style::default()
                        .fg(color("242b32"))
                        .bg(color(&t.ivory))
                        .bold()
                } else {
                    Style::default()
                        .fg(color(&t.foreground))
                        .bg(color("28323d"))
                        .bold()
                })
        }
        f.render_widget(button("+ New mission", true, &t), self.new_area);
        f.render_widget(
            button(
                if self.paused[self.selected] {
                    "▶ Resume"
                } else {
                    "Ⅱ Pause"
                },
                false,
                &t,
            ),
            self.pause_area,
        );
        let feed = r(
            cols[2].x + 2,
            cols[2].y + 3,
            cols[2].width.saturating_sub(5),
            cols[2].height.saturating_sub(6),
        );
        let keep = usize::from(feed.height / 3).max(1);
        let lines: Vec<_> = self
            .log
            .iter()
            .zip(self.log_times.iter())
            .skip(self.log.len().saturating_sub(keep))
            .flat_map(|(entry, stamp)| {
                let (source, message) = entry.split_once("  ").unwrap_or(("SYSTEM", entry));
                [
                    Line::from(vec![
                        Span::styled(format!("{stamp}  "), Style::default().fg(color("666f78"))),
                        Span::styled(source.to_owned(), Style::default().fg(color(&t.accent))),
                    ]),
                    Line::raw(message.trim().to_owned()),
                    Line::raw(""),
                ]
            })
            .collect();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), feed);
        let iy = a.height.saturating_sub(5);
        f.render_widget(
            Paragraph::new("❯").style(Style::default().fg(color("b8ccd9"))),
            r(4, iy + 1, 1, 1),
        );
        self.input_area = r(7, iy, a.width.saturating_sub(23), 3);
        self.send_area = r(a.width.saturating_sub(15), iy, 12, 3);
        f.render_widget(
            Paragraph::new(format!(
                " {}",
                if self.input.is_empty() {
                    "Give your selected agent a mission…"
                } else {
                    &self.input
                }
            ))
            .block(
                Block::bordered().border_style(Style::default().fg(color(if self.editing {
                    "b8ccd9"
                } else {
                    "455566"
                }))),
            )
            .style(
                Style::default()
                    .fg(color(if self.input.is_empty() {
                        "666f78"
                    } else {
                        "c0c6cd"
                    }))
                    .bg(color("191f27")),
            ),
            self.input_area,
        );
        f.render_widget(button("Send ↗", true, &t), self.send_area);
        f.render_widget(
            Paragraph::new(if self.notice.is_empty() {
                "   ● SIGNAL CLEAR    Local demo · commands stay in this preview"
            } else {
                &self.notice
            })
            .style(Style::default().fg(color("a1adb8"))),
            r(0, a.height.saturating_sub(2), a.width, 1),
        );
        f.render_widget(
            Paragraph::new(" ^Q Quit    ^N New mission    ^P Pause / resume    ^L Clear feed")
                .style(Style::default().bg(color("1b2027")).fg(color("a5b1bd"))),
            r(0, a.height.saturating_sub(1), a.width, 1),
        );
    }
}

pub fn banner() -> String {
    let glyph = |c| match c {
        'A' => [" ▄▄ ", "█▄▄█", "█  █"],
        'G' => [" ▄▄▄", "█ ▄▄", "█▄▄█"],
        'E' => ["▄▄▄▄", "█▄▄ ", "█▄▄▄"],
        'N' => ["▄  ▄", "██ █", "█ ██"],
        'T' => ["▄▄▄▄", " ▐▌ ", " ▐▌ "],
        'C' => [" ▄▄▄", "█   ", "█▄▄▄"],
        'O' => [" ▄▄ ", "█  █", "▀▄▄▀"],
        'M' => ["▄   ▄", "█▀▄▀█", "█   █"],
        'D' => ["▄▄▄ ", "█  █", "█▄▄▀"],
        _ => ["  ", "  ", "  "],
    };
    (0..3)
        .map(|row| {
            "AGENT COMMAND"
                .chars()
                .map(|c| glyph(c)[row])
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn snapshot(path: &Path) -> io::Result<()> {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(150, 46)).unwrap();
    terminal.draw(|f| App::default().draw(f)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut svg=String::from("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1500 920\"><rect width=\"100%\" height=\"100%\" fill=\"#111419\"/>");
    let css_color = |c: Color| match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#c0c6cd".into(),
    };
    // Paint backgrounds before text to avoid adjacent cells clipping glyphs.
    for y in 0..46 {
        for x in 0..150 {
            let cell = &buffer[(x, y)];
            svg.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"10.1\" height=\"20.1\" fill=\"{}\"/>",
                x * 10,
                y * 20,
                css_color(cell.bg)
            ));
        }
    }
    for y in 0..46 {
        for x in 0..150 {
            let cell = &buffer[(x, y)];
            let symbol = cell
                .symbol()
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let weight = if cell.modifier.contains(ratatui::style::Modifier::BOLD) {
                "bold"
            } else {
                "normal"
            };
            svg.push_str(&format!("<text x=\"{}\" y=\"{}\" font-family=\"monospace\" font-weight=\"{weight}\" font-size=\"16\" textLength=\"10\" lengthAdjust=\"spacingAndGlyphs\" fill=\"{}\">{symbol}</text>",x*10,y*20+16,css_color(cell.fg)));
        }
    }
    svg.push_str("</svg>");
    fs::write(path, svg)
}

/// Latest source modification time, used to exit cleanly before a development rebuild.
pub fn source_stamp(root: &Path) -> SystemTime {
    fn walk(path: &Path) -> SystemTime {
        if path.is_dir() {
            fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| walk(&entry.path()))
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH)
        } else {
            fs::metadata(path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        }
    }
    [
        root.join("src"),
        root.join("Cargo.toml"),
        root.join("Cargo.lock"),
    ]
    .iter()
    .map(|p| walk(p))
    .max()
    .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controls_and_unicode() {
        let mut a = App::default();
        a.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        a.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(a.paused[1]);
        a.editing = true;
        for c in "Build λ".chars() {
            a.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        a.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        a.submit();
        assert_eq!(a.missions[1], "Build");
        assert!(a.input.is_empty());
    }
    #[test]
    fn layouts() {
        for (w, h) in [(80, 24), (120, 40), (150, 46), (210, 54)] {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            let mut a = App::default();
            t.draw(|f| a.draw(f)).unwrap();
            assert!(a.input_area.bottom() <= h);
            let text = t
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("AGENT COMMAND"));
        }
    }
    #[test]
    fn theme_reload_retains_valid_theme() {
        let path =
            std::env::temp_dir().join(format!("agent-command-theme-{}.toml", std::process::id()));
        fs::write(&path, "accent = '123456'").unwrap();
        let mut a = App::default();
        a.poll_theme(&path);
        assert_eq!(a.theme.accent, "123456");
        fs::write(&path, "accent = 'invalid'").unwrap();
        a.theme_stamp = None;
        a.poll_theme(&path);
        assert_eq!(a.theme.accent, "123456");
        assert!(!a.notice.is_empty());
        fs::remove_file(path).unwrap();
    }
}
