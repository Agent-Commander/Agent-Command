mod auth;
mod catalog;
mod checkpoint;
mod cli;
mod mission;
mod native;
mod routing;
mod specialists;
pub mod token_burn;
pub mod update;
mod usage;
pub use cli::run_cli;
pub use specialists::{Entry, Member, Status, COMMANDER};

use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError},
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

const PREVIEW_NOTICE: &str = "Preview only; no external dispatch.";

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

fn profile_route(
    label: &str,
    ranked: &[routing::Candidate],
    exclusions: &[routing::Exclusion],
) -> ProfileRoute {
    if let Some(candidate) = ranked.iter().find(|candidate| candidate.label == label) {
        return ProfileRoute::Routed {
            model: candidate.configuration.label(),
            metric: routing::score_label(candidate),
        };
    }
    let reason = exclusions
        .iter()
        .find(|exclusion| exclusion.label == label)
        .map_or("below routing floor", |exclusion| exclusion.reason);
    ProfileRoute::Excluded(reason.into())
}

fn deck_card(
    label: &str,
    allowance: usage::Allowance,
    route: Option<&ProfileRoute>,
) -> token_burn::AccountCard {
    let (provider, profile) = label.split_once('/').unwrap_or((label, ""));
    let (route_model, route_metric) = match route {
        _ if allowance.latched => (None, Some("latched".to_owned())),
        Some(ProfileRoute::Routed { model, metric }) => (Some(model.clone()), Some(metric.clone())),
        Some(ProfileRoute::Excluded(reason)) => (None, Some(reason.clone())),
        None => (None, None),
    };
    token_burn::AccountCard {
        provider: provider.into(),
        profile: profile.into(),
        tier: String::new(),
        primary_remaining: allowance.five,
        secondary_remaining: allowance.week,
        seconds_until_reset: allowance.reset_in,
        route_model,
        route_metric,
    }
}

fn deck_palette(path: &Path) -> token_burn::TokenBurnPalette {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| token_burn::TokenBurnPalette::from_theme_toml(&text).ok())
        .unwrap_or_default()
}

fn deck_mode(area: Rect, compact_layout: bool) -> token_burn::DeckMode {
    if compact_layout {
        token_burn::DeckMode::Compact
    } else {
        token_burn::DeckMode::for_area(area)
    }
}

const COMPACT_DECK_MAX_ROWS: usize = 3;
const FULL_DECK_MAX_CARDS: usize = 3;
const UPDATE_DISMISS_HINT: &str = " ✕ F6 ";

fn framed(mut request: routing::Request, frame: impl Fn(&str) -> String) -> routing::Request {
    request.units = request.units.iter().map(|unit| frame(unit)).collect();
    request
}

#[derive(Clone, Debug, PartialEq)]
enum ProfileRoute {
    Routed { model: String, metric: String },
    Excluded(String),
}

struct Live {
    workspace: PathBuf,
    profiles: Vec<auth::Profile>,
    profile_root: Option<PathBuf>,
    preferred: Option<usize>,
    pins: native::Pins,
    last_routed: Option<usize>,
    profile_area: Rect,
    usage: Option<usage::Usage>,
    usage_checked_at: Option<u64>,
    usage_worker: Option<std::thread::JoinHandle<()>>,
    running_profile: Option<usize>,
    continuation: Option<String>,
    pending: Option<(usize, checkpoint::Claim)>,
    abandon_area: Rect,
    mission: Option<(String, mission::Mission)>,
    store: Option<specialists::Store>,
    queue: VecDeque<(String, String)>,
    reply: String,
    entry: Option<(String, &'static str)>,
    routes: Vec<ProfileRoute>,
    allowances: Vec<usage::Allowance>,
}

impl Live {
    fn new(
        workspace: PathBuf,
        profiles: Vec<auth::Profile>,
        profile_root: Option<PathBuf>,
    ) -> Self {
        Self {
            workspace,
            profiles,
            store: profile_root.clone().map(specialists::Store::new),
            profile_root,
            preferred: None,
            pins: native::Pins::default(),
            last_routed: None,
            profile_area: Rect::default(),
            usage: None,
            usage_checked_at: None,
            usage_worker: None,
            running_profile: None,
            continuation: None,
            pending: None,
            abandon_area: Rect::default(),
            mission: None,
            queue: VecDeque::new(),
            reply: String::new(),
            entry: None,
            routes: Vec::new(),
            allowances: Vec::new(),
        }
    }

    fn refresh_routes(&mut self) {
        let (candidates, exclusions) =
            routing::candidates(&self.profiles, &self.catalog(), &self.pins, usage::now());
        let request = routing::Request::default();
        let ranked = routing::rank(candidates, request.policy, request.difficulty);
        let ranked = ranked.as_deref().unwrap_or_default();
        self.routes = self
            .profiles
            .iter()
            .map(|profile| profile_route(&profile.to_string(), ranked, &exclusions))
            .collect();
        self.entry = self.choose_entry(ranked);
    }

    fn choose_entry(&self, ranked: &[routing::Candidate]) -> Option<(String, &'static str)> {
        if let Some(index) = self.preferred {
            return Some((self.profiles[index].to_string(), "override"));
        }
        match ranked.first() {
            Some(best) => Some((
                format!("{} {}", best.label, best.configuration.label()),
                "auto",
            )),
            None => self
                .profiles
                .first()
                .map(|profile| (profile.to_string(), "first signed-in")),
        }
    }

    fn read_allowances(&mut self, now: usage::Tick) {
        self.allowances = self
            .profiles
            .iter()
            .map(|profile| {
                usage::Usage::read(profile)
                    .map(|usage| usage.allowance(now))
                    .unwrap_or_default()
            })
            .collect();
    }

    fn deck_cards(&self) -> Vec<token_burn::AccountCard> {
        self.profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                deck_card(
                    &profile.to_string(),
                    self.allowances.get(index).copied().unwrap_or_default(),
                    self.routes.get(index),
                )
            })
            .collect()
    }

    fn entry_label(&self) -> String {
        self.entry.as_ref().map_or_else(
            || "no signed-in profile".into(),
            |(label, source)| format!("{label} ({source})"),
        )
    }

    fn route(&self, request: routing::Request) -> mission::Route {
        mission::Route {
            request,
            preferred: self.preferred.map(|index| self.profiles[index].to_string()),
            catalog: self.catalog(),
            pins: self.pins.clone(),
        }
    }

    fn displayed_profile(&self) -> usize {
        self.running_profile
            .or(self.preferred)
            .or(self.last_routed)
            .unwrap_or(0)
    }

    fn load_override(&mut self) -> Option<String> {
        let root = self.profile_root.as_ref()?;
        let label = match auth::existing_private_state(root, routing::STATE_DIRECTORY)
            .and_then(|state| state.map_or(Ok(None), |state| routing::load_override(&state)))
        {
            Ok(label) => label?,
            Err(error) => {
                return Some(format!(
                    "Cannot read routing override: {error}. Using Auto."
                ))
            }
        };
        self.preferred = self
            .profiles
            .iter()
            .position(|profile| profile.to_string() == label);
        self.preferred
            .is_none()
            .then(|| format!("Routing override {label} is not an available profile. Using Auto."))
    }

    fn load_pins(&mut self) -> Option<String> {
        let root = self.profile_root.as_ref()?;
        match native::load_existing(root) {
            Ok(pins) => {
                self.pins = pins;
                None
            }
            Err(error) => Some(format!(
                "Cannot read model pins: {error}. Pins ignored; aliases stay unscored."
            )),
        }
    }

    fn pin_label(&self, index: usize) -> String {
        self.pins
            .get(&self.profiles[index].to_string())
            .map_or_else(
                || "no model pin".into(),
                |pin| format!("pin {}", pin.label()),
            )
    }

    fn save_override(&self) -> io::Result<()> {
        let root = self
            .profile_root
            .as_ref()
            .ok_or_else(|| io::Error::other("No profile root"))?;
        let label = self.preferred.map(|index| self.profiles[index].to_string());
        routing::save_override(&routing::state_directory(root)?, label.as_deref())
    }

    fn catalog(&self) -> catalog::Catalog {
        let state = self.profile_root.as_ref().and_then(|root| {
            auth::existing_private_state(root, routing::STATE_DIRECTORY)
                .ok()
                .flatten()
        });
        catalog::Catalog::load(state.as_deref())
    }

    fn route_mode(&self) -> &'static str {
        if self.preferred.is_some() {
            "Override"
        } else {
            "Auto"
        }
    }

    fn refresh_usage(&mut self) {
        let now = usage::now();
        if self.usage_checked_at == Some(now.elapsed / 1_000_000_000) {
            return;
        }
        self.usage_checked_at = Some(now.elapsed / 1_000_000_000);
        self.read_allowances(now);
        let displayed = self.displayed_profile();
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
    pub crew: Vec<Member>,
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
    crew_row_height: u16,
    crew_offset: usize,
    pub deck_palette: token_burn::TokenBurnPalette,
    update_receiver: Option<Receiver<String>>,
    pub update_banner: Option<String>,
    pub update_area: Rect,
}
impl Default for App {
    fn default() -> Self {
        Self {
            live: None,
            selected: 0,
            crew: vec![Member::commander()],
            input: String::new(),
            editing: false,
            quit: false,
            log: VecDeque::from([
                "SYSTEM  Commander online. Message Commander, or /spawn NAME ROLE [TASK]."
                    .to_owned(),
            ]),
            log_times: VecDeque::from([chrono::Local::now().format("%H:%M:%S").to_string()]),
            theme: Theme::default(),
            notice: String::new(),
            theme_stamp: None,
            crew_area: Rect::default(),
            input_area: Rect::default(),
            new_area: Rect::default(),
            pause_area: Rect::default(),
            send_area: Rect::default(),
            crew_row_height: 1,
            crew_offset: 0,
            deck_palette: token_burn::TokenBurnPalette::default(),
            update_receiver: None,
            update_banner: None,
            update_area: Rect::default(),
        }
    }
}
impl App {
    fn live(workspace: PathBuf) -> Self {
        let mut notice = String::new();
        let root = auth::root();
        let profiles = root
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
            .and_then(|root| auth::profiles(root))
            .unwrap_or_else(|error| {
                notice = format!("Cannot load profiles: {error}");
                Vec::new()
            });
        let pending = checkpoint::Claim::pending(&profiles, &workspace).unwrap_or_else(|error| {
            notice = format!("Cannot read continuation state: {error}");
            None
        });
        let mut live = Live::new(workspace, profiles, root.ok());
        live.pending = pending;
        if let Some(message) = live.load_pins() {
            notice = message;
        }
        if let Some(message) = live.load_override() {
            notice = message;
        }
        let mut app = Self::from_live(live);
        if app.notice.is_empty() {
            app.notice = notice;
        }
        app
    }

    fn from_live(mut live: Live) -> Self {
        let mut app = Self::default();
        app.log.clear();
        app.log_times.clear();
        if let Some(store) = &live.store {
            match store.load_commander() {
                Ok(entries) => {
                    for entry in entries {
                        app.log.push_back(entry.text);
                        app.log_times.push_back(entry.at);
                    }
                }
                Err(error) => app.notice = format!("Cannot read Commander transcript: {error}"),
            }
            match store.load_roster() {
                Ok(members) => app.crew.extend(members),
                Err(error) => app.notice = format!("Cannot read specialist roster: {error}"),
            }
        }
        live.refresh_routes();
        live.read_allowances(usage::now());
        app.live = Some(live);
        app
    }

    fn select_profile(&mut self) {
        let Some(live) = &mut self.live else { return };
        if live.mission.is_some() || live.profiles.is_empty() {
            return;
        }
        live.preferred = match live.preferred {
            None => Some(0),
            Some(index) if index + 1 < live.profiles.len() => Some(index + 1),
            Some(_) => None,
        };
        live.usage_checked_at = None;
        live.refresh_usage();
        live.refresh_routes();
        self.notice = match live.save_override() {
            Ok(()) => match live.preferred {
                Some(index) => format!("Override saved: {}", live.profiles[index]),
                None => "Auto routing: highest-ranked eligible profile".into(),
            },
            Err(error) => format!("Override not saved: {error}"),
        };
    }

    fn status(&self, index: usize) -> &'static str {
        self.crew[index].status.label()
    }

    fn poll_mission(&mut self) {
        if let Some(live) = &mut self.live {
            live.refresh_usage();
        }
        for _ in 0..64 {
            let Some((name, event)) = self.next_event() else {
                return;
            };
            self.handle_event(&name, event);
        }
    }

    fn next_event(&mut self) -> Option<(String, mission::Event)> {
        let (name, running) = self.live.as_ref()?.mission.as_ref()?;
        let event = match running.events.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                mission::Event::Finished(Err(io::Error::other("Mission worker stopped")))
            }
        };
        Some((name.clone(), event))
    }

    fn handle_event(&mut self, name: &str, event: mission::Event) {
        let index = specialists::find(&self.crew, name).unwrap_or(0);
        let live = self.live.as_mut().expect("live mission");
        match event {
            mission::Event::Running { profile, reason } => {
                self.crew[index].status = Status::Running;
                live.running_profile = Some(profile);
                live.last_routed = Some(profile);
                live.usage_checked_at = None;
                live.refresh_usage();
                if let Some(reason) = reason {
                    self.notice = reason.clone();
                    self.push_log(format!("SYSTEM  {reason}"));
                }
            }
            mission::Event::Continuing { from, to } => {
                let link = format!("Limit: {} → {}", live.profiles[from], live.profiles[to]);
                live.continuation = Some(link.clone());
                live.pending = None;
                self.notice = link.clone();
                self.push_log(format!("SYSTEM  {link}"));
            }
            mission::Event::Output(text) => {
                live.reply.push_str(&text);
                live.reply.push('\n');
                for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
                    self.record(index, format!("{name}  {line}"));
                }
            }
            mission::Event::Finished(result) => self.finish_mission(index, result),
        }
    }

    fn finish_mission(&mut self, index: usize, result: io::Result<()>) {
        let live = self.live.as_mut().expect("live mission");
        live.mission = None;
        if live.pending.is_some() {
            if let Ok(pending) = checkpoint::Claim::pending(&live.profiles, &live.workspace) {
                live.pending = pending;
            }
        }
        live.running_profile = None;
        live.usage_checked_at = None;
        live.refresh_usage();
        let reply = std::mem::take(&mut live.reply);
        let continuation = live.continuation.clone();
        let member = &mut self.crew[index];
        member.status = if result.is_ok() {
            Status::Done
        } else {
            Status::Error
        };
        let name = member.name.clone();
        let label = member.status.label();
        if member.is_commander() {
            self.notice = match &result {
                Ok(()) => continuation.map_or_else(
                    || format!("{name} replied."),
                    |link| format!("Done · {link}"),
                ),
                Err(error) => format!("{name}: {error}"),
            };
            if result.is_ok() {
                self.apply_directives(&reply);
            }
        } else {
            let outcome = match &result {
                Ok(()) => specialists::summary(&reply),
                Err(error) => error.to_string(),
            };
            self.push_log(format!("{name}  {label} · {outcome}"));
            self.notice = format!("{name} {label}");
            self.save_roster();
        }
        self.start_next();
    }

    fn apply_directives(&mut self, reply: &str) {
        for directive in specialists::directives(reply) {
            match directive {
                specialists::Directive::Spawn { name, role, task } => {
                    if let (Some(index), Some(task)) = (self.spawn(&name, &role), task) {
                        self.delegate(index, task);
                    }
                }
                specialists::Directive::Delegate { name, task } => self.ask(&name, task),
            }
        }
    }

    fn save_roster(&mut self) {
        let Some(store) = self.live.as_ref().and_then(|live| live.store.as_ref()) else {
            return;
        };
        if let Err(error) = store.save_roster(&self.crew) {
            self.notice = format!("Roster not saved: {error}");
        }
    }

    fn record(&mut self, index: usize, text: String) {
        if self.crew[index].is_commander() {
            return self.push_log(text);
        }
        let entry = Entry::now(text);
        if let Some(store) = self.live.as_ref().and_then(|live| live.store.as_ref()) {
            if let Err(error) = store.append_history(&self.crew[index].name, &entry) {
                self.notice = format!("History not saved: {error}");
            }
        }
        self.crew[index].remember(entry);
    }

    fn spawn(&mut self, name: &str, role: &str) -> Option<usize> {
        if let Err(error) = specialists::validate_name(name, &self.crew) {
            self.notice = error;
            return None;
        }
        let workspace = match self
            .live
            .as_ref()
            .and_then(|live| Some((live.store.as_ref()?, &live.workspace)))
        {
            Some((store, base)) => match store.isolate(base, name) {
                Ok(workspace) => Some(workspace),
                Err(error) => {
                    self.notice = format!("Cannot isolate {name}: {error}");
                    return None;
                }
            },
            None => None,
        };
        self.crew.push(Member::new(name, role, None, workspace));
        let index = self.crew.len() - 1;
        self.push_log(format!(
            "SYSTEM  Spawned {name} ({role}) · {}",
            specialists::workspace_label(&self.crew[index])
        ));
        self.save_roster();
        Some(index)
    }

    fn ask(&mut self, name: &str, task: String) {
        match specialists::find(&self.crew, name) {
            Some(index) if !self.crew[index].is_commander() => self.delegate(index, task),
            Some(_) => self.notice = "Message Commander directly.".into(),
            None => self.notice = format!("No specialist named {name}."),
        }
    }

    fn delegate(&mut self, index: usize, task: String) {
        let name = self.crew[index].name.clone();
        if self.crew[index].paused {
            self.notice = format!("Resume {name} before delegating.");
            return;
        }
        self.crew[index].mission = task.clone();
        self.record(index, format!("TASK  {task}"));
        let Some(live) = &mut self.live else {
            self.notice = PREVIEW_NOTICE.into();
            return;
        };
        live.queue.push_back((name.clone(), task.clone()));
        self.push_log(format!("SYSTEM  Delegated to {name}: {task}"));
        self.save_roster();
        self.start_next();
    }

    fn start_next(&mut self) {
        let Some(live) = &mut self.live else { return };
        if live.mission.is_some() {
            return;
        }
        let Some((name, task)) = live.queue.pop_front() else {
            return;
        };
        let Some(index) = specialists::find(&self.crew, &name) else {
            return self.start_next();
        };
        match self.launch_specialist(index, &task) {
            Ok(()) => {}
            Err(reason) => {
                self.crew[index].status = Status::Error;
                self.push_log(format!("{name}  ERROR · {reason}"));
                self.notice = reason;
                self.save_roster();
                self.start_next();
            }
        }
    }

    fn launch_specialist(&mut self, index: usize, task: &str) -> Result<(), String> {
        let live = self.live.as_mut().expect("live delegation");
        let member = &self.crew[index];
        let workspace = member
            .workspace
            .as_ref()
            .ok_or_else(|| format!("{} has no isolated workspace.", member.name))?;
        if live.profiles.is_empty() {
            return Err("Sign in with agent-command auth login PROVIDER PROFILE.".into());
        }
        let request = framed(routing::parse(task)?, |unit| {
            specialists::specialist_prompt(member, unit)
        });
        live.reply.clear();
        live.continuation = None;
        let mission = mission::Mission::launch(
            live.profiles.clone(),
            live.route(request),
            workspace.path.clone(),
        );
        live.mission = Some((member.name.clone(), mission));
        Ok(())
    }

    pub fn poll_theme(&mut self, path: &Path) {
        let stamp = fs::metadata(path).and_then(|m| m.modified()).ok();
        if stamp != self.theme_stamp {
            self.theme_stamp = stamp;
            self.deck_palette = deck_palette(path);
            match Theme::load(path) {
                Ok(theme) => {
                    self.theme = theme;
                    self.notice.clear();
                }
                Err(e) => self.notice = format!("Theme unchanged: {e}"),
            }
        }
    }
    pub fn watch_updates(&mut self, receiver: Option<Receiver<String>>) {
        self.update_receiver = receiver;
    }

    pub fn poll_update(&mut self) {
        let Some(receiver) = &self.update_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(latest) => {
                self.update_banner = Some(update::one_line_notice(&latest));
                self.update_receiver = None;
            }
            Err(TryRecvError::Disconnected) => self.update_receiver = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    pub fn dismiss_update(&mut self) {
        self.update_banner = None;
        self.update_area = Rect::default();
    }

    fn push_log(&mut self, text: String) {
        let entry = Entry::now(text);
        if let Some(store) = self.live.as_ref().and_then(|live| live.store.as_ref()) {
            if let Err(error) = store.append_commander(&entry) {
                self.notice = format!("Transcript not saved: {error}");
            }
        }
        self.log.push_back(entry.text);
        self.log_times.push_back(entry.at);
        if self.log.len() > specialists::FEED_LIMIT {
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
        let name = self.crew[self.selected].name.clone();
        if self
            .live
            .as_ref()
            .and_then(|live| live.mission.as_ref())
            .is_some_and(|(running, _)| *running == name)
        {
            self.notice = "This mission is running. Pause is available between missions.".into();
            return;
        }
        let member = &mut self.crew[self.selected];
        member.paused = !member.paused;
        let state = if member.paused { "Paused" } else { "Resumed" };
        self.push_log(format!("{name}  {state}."));
    }

    pub fn submit(&mut self) {
        let command = self.input.trim().to_owned();
        if command.is_empty() {
            return;
        }
        match specialists::parse_command(&command) {
            Err(usage) => self.notice = usage,
            Ok(specialists::Command::Message(text)) if self.selected > 0 => {
                self.input.clear();
                self.delegate(self.selected, text);
            }
            Ok(specialists::Command::Message(text)) => self.message_commander(text),
            Ok(command) => {
                self.input.clear();
                self.orchestrate(command);
            }
        }
    }

    fn orchestrate(&mut self, command: specialists::Command) {
        match command {
            specialists::Command::Spawn { name, role, task } => {
                if let Some(index) = self.spawn(&name, &role) {
                    self.selected = index;
                    if let Some(task) = task {
                        self.delegate(index, task);
                    }
                }
            }
            specialists::Command::Ask { name, task } => self.ask(&name, task),
            specialists::Command::Roster => {
                let entry = self
                    .live
                    .as_ref()
                    .map_or_else(|| "preview".into(), Live::entry_label);
                let roster = specialists::roster_summary(&self.crew);
                self.push_log(format!("SYSTEM  Roster: {roster} · entry {entry}"));
            }
            specialists::Command::Remove { name } => self.remove(&name),
            specialists::Command::Entry { profile } => self.set_entry(&profile),
            specialists::Command::Message(text) => self.message_commander(text),
        }
    }

    fn remove(&mut self, name: &str) {
        let Some(index) = specialists::find(&self.crew, name) else {
            self.notice = format!("No specialist named {name}.");
            return;
        };
        let member = &self.crew[index];
        if member.is_commander() {
            self.notice = "Commander is the persistent entry bot and cannot be removed.".into();
            return;
        }
        if let Some(live) = &mut self.live {
            if live
                .mission
                .as_ref()
                .is_some_and(|(running, _)| *running == member.name)
            {
                self.notice = format!("{} is running; remove it after it finishes.", member.name);
                return;
            }
            live.queue.retain(|(queued, _)| *queued != member.name);
            if let Some(Err(error)) = live.store.as_ref().map(|store| store.release(member)) {
                self.notice = format!("Cannot remove {}: {error}", member.name);
                return;
            }
        }
        let member = self.crew.remove(index);
        self.selected = self.selected.min(self.crew.len() - 1);
        self.push_log(format!("SYSTEM  Removed {}.", member.name));
        self.save_roster();
    }

    fn set_entry(&mut self, profile: &str) {
        let Some(live) = &mut self.live else {
            self.notice = "Preview: sign in to choose an entry profile.".into();
            return;
        };
        if live.mission.is_some() {
            self.notice = "Change the entry profile between missions.".into();
            return;
        }
        live.preferred = if profile == "auto" {
            None
        } else {
            match live
                .profiles
                .iter()
                .position(|candidate| candidate.to_string() == profile)
            {
                Some(index) => Some(index),
                None => {
                    self.notice = format!("{profile} is not a signed-in profile.");
                    return;
                }
            }
        };
        live.refresh_routes();
        live.usage_checked_at = None;
        self.notice = match live.save_override() {
            Ok(()) => format!("Entry model: {}", live.entry_label()),
            Err(error) => format!("Entry override not saved: {error}"),
        };
    }

    fn message_commander(&mut self, command: String) {
        let Some(live) = &mut self.live else {
            self.crew[0].mission = command.clone();
            self.push_log(format!("YOU  {command}"));
            self.input.clear();
            self.notice = PREVIEW_NOTICE.into();
            return;
        };
        if live.mission.is_some() {
            self.notice = "A mission is already running.".into();
            return;
        }
        if self.crew[0].paused {
            self.notice = "Resume Commander before sending a message.".into();
            return;
        }
        if let Some((index, claim)) = live.pending.clone() {
            live.mission = Some((
                COMMANDER.into(),
                mission::Mission::resume(
                    live.profiles.clone(),
                    index,
                    live.workspace.clone(),
                    command.clone(),
                    claim,
                ),
            ));
            self.crew[0].mission = command;
            self.input.clear();
            return;
        }
        if live.profiles.is_empty() {
            self.notice = "Sign in with agent-command auth login PROVIDER PROFILE.".into();
            return;
        }
        let request = match routing::parse(&command) {
            Ok(request) => framed(request, |unit| {
                specialists::commander_prompt(self.log.iter().map(String::as_str), &self.crew, unit)
            }),
            Err(error) => {
                self.notice = error;
                return;
            }
        };
        self.notice = format!("Routing in {}", live.workspace.display());
        live.continuation = None;
        live.reply.clear();
        let mission = mission::Mission::launch(
            live.profiles.clone(),
            live.route(request),
            live.workspace.clone(),
        );
        live.mission = Some((COMMANDER.into(), mission));
        self.crew[0].mission = command.clone();
        self.input.clear();
        self.push_log(format!("YOU  {command}"));
    }

    fn abandon_continuation(&mut self) {
        let Some(live) = &mut self.live else { return };
        if live.mission.is_some() {
            return;
        }
        let Some((index, claim)) = &live.pending else {
            return;
        };
        self.notice = match claim.abandon(&live.profiles[*index]) {
            Ok(()) => {
                live.pending = None;
                self.input.clear();
                "Interrupted continuation abandoned.".into()
            }
            Err(error) => error.to_string(),
        };
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
            KeyCode::F(4) => self.abandon_continuation(),
            KeyCode::F(6) => self.dismiss_update(),
            KeyCode::Tab => self.editing = !self.editing,
            KeyCode::Esc => self.editing = false,
            KeyCode::Enter if self.editing => self.submit(),
            KeyCode::Enter => self.editing = true,
            KeyCode::Backspace if self.editing => {
                self.input.pop();
            }
            KeyCode::Char(c) if self.editing => self.input.push(c),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.crew.len() - 1),
            KeyCode::Home => self.selected = 0,
            _ => {}
        }
    }
    pub fn click(&mut self, x: u16, y: u16) {
        let point = ratatui::layout::Position::new(x, y);
        if self.update_area.contains(point) {
            self.dismiss_update();
        } else if self
            .live
            .as_ref()
            .is_some_and(|live| live.abandon_area.contains(point))
        {
            self.abandon_continuation();
        } else if self
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
            let row = usize::from((y - self.crew_area.y) / self.crew_row_height.max(1));
            if self.crew_offset + row < self.crew.len() {
                self.selected = self.crew_offset + row;
            }
            self.editing = false;
        }
    }
    fn crew_counts(&self) -> [String; 4] {
        let count = |status| {
            self.crew
                .iter()
                .filter(|member| member.status == status)
                .count()
        };
        [
            format!("{:02}  CREW", self.crew.len()),
            format!("{:02}  RUNNING", count(Status::Running)),
            format!("{:02}  NEED ATTENTION", count(Status::Error)),
            if self.live.is_some() {
                "LIVE  /  COMMANDER".into()
            } else {
                "PREVIEW  /  NO DISPATCH".into()
            },
        ]
    }

    fn selected_details(&self) -> Vec<Line<'static>> {
        let member = &self.crew[self.selected];
        if member.is_commander() {
            let entry = self
                .live
                .as_ref()
                .map_or_else(|| "preview".into(), Live::entry_label);
            vec![
                Line::raw("ENTRY MODEL"),
                Line::raw(entry),
                Line::raw(""),
                Line::raw("SPECIALISTS"),
                Line::raw(format!("{} on roster", self.crew.len() - 1)),
            ]
        } else {
            vec![
                Line::raw("ROLE"),
                Line::raw(member.role.clone()),
                Line::raw(""),
                Line::raw("WORKSPACE"),
                Line::raw(specialists::workspace_label(member)),
            ]
        }
    }

    fn feed_entries(&self) -> Vec<(&str, &str)> {
        if self.selected == 0 {
            self.log_times
                .iter()
                .zip(&self.log)
                .map(|(at, text)| (at.as_str(), text.as_str()))
                .collect()
        } else {
            self.crew[self.selected]
                .history
                .iter()
                .map(|entry| (entry.at.as_str(), entry.text.as_str()))
                .collect()
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        self.selected = self.selected.min(self.crew.len() - 1);
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
        let banner_rows = self.draw_update_banner(f, a);
        let cards = self.live.as_ref().map(Live::deck_cards).unwrap_or_default();
        let mode = deck_mode(a, compact);
        let deck_rows = match mode {
            token_burn::DeckMode::Full if !cards.is_empty() => {
                self.draw_deck(
                    f,
                    &cards,
                    mode,
                    r(
                        3,
                        2,
                        a.width.saturating_sub(6),
                        token_burn::FULL_CARD_HEIGHT,
                    ),
                );
                0
            }
            token_burn::DeckMode::Full => 0,
            token_burn::DeckMode::Compact => {
                let rows = cards.len().min(COMPACT_DECK_MAX_ROWS) as u16;
                self.draw_deck(
                    f,
                    &cards,
                    mode,
                    r(1, 1 + banner_rows, a.width.saturating_sub(2), rows),
                );
                rows
            }
        };
        let top_shift = (banner_rows + deck_rows).saturating_sub(1);
        if !compact && cards.is_empty() {
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
        }
        if !compact {
            let ma = r(3, 8, a.width.saturating_sub(6), 3);
            f.render_widget(
                Block::default().style(Style::default().bg(color("191e25"))),
                ma,
            );
            let metrics = Layout::horizontal([Constraint::Fill(1); 4]).split(ma);
            let labels = self.crew_counts();
            for (i, label) in labels.iter().enumerate() {
                f.render_widget(
                    Paragraph::new(label.as_str())
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
        let usage_rows = if self.live.is_some() { 3 } else { 0 };
        let main = if compact {
            r(
                1,
                2 + top_shift,
                a.width.saturating_sub(2),
                a.height.saturating_sub(8 + usage_rows + top_shift),
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
            cols[0].y + if compact && self.live.is_some() { 2 } else { 3 },
            cols[0].width.saturating_sub(4),
            if compact {
                8.min(cols[0].height.saturating_sub(4))
            } else {
                16.min(cols[0].height.saturating_sub(7))
            },
        );
        let stride = (if compact { 2 } else { 4 })
            .min(self.crew_area.height)
            .max(1);
        let visible = usize::from((self.crew_area.height / stride).max(1));
        self.crew_row_height = stride;
        self.crew_offset = (self.selected + 1).saturating_sub(visible);
        for (row_index, (i, member)) in self
            .crew
            .iter()
            .enumerate()
            .skip(self.crew_offset)
            .take(visible)
            .enumerate()
        {
            let name = &member.name;
            let y = self.crew_area.y + row_index as u16 * stride;
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
                        Style::default().fg(color(match member.status {
                            Status::Running => &t.amber,
                            Status::Error => "b86d67",
                            _ => &t.accent,
                        })),
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
                Line::styled(
                    format!("   {} · {}", self.status(i).to_lowercase(), member.role),
                    style,
                ),
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
                cols[0]
                    .bottom()
                    .saturating_sub(4)
                    .max(self.crew_area.bottom()),
                cols[0].width.saturating_sub(5),
                2,
            ),
        );
        let x = cols[1].x + 2;
        let y = cols[1].y + 3;
        let w = cols[1].width.saturating_sub(4);
        let selected = &self.crew[self.selected];
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{}  ", selected.name), Style::default().bold()),
                Span::styled(
                    format!(
                        "● {}",
                        if selected.paused {
                            "PAUSED"
                        } else {
                            self.status(self.selected)
                        }
                    ),
                    Style::default()
                        .fg(color(if selected.status == Status::Running {
                            &t.amber
                        } else {
                            &t.accent
                        }))
                        .bold(),
                ),
            ])),
            r(x, y, w, 1),
        );
        f.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    selected.mission.clone(),
                    Style::default().fg(color("d4d9df")),
                ),
                Line::styled(
                    format!(
                        "{}  /  {:02}",
                        selected.role.to_uppercase(),
                        self.selected + 1
                    ),
                    Style::default().fg(color(&t.muted)),
                ),
            ])
            .wrap(Wrap { trim: false }),
            r(x, y + 2, w, 4),
        );
        let by = cols[1].bottom().saturating_sub(5);
        if by > y + 7 {
            f.render_widget(
                Paragraph::new(self.selected_details())
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(color(&t.muted))),
                r(x, y + 6, w, by - y - 7),
            );
        }
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
                if self.crew[self.selected].paused {
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
        let entries = self.feed_entries();
        let mut lines: Vec<_> = entries
            .iter()
            .skip(entries.len().saturating_sub(keep))
            .flat_map(|(stamp, entry)| {
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
            live.profile_area = r(2, iy.saturating_sub(4), a.width.saturating_sub(4), 2);
            let now = usage::now();
            let label = if live.profiles.is_empty() {
                vec![Line::raw(
                    "No profiles · agent-command auth login PROVIDER PROFILE",
                )]
            } else {
                {
                    let latch = live.usage.map_or_else(
                        || "State unavailable".into(),
                        |usage| usage.latch_label(now),
                    );
                    let control = if live.running_profile.is_some() {
                        "Running"
                    } else {
                        "F2 / click"
                    };
                    vec![
                        Line::raw(format!("{control} · {latch} · {}", live.route_mode())),
                        Line::raw(format!(
                            "{} · {}",
                            live.profiles[live.displayed_profile()],
                            live.pin_label(live.displayed_profile())
                        )),
                    ]
                }
            };
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
                    if self
                        .live
                        .as_ref()
                        .is_some_and(|live| live.pending.is_some())
                    {
                        "Re-enter the original task to resume…".into()
                    } else if self.selected == 0 {
                        "Message Commander or /spawn NAME ROLE [TASK]…".into()
                    } else {
                        format!("Give {} a task…", self.crew[self.selected].name)
                    }
                } else {
                    self.input.clone()
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
        let pending = self
            .live
            .as_ref()
            .is_some_and(|live| live.pending.is_some() && live.mission.is_none());
        f.render_widget(
            Paragraph::new(if pending && self.notice.is_empty() {
                "Interrupted continuation pending · re-enter task, then Send"
            } else if self.notice.is_empty() {
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
        let shortcuts = r(0, a.height.saturating_sub(1), a.width, 1);
        let style = Style::default().bg(color("1b2027")).fg(color("a5b1bd"));
        f.render_widget(
            Paragraph::new(" ^Q Quit    ^N New mission    ^P Pause / resume    ^L Clear feed")
                .style(style),
            shortcuts,
        );
        if let Some(live) = &mut self.live {
            live.abandon_area = if pending {
                r(a.width.saturating_sub(14), shortcuts.y, 14, 1)
            } else {
                Rect::default()
            };
            if pending {
                f.render_widget(
                    Paragraph::new("  F4 Abandon  ").style(style),
                    live.abandon_area,
                );
            }
        }
    }
}

impl App {
    fn draw_update_banner(&mut self, f: &mut Frame, area: Rect) -> u16 {
        let Some(text) = &self.update_banner else {
            self.update_area = Rect::default();
            return 0;
        };
        self.update_area = Rect::new(area.x, area.y + 1, area.width, 1).intersection(area);
        f.render_widget(
            Paragraph::new(format!("{UPDATE_DISMISS_HINT} {text}")).style(
                Style::default()
                    .fg(color("1b2027"))
                    .bg(color(&self.theme.amber))
                    .bold(),
            ),
            self.update_area,
        );
        1
    }

    fn draw_deck(
        &self,
        f: &mut Frame,
        cards: &[token_burn::AccountCard],
        mode: token_burn::DeckMode,
        area: Rect,
    ) {
        if mode == token_burn::DeckMode::Compact {
            token_burn::render_deck_as(mode, cards, area, f.buffer_mut(), &self.deck_palette);
            return;
        }
        let shown = &cards[..cards.len().min(FULL_DECK_MAX_CARDS)];
        let columns = Layout::horizontal(vec![Constraint::Fill(1); shown.len()])
            .spacing(1)
            .split(area);
        for (card, column) in shown.iter().zip(columns.iter()) {
            token_burn::render_deck_as(
                mode,
                std::slice::from_ref(card),
                *column,
                f.buffer_mut(),
                &self.deck_palette,
            );
        }
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
    fn test_live(
        workspace: PathBuf,
        profiles: Vec<auth::Profile>,
        preferred: Option<usize>,
    ) -> Live {
        let root = profiles.first().map(|profile| profile.root().to_owned());
        Live {
            preferred,
            ..Live::new(workspace, profiles, root)
        }
    }
    #[cfg(unix)]
    #[test]
    fn live_mission_uses_selected_profile_and_renders_output_and_outcome() {
        use std::os::unix::fs::PermissionsExt;
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
prompt=$(cat)
case "$prompt" in "You are Commander"*"Operator: Mission input") ;; *) exit 9 ;; esac
test -f workspace-marker || exit 9
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
                live: Some(test_live(
                    workspace.clone(),
                    vec![codex.clone(), claude.clone()],
                    key.then_some(0),
                )),
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
    }

    #[cfg(unix)]
    fn wait_for(app: &mut App, ready: impl Fn(&App) -> bool) {
        use std::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready(app) {
            assert!(Instant::now() < deadline, "{}", app.notice);
            app.poll_mission();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn continuation_receipt_is_visible_after_keyboard_and_mouse_submission() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for (width, height, keyboard) in [(80, 24, true), (120, 40, false)] {
            let (_fixture, workspace, profiles) =
                mission::tests::limit_fixture(&format!("continuation-panel-{width}"), usage::now());
            let mut app = App {
                live: Some(test_live(workspace.clone(), profiles, Some(0))),
                input: "Keep the finished step".into(),
                editing: keyboard,
                ..App::default()
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            } else {
                app.click(app.send_area.x, app.send_area.y);
                app.click(app.send_area.x, app.send_area.y);
            }
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let notice: String = (0..width)
                .map(|x| buffer[(x, height - 2)].symbol())
                .collect();
            assert!(
                notice.contains("Done · Limit: claude/first → claude/standby"),
                "{notice}"
            );
            assert_eq!(
                fs::read_to_string(workspace.join("completed")).unwrap(),
                "done\ncontinued\n"
            );
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
        let long = "a".repeat(64);
        let latched = fixture.profile(Provider::Claude, &long);
        let now = usage::now();
        Usage::observe_claude(
            &latched,
            &json!({
                "type":"rate_limit_event",
                "rate_limit_info":{
                    "status":"rejected","resetsAt":now.wall + 7200,
                    "unifiedWindows":{
                        "five_hour":{"utilization":0.25,"resetsAt":now.wall + 7200},
                        "seven_day":{"utilization":0.60,"resetsAt":now.wall + 86400}
                    }
                }
            }),
            now,
        )
        .unwrap();

        for (width, height, key) in INPUT_MATRIX {
            let mut app = App {
                live: Some(test_live(
                    fixture.root.clone(),
                    vec![ready.clone(), latched.clone()],
                    None,
                )),
                ..App::default()
            };
            app.poll_mission();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let area = app.live.as_ref().unwrap().profile_area;
            let panel = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
                let buffer = terminal.backend().buffer();
                (area.y..area.y + 4)
                    .map(|y| {
                        (area.x..area.right())
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
            };
            let initial = panel(&terminal);
            assert!(initial[0].contains("No latch") && initial[1].contains("claude/ready"));
            assert!(initial[2].contains("5h unavailable"));
            assert!(initial[0].contains("Auto"));
            let mut press = |app: &mut App| {
                if key {
                    app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
                } else {
                    app.click(area.x, area.y);
                }
                terminal.draw(|f| app.draw(f)).unwrap();
                panel(&terminal)
            };
            let first = press(&mut app);
            assert!(first[0].contains("Override") && first[1].contains("claude/ready"));
            let changed = press(&mut app);
            assert!(
                changed[0].contains("Latched") && changed[1].contains(&format!("claude/{long}"))
            );
            assert!(changed[2].contains("5h 25% used / 75% left, reset"));
            assert!(changed[3].contains("Week 60% used / 40% left, reset"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn override_persists_across_reopen_and_shows_fallback_reason_with_keyboard_and_mouse() {
        use auth::Provider;
        use serde_json::json;
        use usage::{tests::Fixture, Usage};

        for (width, height, keyboard) in INPUT_MATRIX {
            let fixture = Fixture::new(
                &format!("override-panel-{width}-{keyboard}"),
                r#"
if test "$1" = auth; then
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
cat > /dev/null
while test ! -f release; do sleep 0.01; done
"#,
            );
            let profiles = vec![
                fixture.profile(Provider::Claude, "alpha"),
                fixture.profile(Provider::Claude, "zulu"),
            ];
            let now = usage::now();
            Usage::observe_claude(
                &profiles[1],
                &json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":now.wall + 600}}),
                now,
            )
            .unwrap();
            let mut app = App {
                live: Some(test_live(fixture.root.clone(), profiles.clone(), None)),
                ..App::default()
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let row = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, y: u16| {
                let buffer = terminal.backend().buffer();
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            terminal.draw(|f| app.draw(f)).unwrap();
            for _ in 0..2 {
                let area = app.live.as_ref().unwrap().profile_area;
                if keyboard {
                    app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
                } else {
                    app.click(area.x + area.width / 2, area.y);
                }
                terminal.draw(|f| app.draw(f)).unwrap();
            }
            assert!(row(&terminal, height - 2).contains("Override saved: claude/zulu"));

            let mut reopened = test_live(fixture.root.clone(), profiles.clone(), None);
            assert_eq!(reopened.load_override(), None);
            assert_eq!(reopened.preferred, Some(1));
            let mut app = App {
                live: Some(reopened),
                input: "Do work".into(),
                editing: keyboard,
                ..App::default()
            };
            app.poll_mission();
            terminal.draw(|f| app.draw(f)).unwrap();
            let area = app.live.as_ref().unwrap().profile_area;
            assert!(
                row(&terminal, area.y).contains("Latched")
                    && row(&terminal, area.y).contains("Override")
            );
            assert!(row(&terminal, area.y + 1).contains("claude/zulu"));
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            } else {
                let send = app.send_area;
                click_center(&mut app, send);
            }
            wait_for(&mut app, |app| app.status(0) == "RUNNING");
            terminal.draw(|f| app.draw(f)).unwrap();
            let notice = row(&terminal, height - 2);
            assert!(
                notice.contains("Override claude/zulu skipped: latched · rotated → claude/alpha"),
                "{notice}"
            );
            assert!(row(&terminal, area.y + 1).contains("claude/alpha"));
            fs::write(fixture.root.join("release"), "").unwrap();
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);

            fs::write(
                profiles[0].usage_directory().unwrap().join("state.json"),
                b"{",
            )
            .unwrap();
            app.input = "Do work".into();
            app.editing = keyboard;
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            } else {
                let send = app.send_area;
                click_center(&mut app, send);
            }
            wait_for(&mut app, |app| !app.mission_running());
            terminal.draw(|f| app.draw(f)).unwrap();
            assert_eq!(app.status(0), "ERROR");
            assert_eq!(
                app.notice,
                "Commander: No eligible signed-in subscription profile. Skipped: claude/alpha usage state unreadable; claude/zulu latched."
            );
            assert!(row(&terminal, height - 2)
                .contains("Commander: No eligible signed-in subscription"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn pinned_model_and_ranking_metric_are_visible_with_keyboard_and_mouse() {
        use auth::Provider;
        use catalog::{Endpoint, Row, Snapshot, Speed, SpeedUnit};
        use usage::tests::Fixture;

        for (width, height, keyboard) in INPUT_MATRIX {
            let fixture = Fixture::new(
                &format!("pinned-panel-{width}-{keyboard}"),
                r#"
if test "$1" = auth; then
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
printf '%s\n' "$*" >> arguments
cat > /dev/null
while test ! -f release; do sleep 0.01; done
"#,
            );
            let profiles = vec![
                fixture.profile(Provider::Claude, "alpha"),
                fixture.profile(Provider::Claude, "beta"),
            ];
            let state = routing::state_directory(&fixture.root.join("profiles")).unwrap();
            native::Pins::from_pins(vec![routing::tests::pin(
                "claude/alpha",
                "claude-opus-5-5",
                Some("high"),
            )])
            .unwrap()
            .save(&state)
            .unwrap();
            fs::create_dir_all(catalog::cache_directory(&state)).unwrap();
            catalog::publish(
                &state,
                &Snapshot::sealed(
                    Endpoint::ModelsFree,
                    "4".into(),
                    "synthetic test fixture".into(),
                    vec![Row {
                        provider: "claude".into(),
                        slug: "claude-opus-5-5-high".into(),
                        effort: None,
                        coding_index: None,
                        intelligence_index: Some(63.0),
                        speed: Some(Speed {
                            unit: SpeedUnit::TokensPerSecond,
                            value: 52.5,
                        }),
                    }],
                ),
            )
            .unwrap();
            let mut live = test_live(fixture.root.clone(), profiles, None);
            assert_eq!(live.load_pins(), None);
            let mut app = App {
                live: Some(live),
                ..App::default()
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let row = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, y: u16| {
                let buffer = terminal.backend().buffer();
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            terminal.draw(|f| app.draw(f)).unwrap();
            let area = app.live.as_ref().unwrap().profile_area;
            let mut press = |app: &mut App| {
                if keyboard {
                    app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
                } else {
                    click_center(app, area);
                }
                terminal.draw(|f| app.draw(f)).unwrap();
                row(&terminal, area.y + 1)
            };
            assert!(press(&mut app).contains("claude/alpha · pin claude-opus-5-5 high"));
            assert!(press(&mut app).contains("claude/beta · no model pin"));
            press(&mut app);
            assert!(press(&mut app).contains("claude/alpha · pin claude-opus-5-5 high"));
            app.input = "/hard Do work".into();
            app.editing = keyboard;
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            } else {
                let send = app.send_area;
                click_center(&mut app, send);
            }
            wait_for(&mut app, |app| app.status(0) == "RUNNING");
            assert!(app.notice.starts_with(
                "Override → claude/alpha claude-opus-5-5 high · AA intelligence index 63.0 · 52 tok/s"
            ));
            terminal.draw(|f| app.draw(f)).unwrap();
            let notice = row(&terminal, height - 2);
            assert!(notice.contains("AA intelligence index 63.0"), "{notice}");
            assert!(row(&terminal, area.y + 1).contains("claude/alpha · pin claude-opus-5-5 high"));
            fs::write(fixture.root.join("release"), "").unwrap();
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);
            assert_eq!(
                fs::read_to_string(fixture.root.join("arguments")).unwrap(),
                "--model claude-opus-5-5 --effort high --print --verbose --output-format stream-json --setting-sources user --\n"
            );
            fs::write(state.join(native::PINS_FILE), b"{\"schema\":\"agent-command-native-pins-v1\",\"pins\":[{\"profile\":\"claude/alpha\",\"model\":\"opus\",\"effort\":null}]}").unwrap();
            let mut reopened = test_live(
                fixture.root.clone(),
                app.live.take().unwrap().profiles,
                None,
            );
            assert!(reopened
                .load_pins()
                .unwrap()
                .starts_with("Cannot read model pins"));
            assert_eq!(reopened.pin_label(0), "no model pin");
        }
    }

    #[cfg(unix)]
    const ORCHESTRATOR_PROVIDER: &str = r#"
if test "$1" = auth; then
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
prompt=$(cat)
case "$prompt" in
  "You are Commander"*"Operator: Review the patch please")
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Spawning a reviewer.\nSPAWN_SPECIALIST: nova | reviewer | Check the README"}]}}' ;;
  "You are Commander"*"nova (reviewer, DONE)"*"Spawning a reviewer."*"Operator: Hi")
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Hello again"}]}}' ;;
  "You are nova"*"Task: Check the README")
    test -f README || exit 3
    printf 'reviewed\n' > review.txt
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Working\nREADME looks good"}]}}' ;;
  *) exit 9 ;;
esac
"#;

    fn screen(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn region(buffer: &ratatui::buffer::Buffer, area: Rect) -> String {
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(unix)]
    fn send(app: &mut App, text: &str) {
        app.input = text.into();
        app.submit();
    }

    #[cfg(unix)]
    #[test]
    fn commander_delegates_to_an_isolated_specialist_and_survives_restart() {
        use auth::Provider;
        use usage::tests::Fixture;

        let fixture = Fixture::new("orchestrator", ORCHESTRATOR_PROVIDER);
        let repository = fixture.root.join("work");
        specialists::tests::git_repository(&repository);
        let profiles = vec![fixture.profile(Provider::Claude, "alpha")];
        let open = || App::from_live(test_live(repository.clone(), profiles.clone(), None));

        let mut app = open();
        assert_eq!(app.crew.len(), 1);
        assert!(app.crew[0].is_commander());
        send(&mut app, "Review the patch please");
        wait_for(&mut app, |app| {
            app.crew
                .get(1)
                .is_some_and(|nova| nova.status == Status::Done)
        });
        assert_eq!(app.status(0), "DONE");
        assert!(app.log.contains(&"YOU  Review the patch please".to_owned()));
        assert!(app
            .log
            .contains(&"Commander  Spawning a reviewer.".to_owned()));
        assert_eq!(
            app.log.back().unwrap(),
            "nova  DONE · README looks good",
            "{:?}",
            app.log
        );

        let nova = app.crew[1].clone();
        let workspace = nova.workspace.clone().unwrap();
        assert!(matches!(
            workspace.isolation,
            specialists::Isolation::Worktree { .. }
        ));
        assert!(workspace.path.join("review.txt").is_file());
        assert!(!repository.join("review.txt").exists());
        let worktrees = std::process::Command::new("git")
            .current_dir(&repository)
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&worktrees.stdout).contains(workspace.path.to_str().unwrap())
        );
        let history: Vec<_> = nova
            .history
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(
            history,
            [
                "TASK  Check the README",
                "nova  Working",
                "nova  README looks good"
            ]
        );
        assert!(!app.log.iter().any(|line| line == "nova  Working"));

        for (width, height, keyboard) in INPUT_MATRIX {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            assert!(
                crew.contains("Commander") && crew.contains("nova"),
                "{crew}"
            );
            assert!(crew.contains("done · rev"), "{width}x{height}: {crew}");
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            } else {
                app.click(app.crew_area.x, app.crew_area.y + app.crew_row_height + 1);
            }
            assert_eq!(app.selected, 1);
            let buffer = screen(&mut app, width, height);
            let feed = Rect::new(
                app.pause_area.right() + 2,
                app.crew_area.y,
                width - app.pause_area.right() - 3,
                app.input_area.y - app.crew_area.y,
            );
            assert!(region(&buffer, feed).contains("README looks good"));
            assert!(region(&buffer, Rect::new(0, 0, width, height)).contains("nova  ● DONE"));
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            } else {
                app.click(app.crew_area.x, app.crew_area.y + 1);
            }
            assert_eq!(app.selected, 0);
            let buffer = screen(&mut app, width, height);
            let commander_feed = region(&buffer, feed);
            assert!(
                commander_feed.contains("DONE · README") && !commander_feed.contains("Working"),
                "{width}x{height}: {commander_feed}"
            );
        }

        let mut app = open();
        assert_eq!(app.crew.len(), 2);
        assert_eq!(app.crew[1].name, "nova");
        assert_eq!(app.crew[1].status, Status::Done);
        assert_eq!(app.crew[1].history, nova.history);
        assert!(app
            .log
            .contains(&"nova  DONE · README looks good".to_owned()));
        send(&mut app, "Hi");
        wait_for(&mut app, |app| !app.mission_running());
        assert_eq!(
            app.log.back().unwrap(),
            "Commander  Hello again",
            "{}",
            app.notice
        );

        send(&mut app, "/roster");
        assert!(app.log.back().unwrap().starts_with(
            "SYSTEM  Roster: Commander (entry, DONE) · nova (reviewer, DONE) · entry claude/alpha"
        ));
        send(&mut app, "/remove commander");
        assert_eq!(app.crew.len(), 2);
        send(&mut app, "/remove nova");
        assert_eq!(app.crew.len(), 1);
        assert!(!workspace.path.exists());
        assert_eq!(open().crew.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn entry_model_is_auto_chosen_overridable_and_falls_back_to_first_profile() {
        use auth::Provider;
        use usage::tests::Fixture;

        let fixture = Fixture::new("entry-model", ORCHESTRATOR_PROVIDER);
        let profiles = vec![
            fixture.profile(Provider::Claude, "alpha"),
            fixture.profile(Provider::Claude, "beta"),
        ];
        let mut app = App::from_live(test_live(fixture.root.clone(), profiles.clone(), None));
        let entry = app.live.as_ref().unwrap().entry.clone().unwrap();
        assert_eq!(entry.1, "auto");
        assert!(entry.0.starts_with("claude/"));
        send(&mut app, "/entry claude/beta");
        assert_eq!(app.notice, "Entry model: claude/beta (override)");
        let mut reopened = test_live(fixture.root.clone(), profiles.clone(), None);
        assert_eq!(reopened.load_override(), None);
        assert_eq!(reopened.preferred, Some(1));
        send(&mut app, "/entry nobody/else");
        assert_eq!(app.notice, "nobody/else is not a signed-in profile.");
        send(&mut app, "/entry auto");
        assert!(app.notice.ends_with("(auto)"), "{}", app.notice);
        let buffer = screen(&mut app, 120, 40);
        assert!(region(&buffer, Rect::new(0, 0, 120, 40)).contains("ENTRY MODEL"));

        let signed_out = Fixture::new("entry-signed-out", "exit 1\n");
        let profiles = vec![signed_out.profile(Provider::Claude, "gamma")];
        let app = App::from_live(test_live(signed_out.root.clone(), profiles, None));
        assert_eq!(
            app.live.as_ref().unwrap().entry,
            Some(("claude/gamma".into(), "first signed-in"))
        );
    }

    #[test]
    fn preview_spawn_ask_and_remove_stay_in_memory() {
        let mut app = App::default();
        for text in [
            "/spawn atlas builder Build it",
            "/spawn atlas again",
            "/ask ghost x",
        ] {
            app.input = text.into();
            app.submit();
        }
        assert_eq!(app.crew.len(), 2);
        assert_eq!(app.notice, "No specialist named ghost.");
        assert_eq!(app.crew[1].mission, "Build it");
        assert_eq!(app.selected, 1);
        app.input = "/spawn".into();
        app.submit();
        assert_eq!(app.input, "/spawn");
        app.input = "/remove atlas".into();
        app.submit();
        assert_eq!(app.crew.len(), 1);
        assert_eq!(app.selected, 0);
    }

    #[cfg(unix)]
    const INPUT_MATRIX: [(u16, u16, bool); 4] = [
        (80, 24, true),
        (80, 24, false),
        (120, 40, true),
        (120, 40, false),
    ];

    fn click_center(app: &mut App, area: Rect) {
        app.click(area.x + area.width / 2, area.y + area.height / 2);
    }

    #[test]
    fn controls_and_unicode() {
        let mut a = App {
            input: "/spawn nova reviewer".into(),
            ..App::default()
        };
        a.submit();
        a.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        a.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        a.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(a.crew[1].paused);
        a.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        a.editing = true;
        for c in "Build λ".chars() {
            a.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        a.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        a.submit();
        assert_eq!(a.crew[1].mission, "Build");
        assert_eq!(a.crew[1].history.back().unwrap().text, "TASK  Build");
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

    #[test]
    fn deck_palette_follows_theme_and_falls_back_when_it_does_not_parse() {
        let path = std::env::temp_dir().join(format!(
            "agent-command-deck-theme-{}.toml",
            std::process::id()
        ));
        fs::write(&path, "accent = '123456'").unwrap();
        let mut app = App::default();
        app.poll_theme(&path);
        assert_eq!(app.deck_palette.accent, color("123456"));
        fs::write(&path, "accent = [").unwrap();
        app.theme_stamp = None;
        app.poll_theme(&path);
        assert_eq!(app.deck_palette, token_burn::TokenBurnPalette::default());
        assert_eq!(app.theme.accent, "123456");
        for (width, height) in SIZES {
            let buffer = screen(&mut app, width, height);
            assert!(region(&buffer, app.input_area).contains("Message Commander"));
        }
        fs::remove_file(path).unwrap();
    }

    const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];

    fn row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        region(buffer, Rect::new(0, y, buffer.area.width, 1))
    }

    #[cfg(unix)]
    fn cell_at(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
        (0..buffer.area.height).find_map(|y| {
            let symbols: Vec<&str> = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            let width = needle.chars().count();
            (0..symbols.len().saturating_sub(width - 1))
                .find_map(|x| (symbols[x..x + width].concat() == needle).then_some((x as u16, y)))
        })
    }

    #[test]
    fn update_banner_appears_from_an_injected_receiver_and_dismisses() {
        for (index, (width, height)) in SIZES.into_iter().enumerate() {
            let mut app = App::default();
            let (sender, receiver) = std::sync::mpsc::channel();
            app.watch_updates(Some(receiver));
            app.poll_update();
            assert!(app.update_banner.is_none());
            sender.send("9.9.9".to_owned()).unwrap();
            app.poll_update();
            let buffer = screen(&mut app, width, height);
            let banner = row(&buffer, 1);
            assert!(banner.starts_with(UPDATE_DISMISS_HINT), "{banner}");
            assert!(
                banner.contains("Agent Command 9.9.9 is available"),
                "{banner}"
            );
            assert!(app.input_area.bottom() <= height);
            assert!(region(&buffer, app.crew_area).contains("Commander"));
            if index == 0 {
                app.key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
            } else {
                let banner = app.update_area;
                click_center(&mut app, banner);
            }
            assert!(app.update_banner.is_none());
            let buffer = screen(&mut app, width, height);
            assert!(!row(&buffer, 1).contains("9.9.9"));
            drop(sender);
            app.poll_update();
            assert!(app.update_receiver.is_none());
        }
        let mut app = App::default();
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        app.watch_updates(Some(receiver));
        drop(sender);
        app.poll_update();
        assert!(app.update_receiver.is_none() && app.update_banner.is_none());
    }

    #[cfg(unix)]
    fn observe(profile: &auth::Profile, status: &str, five: f64, week: f64) {
        let now = usage::now();
        usage::Usage::observe_claude(
            profile,
            &serde_json::json!({
                "type":"rate_limit_event",
                "rate_limit_info":{
                    "status":status,"resetsAt":now.wall + 7200,
                    "unifiedWindows":{
                        "five_hour":{"utilization":five,"resetsAt":now.wall + 7200},
                        "seven_day":{"utilization":week,"resetsAt":now.wall + 86400}
                    }
                }
            }),
            now,
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn token_burn_deck_renders_known_unknown_and_latched_usage() {
        use auth::Provider;
        use usage::tests::Fixture;

        let fixture = Fixture::new("token-burn-deck", ORCHESTRATOR_PROVIDER);
        let known = fixture.profile(Provider::Claude, "known");
        let fresh = fixture.profile(Provider::Claude, "fresh");
        let held = fixture.profile(Provider::Claude, "held");
        observe(&known, "allowed", 0.36, 0.88);
        observe(&held, "rejected", 1.0, 0.5);
        let mut app = App::from_live(test_live(
            fixture.root.clone(),
            vec![known, fresh, held],
            None,
        ));
        let palette = app.deck_palette;

        let compact = screen(&mut app, 80, 24);
        let [known_row, fresh_row, held_row] = [1, 2, 3].map(|y| row(&compact, y));
        assert!(known_row.contains("claude · known"), "{known_row}");
        assert!(
            known_row.contains("64%") && known_row.contains("12%"),
            "{known_row}"
        );
        assert!(known_row.contains("↻ 2h") || known_row.contains("↻ 1h 59m"));
        assert!(fresh_row.contains("claude · fresh") && fresh_row.contains("--"));
        assert!(fresh_row.contains("↻ -"), "{fresh_row}");
        assert!(held_row.contains("claude · held") && held_row.contains("0%"));
        let (x, y) = cell_at(&compact, "--").unwrap();
        assert_eq!((y, compact[(x, y)].fg), (2, palette.unknown));
        let (x, y) = cell_at(&compact, " - ").unwrap();
        assert_eq!(compact[(x, y)].bg, palette.unknown);
        let (x, y) = cell_at(&compact, "↻ -").unwrap();
        assert_eq!(compact[(x, y)].fg, palette.muted);
        assert!(app.crew_area.y > 3 && app.input_area.bottom() <= 24);

        let full = screen(&mut app, 120, 40);
        let deck = region(&full, Rect::new(0, 2, 120, token_burn::FULL_CARD_HEIGHT));
        for expected in [
            "claude · known",
            "claude · fresh",
            "claude · held",
            "64%",
            "12%",
            "▸ provider default · unscored",
            "▸ latched",
        ] {
            assert!(deck.contains(expected), "{expected}\n{deck}");
        }
        assert_eq!(full[(3, 2)].symbol(), "╭");
        let (x, y) = cell_at(&full, "--").unwrap();
        assert_eq!(full[(x, y)].fg, palette.unknown);
        let (x, y) = cell_at(&full, "▸ latched").unwrap();
        assert_eq!(full[(x, y)].fg, palette.muted);
        assert!(app.input_area.bottom() <= 40);
    }

    #[cfg(unix)]
    #[test]
    fn commander_specialist_and_deck_are_visible_together() {
        use auth::Provider;
        use usage::tests::Fixture;

        let fixture = Fixture::new("deck-crew", ORCHESTRATOR_PROVIDER);
        let repository = fixture.root.join("work");
        specialists::tests::git_repository(&repository);
        let profile = fixture.profile(Provider::Claude, "alpha");
        observe(&profile, "allowed", 0.5, 0.25);
        let mut app = App::from_live(test_live(repository, vec![profile], None));
        send(&mut app, "/spawn atlas builder");
        assert_eq!(app.crew.len(), 2, "{}", app.notice);
        for (width, height) in SIZES {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            assert!(
                crew.contains("Commander") && crew.contains("atlas"),
                "{crew}"
            );
            let whole = region(&buffer, buffer.area);
            assert!(whole.contains("claude · alpha") && whole.contains("50%"));
            assert!(region(&buffer, app.input_area).contains("Give atlas a task"));
            app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
            let buffer = screen(&mut app, width, height);
            let whole = region(&buffer, buffer.area);
            assert!(whole.contains("Spawned atlas"), "{width}x{height}\n{whole}");
            assert!(whole.contains("claude · alpha"));
            assert!(region(&buffer, app.input_area).contains("Message Commander"));
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            app.editing = true;
            app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            assert_eq!(app.input, "x");
            app.input.clear();
        }
    }
}
