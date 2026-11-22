mod auth;
mod cli;
mod mission;
mod usage;
pub use cli::run_cli;

use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

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

struct Live {
    workspace: PathBuf,
    profiles: Vec<auth::Profile>,
    selected_profile: usize,
    profile_area: Rect,
    usage: Option<usage::Usage>,
    usage_checked_at: Option<i64>,
    usage_worker: Option<std::thread::JoinHandle<()>>,
    running_profile: Option<usize>,
    mission: Option<(usize, mission::Mission)>,
    outcomes: [&'static str; 4],
}

impl Live {
    fn refresh_usage(&mut self) {
        let now = usage::now();
        if self.usage_checked_at == Some(now) {
            return;
        }
        self.usage_checked_at = Some(now);
        let displayed = self.running_profile.unwrap_or(self.selected_profile);
        self.usage = self
            .profiles
            .get(displayed)
            .and_then(|profile| usage::Usage::read(profile).ok());
        if self.mission.is_some()
            || self
                .usage_worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        {
            return;
        }
        let due: Vec<_> = self
            .profiles
            .iter()
            .filter(|profile| {
                profile.provider == auth::Provider::Codex
                    && usage::Usage::read(profile).is_ok_and(|usage| usage.codex_due(now))
            })
            .cloned()
            .collect();
        if !due.is_empty() {
            self.usage_worker = Some(std::thread::spawn(move || {
                for profile in due {
                    let _ = usage::refresh_codex(&profile, usage::now());
                }
            }));
        }
    }
}

pub struct App {
    live: Option<Live>,
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
            live: None,
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
    fn live(workspace: PathBuf) -> Self {
        let mut app = Self::default();
        let profiles = auth::root()
            .and_then(|root| auth::profiles(&root))
            .unwrap_or_else(|error| {
                app.notice = format!("Cannot load profiles: {error}");
                Vec::new()
            });
        app.live = Some(Live {
            workspace,
            profiles,
            selected_profile: 0,
            profile_area: Rect::default(),
            usage: None,
            usage_checked_at: None,
            usage_worker: None,
            running_profile: None,
            mission: None,
            outcomes: ["READY"; 4],
        });
        app.log.clear();
        app.log_times.clear();
        app.missions = std::array::from_fn(|_| "Standing by for a new mission".into());
        app
    }

    fn select_profile(&mut self) {
        let Some(live) = &mut self.live else { return };
        if live.mission.is_none() && !live.profiles.is_empty() {
            live.selected_profile = (live.selected_profile + 1) % live.profiles.len();
            live.usage_checked_at = None;
            live.refresh_usage();
        }
    }

    fn status(&self, index: usize) -> &'static str {
        self.live
            .as_ref()
            .map_or(STATUSES[index], |live| live.outcomes[index])
    }

    fn poll_mission(&mut self) {
        if let Some(live) = &mut self.live {
            live.refresh_usage();
        }
        for _ in 0..64 {
            let Some(live) = &mut self.live else { return };
            let Some((agent, running)) = &live.mission else {
                return;
            };
            let agent = *agent;
            match running.events.try_recv() {
                Ok(mission::Event::Running { profile, rotated }) => {
                    live.outcomes[agent] = "RUNNING";
                    live.running_profile = Some(profile);
                    live.usage_checked_at = None;
                    live.refresh_usage();
                    if rotated {
                        let reason = format!(
                            "{} selected because the preferred profile is latched.",
                            live.profiles[profile]
                        );
                        self.notice = reason.clone();
                        self.push_log(format!("SYSTEM  {reason}"));
                    }
                }
                Ok(mission::Event::Output(text)) => {
                    self.push_log(format!("{}  {text}", NAMES[agent]))
                }
                Ok(mission::Event::Finished(result)) => self.finish_mission(agent, result),
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.finish_mission(agent, Err(io::Error::other("Mission worker stopped")));
                }
            }
        }
    }

    fn finish_mission(&mut self, agent: usize, result: io::Result<()>) {
        let live = self.live.as_mut().expect("live mission");
        live.mission = None;
        live.running_profile = None;
        live.usage_checked_at = None;
        live.refresh_usage();
        live.outcomes[agent] = if result.is_ok() { "DONE" } else { "FAILED" };
        self.notice = match result {
            Ok(()) => format!("{} completed the mission.", NAMES[agent]),
            Err(error) => format!("{}: {error}", NAMES[agent]),
        };
    }

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
    fn mission_running(&self) -> bool {
        self.live
            .as_ref()
            .is_some_and(|live| live.mission.is_some())
    }

    pub fn toggle_pause(&mut self) {
        if self
            .live
            .as_ref()
            .and_then(|live| live.mission.as_ref())
            .is_some_and(|(agent, _)| *agent == self.selected)
        {
            self.notice = "This mission is running. Pause is available between missions.".into();
            return;
        }
        self.paused[self.selected] = !self.paused[self.selected];
        self.push_log(format!(
            "{}  {}.",
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
        if command.is_empty() {
            return;
        }
        if let Some(live) = &mut self.live {
            if live.mission.is_some() {
                self.notice = "A mission is already running.".into();
                return;
            }
            if self.paused[self.selected] {
                self.notice = "Resume this agent before sending a mission.".into();
                return;
            }
            let Some(profile) = live.profiles.get(live.selected_profile).cloned() else {
                self.notice = "Sign in with agent-command auth login PROVIDER PROFILE.".into();
                return;
            };
            self.notice = format!("Starting {profile} in {}", live.workspace.display());
            live.outcomes[self.selected] = "STARTING";
            live.mission = Some((
                self.selected,
                mission::Mission::launch(
                    live.profiles.clone(),
                    live.selected_profile,
                    live.workspace.clone(),
                    command.clone(),
                ),
            ));
            self.missions[self.selected] = command;
            self.input.clear();
        } else {
            self.missions[self.selected] = command.clone();
            self.push_log(format!("YOU -> {}  {command}", NAMES[self.selected]));
            self.input.clear();
            self.notice = "Preview mission only; no external dispatch.".into();
        }
    }
    pub fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q' | 'c') if self.mission_running() => {
                    self.notice = "Wait for the running mission before quitting.".into();
                }
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
            KeyCode::F(2) => self.select_profile(),
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
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.profile_area.contains(point))
        {
            self.select_profile();
        } else if self.new_area.contains(point) || self.input_area.contains(point) {
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
                Paragraph::new(if self.live.is_some() {
                    "PRIVATEER RADIO  /  LOCAL"
                } else {
                    "PRIVATEER RADIO  /  LOCAL\n───┤ ◆ ├───  SIGNAL CLEAR\n1200 BAUD  ·  UI PREVIEW"
                })
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
            let labels = if self.live.is_some() {
                ["", "", "", ""]
            } else {
                [
                    "04  AGENTS ONLINE",
                    "02  ACTIVE MISSIONS",
                    "00  NEED ATTENTION",
                    "UI PROTOTYPE  /  SAMPLE DATA",
                ]
            };
            for (i, label) in labels.iter().enumerate() {
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
        let usage_rows = if self.live.is_some() { 2 } else { 0 };
        let main = if compact {
            r(
                1,
                2,
                a.width.saturating_sub(2),
                a.height.saturating_sub(8 + usage_rows),
            )
        } else {
            r(
                3,
                12,
                a.width.saturating_sub(6),
                a.height.saturating_sub(18 + usage_rows),
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
                Line::styled(format!("   {}", self.status(i).to_lowercase()), style),
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
                            self.status(self.selected)
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
        let elapsed = if self.live.is_some() {
            "—"
        } else if self.selected == 0 {
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
        if self.live.is_none() && cols[1].height > 22 {
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
        let keep = (usize::from(feed.height) + 1).div_ceil(3);
        let mut lines: Vec<_> = self
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
        lines.pop();
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let hidden_rows = paragraph
            .line_count(feed.width)
            .saturating_sub(usize::from(feed.height));
        let scroll = u16::try_from(hidden_rows).unwrap_or(u16::MAX);
        f.render_widget(paragraph.scroll((scroll, 0)), feed);
        let iy = a.height.saturating_sub(5);
        if let Some(live) = &mut self.live {
            live.profile_area = r(7, iy.saturating_sub(3), a.width.saturating_sub(14), 1);
            let now = usage::now();
            let selected = live.running_profile.unwrap_or(live.selected_profile);
            let label = live.profiles.get(selected).map_or_else(
                || "No profiles · agent-command auth login PROVIDER PROFILE".into(),
                |profile| {
                    let latch = live.usage.map_or_else(
                        || "State unavailable".into(),
                        |usage| usage.latch_label(now),
                    );
                    let control = if live.running_profile.is_some() {
                        "Running"
                    } else {
                        "F2 / click"
                    };
                    format!("{control} · {latch} · {profile}")
                },
            );
            let [five, week] = live.usage.unwrap_or_default().rows(now);
            f.render_widget(
                Paragraph::new(label).style(Style::default().fg(color(&t.accent))),
                live.profile_area,
            );
            f.render_widget(
                Paragraph::new(vec![Line::raw(five), Line::raw(week)])
                    .style(Style::default().fg(color(&t.muted))),
                r(7, iy.saturating_sub(2), a.width.saturating_sub(14), 2),
            );
        }
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
                if self.live.is_some() {
                    "   Ready"
                } else {
                    "   ● SIGNAL CLEAR    Local demo · commands stay in this preview"
                }
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
    #[cfg(unix)]
    #[test]
    fn live_mission_uses_selected_profile_and_renders_output_and_outcome() {
        use std::{
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };
        let workspace =
            std::env::temp_dir().join(format!("agent-command-mission-{}", std::process::id()));
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("workspace-marker"), "").unwrap();
        let helper = workspace.join("provider");
        fs::write(&helper, r#"#!/bin/sh
case "${1}" in
    auth)
        test -d "$CLAUDE_CONFIG_DIR" && test -z "$CODEX_HOME" || exit 8
        printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
        exit
        ;;
    -c)
        test -d "$CODEX_HOME" && test -z "$CLAUDE_CONFIG_DIR" || exit 8
        shift 4
        if test "$1" = login; then
            printf '%s\n' 'Logged in using ChatGPT' >&2
            exit
        fi
        ;;
esac
test -f workspace-marker && test "$(cat)" = "Mission input" || exit 9
long=
remaining=58
while test "$remaining" -gt 0; do
    long="${long}wide "
    remaining=$((remaining - 1))
done
if test "$1" = --print; then
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$long"
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Claude output"},{"type":"tool_use","input":"internal event"}]}}'
else
    test "$1" = -a && test "$3" = exec || exit 10
    printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$long"
    printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Codex output"}}'
fi
while test ! -f release-mission; do sleep 0.01; done
"#).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let root = workspace.join("profiles");
        let claude = auth::Profile::fixture(&root, auth::Provider::Claude, helper.clone());
        let codex = auth::Profile::fixture(&root, auth::Provider::Codex, helper);

        for (width, height, key, provider) in [
            (80, 24, true, "Claude"),
            (80, 24, false, "Codex"),
            (120, 40, true, "Claude"),
            (120, 40, false, "Codex"),
        ] {
            let mut app = App {
                live: Some(Live {
                    workspace: workspace.clone(),
                    profiles: vec![codex.clone(), claude.clone()],
                    selected_profile: usize::from(!key),
                    profile_area: Rect::default(),
                    usage: None,
                    usage_checked_at: None,
                    usage_worker: None,
                    running_profile: None,
                    mission: None,
                    outcomes: ["READY"; 4],
                }),
                ..App::default()
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let profile = app.live.as_ref().unwrap().profile_area;
            if key {
                app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
            } else {
                app.click(profile.x, profile.y);
            }
            app.input = "Mission input".into();
            if key {
                app.editing = true;
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            } else {
                app.click(app.send_area.x, app.send_area.y);
            }
            wait_for(&mut app, |app| {
                app.log
                    .back()
                    .is_some_and(|line| line.contains(&format!("{provider} output")))
            });
            assert_eq!(app.status(0), "RUNNING");
            terminal.draw(|f| app.draw(f)).unwrap();
            let screen = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                screen.contains(&format!("{provider} output")),
                "{width}×{height}: {provider} tail hidden during streaming"
            );
            assert!(screen.contains("RUNNING"));
            assert!(!screen.contains("internal event"));
            fs::write(workspace.join("release-mission"), "").unwrap();
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);
            terminal.draw(|f| app.draw(f)).unwrap();
            let screen = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(screen.contains("DONE"));
            assert!(
                screen.contains(&format!("{provider} output")),
                "{width}×{height}: {provider} tail hidden after completion"
            );
            fs::remove_file(workspace.join("release-mission")).unwrap();
        }
        fs::remove_dir_all(workspace).unwrap();

        fn wait_for(app: &mut App, ready: impl Fn(&App) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready(app) {
                assert!(Instant::now() < deadline, "{}", app.notice);
                app.poll_mission();
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn usage_panel_shows_selected_allowances_and_latch_with_keyboard_and_mouse() {
        use auth::Provider;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use serde_json::json;
        use usage::{tests::Fixture, Usage};

        let fixture = Fixture::new("usage-panel", "");
        let ready = fixture.profile(Provider::Claude, "ready");
        let latched = fixture.profile(Provider::Claude, "latched");
        let now = usage::now();
        Usage::observe_claude(
            &latched,
            &json!({
                "type":"rate_limit_event",
                "rate_limit_info":{
                    "status":"rejected","resetsAt":now + 7200,
                    "unifiedWindows":{
                        "five_hour":{"utilization":0.25,"resetsAt":now + 7200},
                        "seven_day":{"utilization":0.60,"resetsAt":now + 86400}
                    }
                }
            }),
            now,
        )
        .unwrap();

        for (width, height, key) in [(80, 24, true), (120, 40, false)] {
            let mut app = App {
                live: Some(Live {
                    workspace: fixture.root.clone(),
                    profiles: vec![ready.clone(), latched.clone()],
                    selected_profile: 0,
                    profile_area: Rect::default(),
                    usage: None,
                    usage_checked_at: None,
                    usage_worker: None,
                    running_profile: None,
                    mission: None,
                    outcomes: ["READY"; 4],
                }),
                ..App::default()
            };
            app.poll_mission();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let area = app.live.as_ref().unwrap().profile_area;
            let panel = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
                let buffer = terminal.backend().buffer();
                (area.y..area.y + 3)
                    .map(|y| {
                        (area.x..area.right())
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
            };
            let initial = panel(&terminal);
            assert!(initial[0].contains("No latch · claude/ready"));
            assert!(initial[1].contains("5h unavailable"));
            if key {
                app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
            } else {
                app.click(area.x, area.y);
            }
            terminal.draw(|f| app.draw(f)).unwrap();
            let changed = panel(&terminal);
            assert!(changed[0].contains("Latched") && changed[0].contains("claude/latched"));
            assert!(changed[1].contains("5h 25% used / 75% left, reset"));
            assert!(changed[2].contains("Week 60% used / 40% left, reset"));
        }
    }

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
