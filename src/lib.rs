mod auth;
mod branches;
mod catalog;
mod checkpoint;
mod cli;
pub mod clipboard;
mod connect;
mod control;
mod daemon;
pub mod editor;
mod goal;
mod http;
mod install;
pub mod markdown;
mod mcp;
mod mission;
mod native;
pub mod palette;
mod panel;
mod permissions;
mod review;
mod routing;
mod settings;
mod side;
mod specialists;
mod targeting;
pub mod token_burn;
mod triage;
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
const UPDATE_WIDE_MIN_WIDTH: u16 = 100;
const ABANDON_LABEL: &str = "  F4 Abandon  ";
const SHORTCUTS: &str =
    " ^Q Quit    ^N New mission    ^P Pause / resume    ^L Clear feed    F7 Settled";

fn framed(mut request: routing::Request, frame: impl Fn(&str) -> String) -> routing::Request {
    request.units = request.units.iter().map(|unit| frame(unit)).collect();
    request
}

struct SideAnswer {
    thread: String,
    entry: Entry,
}

struct Snooze {
    thread: String,
    banner: Entry,
}

impl Snooze {
    fn new(thread: &str, label: &str) -> Self {
        Self {
            thread: thread.into(),
            banner: Entry::now(format!(
                "SNOOZED  {label} Feed collapsed until the mission resumes."
            )),
        }
    }
}

fn handoff_link(from: &auth::Profile, to: &auth::Profile) -> String {
    if from.to_string() == to.to_string() {
        format!("Reset passed: resumed on {from}")
    } else if from.provider != to.provider {
        format!("Limit: {from} → {to} (cross-provider fallback)")
    } else {
        format!("Limit: {from} → {to}")
    }
}

fn reset_clock(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0).map_or_else(
        || "an unknown time".into(),
        |utc| {
            utc.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        },
    )
}

#[derive(Clone, Debug, PartialEq)]
struct Delegation {
    name: String,
    task: String,
    target: Option<targeting::Target>,
    review: bool,
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
    pending: Option<(usize, checkpoint::Claim)>,
    abandon_area: Rect,
    runs: Vec<Run>,
    store: Option<specialists::Store>,
    queue: VecDeque<Delegation>,
    tools: branches::Tools,
    entry: Option<(String, &'static str)>,
    routes: Vec<ProfileRoute>,
    allowances: Vec<usage::Allowance>,
    signed_in: Vec<bool>,
}

const MAX_PARALLEL_SPECIALISTS: usize = 4;
const EVENTS_PER_POLL: usize = 64;

struct Run {
    name: String,
    mission: mission::Mission,
    reply: String,
    continuation: Option<String>,
    reviewing: bool,
}

impl Run {
    fn new(name: &str, mission: mission::Mission, reviewing: bool) -> Self {
        Self {
            name: name.into(),
            mission,
            reply: String::new(),
            continuation: None,
            reviewing,
        }
    }

    fn next_event(&self) -> Option<mission::Event> {
        match self.mission.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(mission::Event::Finished(Err(
                io::Error::other("Mission worker stopped"),
            ))),
        }
    }
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
            pending: None,
            abandon_area: Rect::default(),
            runs: Vec::new(),
            queue: VecDeque::new(),
            tools: branches::Tools::default(),
            entry: None,
            routes: Vec::new(),
            allowances: Vec::new(),
            signed_in: Vec::new(),
        }
    }

    fn busy(&self) -> bool {
        !self.runs.is_empty()
    }

    fn running(&self, name: &str) -> bool {
        self.run_position(name).is_some()
    }

    fn run_position(&self, name: &str) -> Option<usize> {
        self.runs
            .iter()
            .position(|run| run.name.eq_ignore_ascii_case(name))
    }

    fn specialist_runs(&self) -> usize {
        self.runs.iter().filter(|run| run.name != COMMANDER).count()
    }

    fn next_ready(&mut self) -> Option<Delegation> {
        if self.specialist_runs() >= MAX_PARALLEL_SPECIALISTS {
            return None;
        }
        let position = self
            .queue
            .iter()
            .position(|delegation| !self.running(&delegation.name))?;
        self.queue.remove(position)
    }

    fn reset_profiles(&mut self, profiles: Vec<auth::Profile>) {
        self.profiles = profiles;
        self.preferred = None;
        self.last_routed = None;
        self.running_profile = None;
        self.usage = None;
        self.usage_checked_at = None;
        self.load_override();
        self.pending = checkpoint::Claim::pending(&self.profiles, &self.workspace)
            .ok()
            .flatten();
        self.refresh_routes();
        self.read_allowances(usage::now());
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

    fn route(
        &self,
        request: routing::Request,
        target: Option<targeting::Target>,
        mode: permissions::PermissionMode,
        limits: mission::LimitPolicy,
    ) -> mission::Route {
        mission::Route {
            request,
            limits,
            preferred: self.preferred.map(|index| self.profiles[index].to_string()),
            catalog: self.catalog(),
            pins: self.pins.clone(),
            target,
            mode,
        }
    }

    fn side_profile(&self, now: usage::Tick) -> Option<auth::Profile> {
        [self.running_profile, self.preferred, self.last_routed]
            .into_iter()
            .flatten()
            .chain(0..self.profiles.len())
            .filter(|index| *index < self.profiles.len())
            .find(|index| {
                usage::Usage::read(&self.profiles[*index]).is_ok_and(|usage| !usage.blocked(now))
            })
            .map(|index| self.profiles[index].clone())
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
        if self.busy()
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
    pub show_settled: bool,
    touch_clock: u64,
    pub deck_palette: token_burn::TokenBurnPalette,
    update_receiver: Option<Receiver<String>>,
    pub update_available: Option<String>,
    pub update_area: Rect,
    exit_message: Option<String>,
    deck_hits: Vec<(usize, Rect)>,
    pub settings_area: Rect,
    pub full_redraw: bool,
    panel: Option<panel::Panel>,
    panel_hits: Vec<(panel::Action, Rect)>,
    host: Option<Box<dyn panel::Host>>,
    settings: settings::Settings,
    launch: settings::LaunchConfig,
    goal: Option<goal::Goal>,
    side_chats: Vec<side::SideChat>,
    side_answers: Vec<SideAnswer>,
    snoozes: Vec<Snooze>,
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
            show_settled: false,
            touch_clock: 0,
            deck_palette: token_burn::TokenBurnPalette::default(),
            update_receiver: None,
            update_available: None,
            update_area: Rect::default(),
            exit_message: None,
            deck_hits: Vec::new(),
            settings_area: Rect::default(),
            full_redraw: false,
            panel: None,
            panel_hits: Vec::new(),
            host: None,
            settings: settings::Settings::default(),
            launch: settings::LaunchConfig::default(),
            goal: None,
            side_chats: Vec::new(),
            side_answers: Vec::new(),
            snoozes: Vec::new(),
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

    fn attach(&mut self, host: Box<dyn panel::Host>, launch: settings::LaunchConfig) {
        if let Some(path) = &launch.settings_path {
            match settings::Settings::load(path) {
                Ok(loaded) => self.settings = loaded,
                Err(error) => {
                    self.notice = format!("Settings unreadable: {error}. Using defaults.")
                }
            }
        }
        self.crew[0].permission = self.settings.commander_permission;
        self.launch = launch;
        self.host = Some(host);
        self.refresh_sign_in();
        if self.no_provider_connected() {
            self.open_connect(panel::Origin::Onboarding);
        }
    }

    pub fn launch_update_check(&self) -> bool {
        settings::launch_update_check(&self.settings, self.launch.no_update_check.as_ref())
    }

    pub fn panel_open(&self) -> bool {
        self.panel.is_some()
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
        if live.busy() || live.profiles.is_empty() {
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
        self.poll_side();
        for _ in 0..EVENTS_PER_POLL {
            let events = self.next_events();
            if events.is_empty() {
                return;
            }
            for (name, event) in events {
                self.handle_event(&name, event);
            }
        }
    }

    fn next_events(&self) -> Vec<(String, mission::Event)> {
        let Some(live) = &self.live else {
            return Vec::new();
        };
        live.runs
            .iter()
            .filter_map(|run| Some((run.name.clone(), run.next_event()?)))
            .collect()
    }

    fn touch(&mut self, index: usize) {
        self.touch_clock += 1;
        self.crew[index].touched = self.touch_clock;
    }

    fn handle_event(&mut self, name: &str, event: mission::Event) {
        let live = self.live.as_mut().expect("live mission");
        let Some(position) = live.run_position(name) else {
            return;
        };
        let Some(index) = specialists::find(&self.crew, name) else {
            live.runs.remove(position);
            return;
        };
        match event {
            mission::Event::Running { profile, reason } => {
                self.crew[index].status = Status::Running;
                if !live.runs[position].reviewing && !self.crew[index].is_commander() {
                    self.crew[index]
                        .review
                        .record_author(live.profiles[profile].provider);
                }
                live.running_profile = Some(profile);
                live.last_routed = Some(profile);
                live.usage_checked_at = None;
                live.refresh_usage();
                self.touch(index);
                if let Some(reason) = reason {
                    self.notice = reason.clone();
                    self.push_log(format!("SYSTEM  {reason}"));
                }
            }
            mission::Event::Continuing { from, to } => {
                let link = handoff_link(&live.profiles[from], &live.profiles[to]);
                live.runs[position].continuation = Some(link.clone());
                if name == COMMANDER {
                    live.pending = None;
                }
                self.snoozes.retain(|snooze| snooze.thread != name);
                self.crew[index].status = Status::Running;
                self.notice = link.clone();
                self.push_log(format!("SYSTEM  {link}"));
            }
            mission::Event::Snoozed { profile, until } => {
                let label = format!(
                    "Snoozed: {} limit resets at {}; auto-resume re-verifies the workspace first.",
                    live.profiles[profile],
                    reset_clock(until)
                );
                self.crew[index].status = Status::Snoozed;
                self.snoozes.retain(|snooze| snooze.thread != name);
                self.snoozes.push(Snooze::new(name, &label));
                self.notice = label.clone();
                self.push_log(format!("SYSTEM  {label}"));
            }
            mission::Event::Output(text) => {
                let run = &mut live.runs[position];
                run.reply.push_str(&text);
                run.reply.push('\n');
                let speaker = if run.reviewing {
                    format!("{name} review")
                } else {
                    name.to_owned()
                };
                for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
                    self.record(index, format!("{speaker}  {line}"));
                }
            }
            mission::Event::Finished(result) => {
                let run = live.runs.remove(position);
                self.finish_mission(index, run, result)
            }
        }
    }

    fn settle_live_state(&mut self) {
        let live = self.live.as_mut().expect("live mission");
        if live.pending.is_some() {
            if let Ok(pending) = checkpoint::Claim::pending(&live.profiles, &live.workspace) {
                live.pending = pending;
            }
        }
        if !live.busy() {
            live.running_profile = None;
        }
        live.usage_checked_at = None;
        live.refresh_usage();
    }

    fn finish_mission(&mut self, index: usize, run: Run, result: io::Result<()>) {
        self.settle_live_state();
        self.touch(index);
        let name = self.crew[index].name.clone();
        self.snoozes.retain(|snooze| snooze.thread != name);
        let outcome: Result<String, String> = match &result {
            Ok(()) => Ok(run.reply.clone()),
            Err(error) => Err(error.to_string()),
        };
        if self.crew[index].is_commander() {
            self.finish_commander(run, result);
        } else {
            self.finish_specialist(index, &run, result);
        }
        self.advance_goal(&name, outcome.as_deref().map_err(String::as_str));
        self.start_next();
    }

    fn finish_commander(&mut self, run: Run, result: io::Result<()>) {
        self.crew[0].status = if result.is_ok() {
            Status::Done
        } else {
            Status::Error
        };
        self.notice = match &result {
            Ok(()) => run.continuation.map_or_else(
                || format!("{COMMANDER} replied."),
                |link| format!("Done · {link}"),
            ),
            Err(error) => format!("{COMMANDER}: {error}"),
        };
        if result.is_ok() {
            self.apply_directives(&run.reply);
        }
    }

    fn finish_specialist(&mut self, index: usize, run: &Run, result: io::Result<()>) {
        let approval = (!run.reviewing)
            .then(|| specialists::approval_request(&run.reply))
            .flatten();
        let (status, outcome) = match &result {
            Ok(()) => match approval {
                Some(question) => (Status::NeedsApproval, question),
                None => (Status::Done, specialists::summary(&run.reply)),
            },
            Err(error) => (Status::Error, error.to_string()),
        };
        let member = &mut self.crew[index];
        member.status = status;
        member.refresh_changes();
        let name = member.name.clone();
        let label = status.label();
        let kind = if run.reviewing { " review" } else { "" };
        self.push_log(format!("{name}{kind}  {label} · {outcome}"));
        self.notice = format!("{name} {label}");
        self.save_roster();
    }

    fn limit_policy(&self) -> mission::LimitPolicy {
        mission::LimitPolicy {
            auto_resume_on_reset: self.settings.auto_resume_on_reset,
            fallback_profile: self.settings.fallback_profile.clone(),
        }
    }

    fn start_goal(&mut self, criteria: &str) {
        if let Some(active) = &self.goal {
            self.notice = format!(
                "Goal already pinned on {} ({}). /goal cancel first.",
                active.member,
                active.status()
            );
            return;
        }
        let member = self.crew[self.selected].name.clone();
        if self.selected == 0 && self.mission_running() {
            self.notice = "Wait for the running mission before pinning a Commander goal.".into();
            return;
        }
        let mut goal = goal::Goal::new(&member, criteria, self.settings.goal_cap());
        let prompt = goal.begin();
        self.notice = format!("Goal pinned on {member}: {}", goal.status());
        self.push_log(format!(
            "SYSTEM  Goal pinned on {member}: {}",
            goal.status()
        ));
        self.goal = Some(goal);
        self.send_goal_turn(&member, prompt);
    }

    fn cancel_goal(&mut self) {
        self.notice = match self.goal.take() {
            Some(goal) => {
                let message = format!(
                    "Goal cancelled on {} after {} of {} turns.",
                    goal.member, goal.turns, goal.cap
                );
                self.push_log(format!("SYSTEM  {message}"));
                message
            }
            None => "No goal is pinned.".into(),
        };
    }

    fn advance_goal(&mut self, member: &str, outcome: Result<&str, &str>) {
        let Some(goal) = self.goal.as_mut().filter(|goal| goal.member == member) else {
            return;
        };
        let message = match goal.after_turn(outcome) {
            goal::Step::Continue(prompt) => {
                let status = goal.status();
                self.push_log(format!("SYSTEM  Goal continues: {status}"));
                self.send_goal_turn(member, prompt);
                return;
            }
            goal::Step::Met => format!("Goal met on {member}: {}", goal.status()),
            goal::Step::CapReached => format!(
                "Goal stopped at the {}-turn safety cap on {member}: {}",
                goal.cap, goal.criteria
            ),
            goal::Step::Failed(error) => format!("Goal stopped on {member}: {error}"),
        };
        self.goal = None;
        self.notice = message.clone();
        self.push_log(format!("SYSTEM  {message}"));
    }

    fn send_goal_turn(&mut self, member: &str, prompt: String) {
        let typed = std::mem::take(&mut self.input);
        match specialists::find(&self.crew, member) {
            Some(0) => self.message_commander(prompt),
            Some(index) => self.delegate(index, prompt),
            None => {}
        }
        self.input = typed;
        if !self.turn_scheduled(member) {
            let reason = std::mem::take(&mut self.notice);
            self.goal = None;
            self.notice = format!("Goal stopped on {member}: {reason}");
        }
    }

    fn turn_scheduled(&self, member: &str) -> bool {
        self.live.as_ref().is_some_and(|live| {
            live.running(member) || live.queue.iter().any(|queued| queued.name == member)
        })
    }

    fn ask_side(&mut self, question: &str) {
        let Some(live) = &self.live else {
            self.notice = PREVIEW_NOTICE.into();
            return;
        };
        let Some(profile) = live.side_profile(usage::now()) else {
            self.notice = format!("{}.", panel::NO_PROVIDER_HINT);
            return;
        };
        let member = &self.crew[self.selected];
        let workspace = member.workspace.as_ref().map_or_else(
            || live.workspace.clone(),
            |workspace| workspace.path.clone(),
        );
        let history: Vec<&str> = if member.is_commander() {
            self.log.iter().map(String::as_str).collect()
        } else {
            member
                .history
                .iter()
                .map(|entry| entry.text.as_str())
                .collect()
        };
        let label = profile.to_string();
        let chat = side::SideChat::launch(&member.name, question, profile, workspace, &history);
        self.side_chats.push(chat);
        self.notice = format!("Side question sent to {label}; the mission keeps running.");
    }

    fn poll_side(&mut self) {
        let mut waiting = Vec::new();
        for chat in std::mem::take(&mut self.side_chats) {
            match chat.poll() {
                side::Poll::Waiting => waiting.push(chat),
                side::Poll::Answered(result) => self.show_side_answer(&chat, result),
            }
        }
        waiting.append(&mut self.side_chats);
        self.side_chats = waiting;
    }

    fn show_side_answer(&mut self, chat: &side::SideChat, result: io::Result<Vec<String>>) {
        let mut lines = vec![format!("BTW  Q: {}", chat.question)];
        match result {
            Ok(answer) if answer.is_empty() => lines.push("BTW  (no answer)".into()),
            Ok(answer) => lines.extend(answer.into_iter().map(|line| format!("BTW  {line}"))),
            Err(error) => lines.push(format!("BTW  Side chat failed: {error}")),
        }
        for text in lines {
            self.side_answers.push(SideAnswer {
                thread: chat.thread.clone(),
                entry: Entry::now(text),
            });
        }
        let overflow = self
            .side_answers
            .len()
            .saturating_sub(specialists::FEED_LIMIT);
        self.side_answers.drain(..overflow);
        self.notice = format!("Side answer ready in the {} feed.", chat.thread);
    }

    fn is_snoozed(&self, thread: &str) -> bool {
        self.snoozes.iter().any(|snooze| snooze.thread == thread)
    }

    fn snoozed(&self) -> bool {
        self.live
            .as_ref()
            .is_some_and(|live| live.runs.iter().all(|run| self.is_snoozed(&run.name)))
    }

    fn apply_directives(&mut self, reply: &str) {
        for directive in specialists::directives(reply) {
            match directive {
                specialists::Directive::Spawn { name, role, task } => {
                    if let (Some(index), Some(task)) =
                        (self.spawn(&name, &role, None, task.as_deref()), task)
                    {
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
        self.touch(index);
    }

    fn spawn(
        &mut self,
        name: &str,
        role: &str,
        mode: Option<permissions::PermissionMode>,
        task: Option<&str>,
    ) -> Option<usize> {
        if let Err(error) = specialists::validate_name(name, &self.crew) {
            self.notice = error;
            return None;
        }
        let mode = mode.unwrap_or_else(|| permissions::PermissionMode::default_for_role(role));
        let workspace = match self
            .live
            .as_ref()
            .and_then(|live| Some((live.store.as_ref()?, &live.workspace)))
        {
            Some((store, base)) => {
                match store.isolate_for(base, name, task.unwrap_or(role), mode) {
                    Ok(workspace) => Some(workspace),
                    Err(error) => {
                        self.notice = error;
                        return None;
                    }
                }
            }
            None => match mode.require_isolation(None) {
                Ok(()) => None,
                Err(refusal) => {
                    self.notice = refusal;
                    return None;
                }
            },
        };
        self.crew
            .push(Member::new(name, role, None, workspace).with_permission(Some(mode)));
        let index = self.crew.len() - 1;
        self.touch(index);
        self.crew[index].refresh_changes();
        let branch = self.crew[index]
            .branch()
            .map_or_else(String::new, |branch| format!(" on {}", branch.name));
        self.push_log(format!(
            "SYSTEM  Spawned {name} ({role}, {}) · {}{branch}",
            mode.flag(),
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
        let targeted = match targeting::extract(&task) {
            Ok(targeted) if targeted.text.is_empty() => {
                self.notice = "Enter a task after the @model target.".into();
                return;
            }
            Ok(targeted) => targeted,
            Err(error) => {
                self.notice = error;
                return;
            }
        };
        self.crew[index].mission = targeted.text.clone();
        self.crew[index].settled = false;
        self.record(index, format!("TASK  {task}"));
        let Some(live) = &mut self.live else {
            self.notice = PREVIEW_NOTICE.into();
            return;
        };
        let busy = live.running(&name);
        live.queue.push_back(Delegation {
            name: name.clone(),
            task: targeted.text,
            target: targeted.target,
            review: false,
        });
        self.push_log(format!("SYSTEM  Delegated to {name}: {task}"));
        if busy {
            self.notice = format!("{name} is busy; the task is queued for its workspace.");
        }
        self.save_roster();
        self.start_next();
    }

    fn review(&mut self, name: &str, focus: Option<String>, next_revision: bool) {
        let Some(index) = specialists::find(&self.crew, name) else {
            self.notice = format!("No specialist named {name}.");
            return;
        };
        let Some(live) = &mut self.live else {
            self.notice = PREVIEW_NOTICE.into();
            return;
        };
        let signed_in = review::signed_in_providers(&live.profiles);
        let reviewed = self.crew[index].workspace.as_ref().map(|w| w.path.clone());
        let base = live.workspace.clone();
        let mut roots: Vec<&Path> = reviewed.iter().map(PathBuf::as_path).collect();
        roots.push(&base);
        let planned = review::plan(
            &mut self.crew[index],
            &review::ReviewRequest {
                focus: focus.as_deref(),
                next_revision,
                signed_in: &signed_in,
                rule_roots: &roots,
            },
        );
        let plan = match planned {
            Ok(plan) => plan,
            Err(refusal) => {
                self.push_log(format!("SYSTEM  {refusal}"));
                self.notice = refusal;
                self.save_roster();
                return;
            }
        };
        let name = self.crew[index].name.clone();
        live.queue.push_back(Delegation {
            name: name.clone(),
            task: plan.prompt,
            target: Some(plan.target),
            review: true,
        });
        self.push_log(format!(
            "SYSTEM  Review {name} round {}/{} · {} · read-only",
            plan.round,
            review::MAX_REVIEW_ROUNDS,
            plan.note
        ));
        self.save_roster();
        self.start_next();
    }

    fn start_next(&mut self) {
        while let Some(delegation) = self.live.as_mut().and_then(Live::next_ready) {
            let Some(index) = specialists::find(&self.crew, &delegation.name) else {
                continue;
            };
            if let Err(reason) = self.launch_specialist(index, delegation) {
                let name = self.crew[index].name.clone();
                self.crew[index].status = Status::Error;
                self.push_log(format!("{name}  ERROR · {reason}"));
                self.notice = reason;
                self.save_roster();
            }
        }
    }

    fn launch_specialist(&mut self, index: usize, delegation: Delegation) -> Result<(), String> {
        let limits = self.limit_policy();
        let live = self.live.as_mut().expect("live delegation");
        let member = &self.crew[index];
        let workspace = member
            .workspace
            .as_ref()
            .ok_or_else(|| format!("{} has no isolated workspace.", member.name))?;
        if live.profiles.is_empty() {
            return Err(format!("{}.", panel::NO_PROVIDER_HINT));
        }
        let (request, mode) = if delegation.review {
            let request = routing::Request {
                units: vec![delegation.task],
                ..routing::Request::default()
            };
            (request, permissions::PermissionMode::ReadOnly)
        } else {
            member
                .permission
                .require_isolation(Some(&workspace.isolation))?;
            let request = framed(routing::parse(&delegation.task)?, |unit| {
                specialists::specialist_prompt(member, unit)
            });
            (request, member.permission)
        };
        let mission = mission::Mission::launch(
            live.profiles.clone(),
            live.route(request, delegation.target, mode, limits),
            workspace.path.clone(),
        );
        live.runs
            .push(Run::new(&member.name, mission, delegation.review));
        self.crew[index].status = Status::Running;
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
                self.update_available = Some(latest);
                self.update_receiver = None;
            }
            Err(TryRecvError::Disconnected) => self.update_receiver = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    fn deck_card_at(&self, point: ratatui::layout::Position) -> Option<usize> {
        self.deck_hits
            .iter()
            .find(|(_, area)| area.contains(point))
            .map(|(index, _)| *index)
    }

    pub fn take_exit_message(&mut self) -> Option<String> {
        self.exit_message.take()
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
        self.live.as_ref().is_some_and(Live::busy)
    }

    fn is_running(&self, name: &str) -> bool {
        self.live.as_ref().is_some_and(|live| live.running(name))
    }

    pub fn toggle_pause(&mut self) {
        let name = self.crew[self.selected].name.clone();
        if self.is_running(&name) {
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
            specialists::Command::Spawn {
                name,
                role,
                mode,
                task,
            } => {
                if let Some(index) = self.spawn(&name, &role, mode, task.as_deref()) {
                    self.selected = index;
                    if let Some(task) = task {
                        self.delegate(index, task);
                    }
                }
            }
            specialists::Command::Ask { name, task } => self.ask(&name, task),
            specialists::Command::Review {
                name,
                focus,
                next_revision,
            } => self.review(&name, focus, next_revision),
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
            specialists::Command::Diff { name } => self.show_diff(name.as_deref()),
            specialists::Command::Pr { name, commit } => self.open_pr(name.as_deref(), commit),
            specialists::Command::Settle { name, settled } => self.settle(&name, settled),
            specialists::Command::SettledDrawer => self.toggle_settled_drawer(),
            specialists::Command::Settings => self.open_settings(),
            specialists::Command::Connect => self.open_connect(panel::Origin::Direct),
            specialists::Command::Update => self.request_update(),
            specialists::Command::Disconnect { target } => self.disconnect_command(target),
            specialists::Command::Reconnect { target } => self.reconnect_command(target),
            specialists::Command::Side { question } => self.ask_side(&question),
            specialists::Command::Goal { criteria } => self.start_goal(&criteria),
            specialists::Command::GoalCancel => self.cancel_goal(),
            specialists::Command::Mode { mode } => self.set_commander_mode(mode),
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
        let mut retained = None;
        if let Some(live) = &mut self.live {
            if live.running(&member.name) {
                self.notice = format!("{} is running; remove it after it finishes.", member.name);
                return;
            }
            live.queue.retain(|queued| queued.name != member.name);
            match live.store.as_ref().map(|store| store.release(member)) {
                Some(Err(error)) => {
                    self.notice = format!("Cannot remove {}: {error}", member.name);
                    return;
                }
                Some(Ok(branch)) => retained = branch,
                None => {}
            }
        }
        let member = self.crew.remove(index);
        self.selected = match self.selected {
            selected if selected == index => 0,
            selected if selected > index => selected - 1,
            selected => selected,
        };
        let note = specialists::removal_note(&member.name, retained.as_deref());
        self.push_log(format!("SYSTEM  {note}"));
        self.save_roster();
    }

    fn target(&self, name: Option<&str>, usage: &str) -> Result<usize, String> {
        let index = match name {
            Some(name) => specialists::find(&self.crew, name)
                .ok_or_else(|| format!("No specialist named {name}."))?,
            None => self.selected,
        };
        if self.crew[index].is_commander() {
            return Err(format!("Select a specialist or name one. {usage}"));
        }
        Ok(index)
    }

    fn show_diff(&mut self, name: Option<&str>) {
        let index = match self.target(name, "Usage: /diff [NAME]") {
            Ok(index) => index,
            Err(error) => return self.notice = error,
        };
        match self.crew[index].diff() {
            Ok(stat) => {
                let report = specialists::diff_report(&self.crew[index], &stat);
                self.notice = format!("{} {}", self.crew[index].name, stat.label());
                self.crew[index].changes = Some(stat);
                self.push_log(format!("SYSTEM  Diff {report}"));
            }
            Err(error) => self.notice = error,
        }
    }

    fn open_pr(&mut self, name: Option<&str>, commit: bool) {
        let index = match self.target(name, "Usage: /pr [NAME] [--commit]") {
            Ok(index) => index,
            Err(error) => return self.notice = error,
        };
        if self.live.is_none() {
            return self.notice = PREVIEW_NOTICE.into();
        }
        let tools = self
            .live
            .as_ref()
            .map(|live| live.tools.clone())
            .unwrap_or_default();
        let result = specialists::publish(&self.crew[index], commit, &tools);
        self.crew[index].refresh_changes();
        let name = self.crew[index].name.clone();
        match result {
            Ok(published) => {
                let report = published.describe();
                self.notice = format!("{name}: {report}");
                self.push_log(format!("SYSTEM  PR {name}: {report}"));
            }
            Err(error) => self.notice = error,
        }
    }

    fn settle(&mut self, name: &str, settled: bool) {
        let Some(index) = specialists::find(&self.crew, name) else {
            return self.notice = format!("No specialist named {name}.");
        };
        if settled && self.is_running(name) {
            return self.notice = format!(
                "{} is running; settle it after it finishes.",
                self.crew[index].name
            );
        }
        match specialists::settle(&mut self.crew[index], settled) {
            Ok(note) => {
                if settled && self.selected == index && !self.show_settled {
                    self.selected = 0;
                }
                self.push_log(format!("SYSTEM  {note}"));
                self.save_roster();
            }
            Err(error) => self.notice = error,
        }
    }

    fn toggle_settled_drawer(&mut self) {
        self.show_settled = !self.show_settled;
        if !self.show_settled && self.crew[self.selected].settled {
            self.selected = 0;
        }
    }

    fn roster_rows(&self) -> Vec<specialists::RosterRow> {
        specialists::inbox_rows(&self.crew, self.show_settled)
    }

    fn step_selection(&mut self, forward: bool) {
        let members: Vec<usize> = self
            .roster_rows()
            .into_iter()
            .filter_map(specialists::RosterRow::member)
            .collect();
        let position = members
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = if forward {
            (position + 1).min(members.len() - 1)
        } else {
            position.saturating_sub(1)
        };
        self.selected = members[next];
    }

    fn set_entry(&mut self, profile: &str) {
        let Some(live) = &mut self.live else {
            self.notice = "Preview: sign in to choose an entry profile.".into();
            return;
        };
        if live.busy() {
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

    pub(crate) fn set_commander_mode(&mut self, mode: permissions::PermissionMode) {
        self.settings.commander_permission = mode;
        self.crew[0].permission = mode;
        let change = format!("Commander permissions: {}", mode.flag());
        self.notice = match &self.launch.settings_path {
            Some(path) => match self.settings.save(path) {
                Ok(()) => format!("{change} (saved)."),
                Err(error) => format!("{change} · not saved: {error}"),
            },
            None => format!("{change} (no config directory; not saved)."),
        };
    }

    fn message_commander(&mut self, command: String) {
        let limits = self.limit_policy();
        let commander_mode = self.settings.commander_permission;
        let Some(live) = &mut self.live else {
            self.crew[0].mission = command.clone();
            self.push_log(format!("YOU  {command}"));
            self.input.clear();
            self.notice = PREVIEW_NOTICE.into();
            return;
        };
        if live.running(COMMANDER) {
            self.notice = if self.is_snoozed(COMMANDER) {
                "A mission is snoozed until its limit resets; /btw still works.".into()
            } else {
                "Commander is still working; wait for its reply.".into()
            };
            return;
        }
        if self.crew[0].paused {
            self.notice = "Resume Commander before sending a message.".into();
            return;
        }
        if let Some((index, claim)) = live.pending.clone() {
            let mission = mission::Mission::resume(
                live.profiles.clone(),
                index,
                live.workspace.clone(),
                command.clone(),
                claim,
            );
            live.runs.push(Run::new(COMMANDER, mission, false));
            self.crew[0].mission = command;
            self.input.clear();
            return;
        }
        if live.profiles.is_empty() {
            self.notice = format!("{}.", panel::NO_PROVIDER_HINT);
            return;
        }
        let targeted = match targeting::extract(&command) {
            Ok(targeted) => targeted,
            Err(error) => {
                self.notice = error;
                return;
            }
        };
        if let Err(refusal) = commander_mode.require_isolation(None) {
            self.notice = format!("{refusal} {}", permissions::COMMANDER_MODE_USAGE);
            return;
        }
        let request = match routing::parse(&targeted.text) {
            Ok(request) => framed(request, |unit| {
                specialists::commander_prompt(self.log.iter().map(String::as_str), &self.crew, unit)
            }),
            Err(error) => {
                self.notice = error;
                return;
            }
        };
        self.notice = format!("Routing in {}", live.workspace.display());
        let mission = mission::Mission::launch(
            live.profiles.clone(),
            live.route(request, targeted.target, commander_mode, limits),
            live.workspace.clone(),
        );
        live.runs.push(Run::new(COMMANDER, mission, false));
        self.crew[0].mission = command.clone();
        self.input.clear();
        self.push_log(format!("YOU  {command}"));
    }

    fn abandon_continuation(&mut self) {
        let Some(live) = &mut self.live else { return };
        if live.running(COMMANDER) {
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
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.panel.is_some() && !(control && matches!(key.code, KeyCode::Char('q' | 'c'))) {
            return self.panel_key(key);
        }
        if control {
            match key.code {
                KeyCode::Char('q' | 'c') if self.mission_running() && !self.snoozed() => {
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
            KeyCode::F(3) => self.open_settings(),
            KeyCode::F(4) => self.abandon_continuation(),
            KeyCode::F(6) => self.request_update(),
            KeyCode::F(7) => self.toggle_settled_drawer(),
            KeyCode::Tab => self.editing = !self.editing,
            KeyCode::Esc => self.editing = false,
            KeyCode::Enter if self.editing => self.submit(),
            KeyCode::Enter => self.editing = true,
            KeyCode::Backspace if self.editing => {
                self.input.pop();
            }
            KeyCode::Char(c) if self.editing => self.input.push(c),
            KeyCode::Up => self.step_selection(false),
            KeyCode::Down => self.step_selection(true),
            KeyCode::Home => self.selected = 0,
            _ => {}
        }
    }
    pub fn click(&mut self, x: u16, y: u16) {
        let point = ratatui::layout::Position::new(x, y);
        if self.panel.is_some() {
            self.panel_click(x, y);
        } else if self.settings_area.contains(point) {
            self.open_settings();
        } else if self.update_area.contains(point) {
            self.request_update();
        } else if let Some(index) = self.deck_card_at(point) {
            self.open_subscription(index, false);
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
            match self.roster_rows().get(self.crew_offset + row) {
                Some(specialists::RosterRow::Member(index)) => self.selected = *index,
                Some(specialists::RosterRow::Settled { .. }) => self.toggle_settled_drawer(),
                None => {}
            }
            self.editing = false;
        }
    }
    fn crew_counts(&self) -> [String; 4] {
        let count = |wanted: fn(Status) -> bool| {
            self.crew
                .iter()
                .filter(|member| !member.settled && wanted(member.status))
                .count()
        };
        [
            format!("{:02}  CREW", self.crew.len()),
            format!("{:02}  RUNNING", count(|status| status == Status::Running)),
            format!("{:02}  NEED ATTENTION", count(Status::needs_attention)),
            if self.live.is_some() {
                "LIVE  /  COMMANDER".into()
            } else {
                "PREVIEW  /  NO DISPATCH".into()
            },
        ]
    }

    fn selected_details(&self) -> Vec<Line<'static>> {
        let member = &self.crew[self.selected];
        let mut lines = if member.is_commander() {
            let entry = self
                .live
                .as_ref()
                .map_or_else(|| "preview".into(), Live::entry_label);
            vec![
                Line::raw("ENTRY MODEL"),
                Line::raw(entry),
                Line::raw(""),
                Line::raw("PERMISSIONS"),
                Line::raw(self.settings.commander_permission.flag()),
                Line::raw(""),
                Line::raw("SPECIALISTS"),
                Line::raw(format!("{} on roster", self.crew.len() - 1)),
            ]
        } else {
            let mut details = Vec::new();
            if let Some(branch) = member.branch() {
                let changes = member
                    .changes
                    .as_ref()
                    .map_or_else(|| "diff unavailable".into(), branches::DiffStat::label);
                details.extend([
                    Line::raw("BRANCH"),
                    Line::raw(format!("{} → {}", branch.name, branch.base)),
                    Line::raw(changes),
                    Line::raw(""),
                ]);
            }
            details.extend([
                Line::raw("ROLE"),
                Line::raw(member.role.clone()),
                Line::raw(""),
                Line::raw("WORKSPACE"),
                Line::raw(specialists::workspace_label(member)),
            ]);
            details
        };
        lines.extend(self.goal_lines(&member.name));
        lines
    }

    fn member_lines(&self, index: usize, t: &Theme) -> Vec<Line<'static>> {
        let member = &self.crew[index];
        let chosen = index == self.selected;
        let style = match (chosen, member.settled) {
            (true, _) => Style::default().fg(color("d1dce4")).bold(),
            (false, true) => Style::default().fg(color(&t.muted)),
            (false, false) => Style::default(),
        };
        vec![
            Line::from(vec![
                Span::styled(
                    " ●",
                    Style::default().fg(color(status_color(member.status, t))),
                ),
                Span::styled(format!(" {}  ", member.name), style),
                Span::styled(
                    format!("/{:02}", index + 1),
                    Style::default().fg(color(if chosen { "97a3ad" } else { "5f646b" })),
                ),
            ]),
            Line::styled(
                format!("   {} · {}", member.status.caption(), member.role),
                style,
            ),
        ]
    }

    fn change_badge(member: &Member) -> String {
        member
            .changes
            .as_ref()
            .filter(|changes| !changes.files.is_empty())
            .map_or_else(String::new, |changes| format!("  ·  {}", changes.totals()))
    }

    fn feed_entries(&self) -> Vec<(&str, &str)> {
        let thread = self.crew[self.selected].name.as_str();
        let mut entries: Vec<(&str, &str)> = match self.collapsed_snooze(thread) {
            Some(banner) => vec![(banner.at.as_str(), banner.text.as_str())],
            None if self.selected == 0 => self
                .log_times
                .iter()
                .zip(&self.log)
                .map(|(at, text)| (at.as_str(), text.as_str()))
                .collect(),
            None => self.crew[self.selected]
                .history
                .iter()
                .map(|entry| (entry.at.as_str(), entry.text.as_str()))
                .collect(),
        };
        entries.extend(
            self.side_answers
                .iter()
                .filter(|answer| answer.thread == thread)
                .map(|answer| (answer.entry.at.as_str(), answer.entry.text.as_str())),
        );
        entries
    }

    fn collapsed_snooze(&self, thread: &str) -> Option<&Entry> {
        self.snoozes
            .iter()
            .find(|snooze| self.settings.snooze_limited_missions && snooze.thread == thread)
            .map(|snooze| &snooze.banner)
    }

    fn goal_lines(&self, member: &str) -> Vec<Line<'static>> {
        self.goal
            .as_ref()
            .filter(|goal| goal.member == member)
            .map_or_else(Vec::new, |goal| {
                vec![Line::raw(""), Line::raw("GOAL"), Line::raw(goal.status())]
            })
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
        self.draw_menu_bar(f, a);
        let compact = a.height < 36 || a.width < 105;
        self.deck_hits.clear();
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
                self.draw_deck(f, &cards, mode, r(1, 1, a.width.saturating_sub(2), rows));
                rows
            }
        };
        let top_shift = deck_rows.saturating_sub(1);
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
        let rows = self.roster_rows();
        let selected_row = rows
            .iter()
            .position(|row| *row == specialists::RosterRow::Member(self.selected))
            .unwrap_or(0);
        self.crew_offset = (selected_row + 1).saturating_sub(visible);
        for (row_index, roster_row) in rows.iter().skip(self.crew_offset).take(visible).enumerate()
        {
            let y = self.crew_area.y + row_index as u16 * stride;
            let row = r(self.crew_area.x, y, self.crew_area.width, stride);
            let lines = match *roster_row {
                specialists::RosterRow::Member(i) => {
                    if i == self.selected {
                        f.render_widget(
                            Block::default().style(Style::default().bg(color(&t.selection))),
                            row,
                        );
                    }
                    self.member_lines(i, &t)
                }
                specialists::RosterRow::Settled { count, open } => settled_lines(count, open, &t),
            };
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
                        .fg(color(status_color(selected.status, &t)))
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
                        "{}  /  {:02}{}",
                        selected.role.to_uppercase(),
                        self.selected + 1,
                        Self::change_badge(selected)
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
                vec![Line::raw(panel::NO_PROVIDER_HINT)]
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
            .is_some_and(|live| live.pending.is_some() && !live.running(COMMANDER));
        f.render_widget(
            Paragraph::new(if pending && self.notice.is_empty() {
                "Interrupted continuation pending · re-enter task, then Send"
            } else if self.notice.is_empty() && self.no_provider_connected() {
                panel::NO_PROVIDER_HINT
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
        self.draw_shortcut_bar(f, r(0, a.height.saturating_sub(1), a.width, 1), pending);
        self.draw_panel(f, a);
    }
}

fn status_color(status: Status, t: &Theme) -> &str {
    match status {
        Status::Running => &t.amber,
        Status::Error => "b86d67",
        Status::NeedsApproval => "cc955d",
        Status::Snoozed => &t.muted,
        Status::Idle | Status::Done => &t.accent,
    }
}

fn settled_lines(count: usize, open: bool, t: &Theme) -> Vec<Line<'static>> {
    let marker = if open { "▾" } else { "▸" };
    let action = if open { "F7 hide" } else { "F7 show" };
    vec![
        Line::styled(
            format!(" {marker} SETTLED ({count})"),
            Style::default().fg(color(&t.muted)).bold(),
        ),
        Line::styled(format!("   {action}"), Style::default().fg(color("666f78"))),
    ]
}

const MENU_BRAND: &str = "  ▣  AGENT COMMAND";
const MENU_ITEMS: &str = "File   Crew   Missions   Radio    |    PRIVATEER";
const MENU_SHIP_NUMBER: &str = " / 01";
const MENU_SETTINGS: &str = " Settings ";

impl App {
    fn draw_menu_bar(&mut self, f: &mut Frame, area: Rect) {
        let bar = Rect::new(area.x, area.y, area.width, 1).intersection(area);
        let settings_width = MENU_SETTINGS.chars().count() as u16;
        let wide = format!("{MENU_BRAND}     {MENU_ITEMS}{MENU_SHIP_NUMBER}");
        let labels = if wide.chars().count() + usize::from(settings_width) < usize::from(bar.width)
        {
            wide
        } else {
            format!("{MENU_BRAND}   {MENU_ITEMS}")
        };
        f.render_widget(
            Paragraph::new(labels).style(
                Style::default()
                    .fg(color("272c32"))
                    .bg(color(&self.theme.ivory))
                    .bold(),
            ),
            bar,
        );
        self.settings_area = Rect::new(
            bar.right().saturating_sub(settings_width),
            bar.y,
            settings_width,
            1,
        )
        .intersection(bar);
        f.render_widget(
            Paragraph::new(MENU_SETTINGS).style(
                Style::default()
                    .fg(color(&self.theme.ivory))
                    .bg(color("272c32"))
                    .bold(),
            ),
            self.settings_area,
        );
    }

    fn draw_shortcut_bar(&mut self, f: &mut Frame, bar: Rect, pending: bool) {
        let style = Style::default().bg(color("1b2027")).fg(color("a5b1bd"));
        f.render_widget(Block::default().style(style), bar);
        let abandon_width = if pending && self.live.is_some() {
            text_width(ABANDON_LABEL)
        } else {
            0
        };
        let abandon_area = right_of(bar, bar.right(), abandon_width);
        let update_label = self
            .update_available
            .as_deref()
            .map(|latest| update_label(latest, bar.width));
        let update_width = update_label.as_deref().map_or(0, text_width);
        self.update_area = right_of(bar, abandon_area.x, update_width);
        let shortcuts_width = self.update_area.x.saturating_sub(bar.x);
        f.render_widget(
            Paragraph::new(clipped(SHORTCUTS, usize::from(shortcuts_width))).style(style),
            Rect::new(bar.x, bar.y, shortcuts_width, bar.height),
        );
        if let Some(label) = update_label {
            f.render_widget(
                Paragraph::new(label).style(
                    Style::default()
                        .fg(color("1b2027"))
                        .bg(color(&self.theme.amber))
                        .bold(),
                ),
                self.update_area,
            );
        }
        if let Some(live) = &mut self.live {
            live.abandon_area = abandon_area;
            if abandon_width > 0 {
                f.render_widget(Paragraph::new(ABANDON_LABEL).style(style), abandon_area);
            }
        }
    }

    fn draw_deck(
        &mut self,
        f: &mut Frame,
        cards: &[token_burn::AccountCard],
        mode: token_burn::DeckMode,
        area: Rect,
    ) {
        if mode == token_burn::DeckMode::Compact {
            token_burn::render_deck_as(mode, cards, area, f.buffer_mut(), &self.deck_palette);
            self.deck_hits = (0..cards.len().min(usize::from(area.height)))
                .map(|index| {
                    let row = Rect::new(area.x, area.y + index as u16, area.width, 1);
                    (index, row)
                })
                .collect();
            return;
        }
        let shown = &cards[..cards.len().min(FULL_DECK_MAX_CARDS)];
        let columns = Layout::horizontal(vec![Constraint::Fill(1); shown.len()])
            .spacing(1)
            .split(area);
        self.deck_hits = columns.iter().copied().enumerate().collect();
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

fn text_width(text: &str) -> u16 {
    u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX)
}

fn right_of(bar: Rect, right: u16, width: u16) -> Rect {
    let width = width.min(right.saturating_sub(bar.x));
    Rect::new(right - width, bar.y, width, bar.height)
}

fn clipped(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.into();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn update_label(latest: &str, bar_width: u16) -> String {
    if bar_width >= UPDATE_WIDE_MIN_WIDTH {
        format!(" Update v{latest} ")
    } else {
        " Update ".into()
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
                "--model claude-opus-5-5 --effort high --print --verbose --output-format stream-json --setting-sources user --permission-mode acceptEdits --allowedTools Bash,Edit,Write,NotebookEdit --\n"
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

    #[cfg(unix)]
    fn cross_review_provider(argv_log: &Path) -> String {
        format!(
            r#"
case "$1" in
    auth) printf '%s\n' '{{"loggedIn":true,"authMethod":"claude.ai"}}'; exit ;;
    -c)
        shift 4
        if test "$1" = login; then printf '%s\n' 'Logged in using ChatGPT' >&2; exit; fi
        ;;
esac
printf '%s\n' "$*" >> '{}'
prompt=$(cat)
case "$prompt" in
  "You are bolt"*"Task: Build it")
    printf '%s\n' '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Built it"}}]}}}}' ;;
  "You are an adversarial read-only reviewer"*"Focus: edge cases"*"SEVERITY:"*)
    printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"VERDICT: APPROVE"}}}}' ;;
  *) exit 9 ;;
esac
"#,
            argv_log.display()
        )
    }

    #[cfg(unix)]
    #[test]
    fn review_routes_to_the_opposite_family_read_only_and_caps_at_three_rounds() {
        use auth::Provider;
        use usage::tests::Fixture;

        let argv_log = std::env::temp_dir().join(format!("ac-cross-argv-{}", std::process::id()));
        let _ = fs::remove_file(&argv_log);
        let fixture = Fixture::new("cross-review", &cross_review_provider(&argv_log));
        let repository = fixture.root.join("work");
        specialists::tests::git_repository(&repository);
        let profiles = vec![
            fixture.profile(Provider::Claude, "alpha"),
            fixture.profile(Provider::Codex, "beta"),
        ];
        let mut app = App::from_live(test_live(repository, profiles, None));
        send(&mut app, "/spawn bolt builder --mode auto-edits");
        assert_eq!(
            app.crew[1].permission,
            permissions::PermissionMode::AutoAcceptEdits
        );
        send(&mut app, "/ask bolt @claude Build it");
        wait_for(&mut app, |app| app.crew[1].status == Status::Done);
        assert_eq!(app.crew[1].review.author, Some(Provider::Claude));
        assert_eq!(app.crew[1].mission, "Build it");
        send(&mut app, "/review bolt edge cases");
        assert!(
            app.log.contains(
                &"SYSTEM  Review bolt round 1/3 · Cross-family review: claude wrote it, codex reviews it · read-only"
                    .to_owned()
            ),
            "{:?}",
            app.log
        );
        wait_for(&mut app, |app| {
            app.crew[1].status == Status::Done && !app.mission_running()
        });
        assert_eq!(
            app.log.back().unwrap(),
            "bolt review  DONE · VERDICT: APPROVE",
            "{:?}",
            app.log
        );
        assert_eq!(app.crew[1].review.author, Some(Provider::Claude));
        assert_eq!(app.crew[1].review.rounds, 1);
        assert!(app.live.as_ref().unwrap().preferred.is_none());
        let argv = fs::read_to_string(&argv_log).unwrap();
        let runs: Vec<&str> = argv
            .lines()
            .filter(|run| !run.starts_with("app-server"))
            .collect();
        assert_eq!(runs.len(), 2, "{argv}");
        assert!(runs[0].starts_with("--print"), "{argv}");
        assert!(
            runs[0].ends_with("--permission-mode acceptEdits --"),
            "{argv}"
        );
        assert!(runs[1].ends_with("--sandbox read-only -"), "{argv}");
        assert!(!argv.contains("dangerously"));
        app.crew[1].review.rounds = review::MAX_REVIEW_ROUNDS;
        send(&mut app, "/review bolt edge cases");
        assert!(
            app.notice.starts_with("Review cap reached"),
            "{}",
            app.notice
        );
        assert!(!app.mission_running());
        send(&mut app, "/review bolt --next-revision edge cases");
        assert_eq!(
            (app.crew[1].review.revision, app.crew[1].review.rounds),
            (1, 1)
        );
        wait_for(&mut app, |app| !app.mission_running());
        let _ = fs::remove_file(&argv_log);
    }

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
    const SIDE_AND_GOAL_PROVIDER: &str = r#"
if test "$1" = auth; then
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
prompt=$(cat)
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$1"; }
case "$*" in *"--permission-mode plan"*)
    case "$prompt" in
      *"Side question (/btw)"*"MAIN-HISTORY-MARKER"*"Question:"*"Where are we?") say "Side answer: still on step 2"; exit ;;
    esac
    exit 5 ;;
esac
operator=${prompt##*Operator: }
case "$operator" in
  *MAIN-HISTORY-MARKER*)
    while ! test -f release; do sleep 0.02; done
    say "Main finished" ;;
  *"Pinned goal (turn 1 of at most 10): ship the release"*) say "Turn one progress" ;;
  *"Pinned goal (turn 2 of at most 10): ship the release"*) say "Shipped"; say "GOAL_MET" ;;
  *"endless polish"*)
    printf 'turn\n' >> turns
    say "Still polishing" ;;
  *) exit 6 ;;
esac
"#;

    #[cfg(unix)]
    #[test]
    fn side_chat_answers_without_interrupting_the_running_mission() {
        use auth::Provider;
        use usage::tests::Fixture;

        let fixture = Fixture::new("side-chat", SIDE_AND_GOAL_PROVIDER);
        let workspace = fixture.root.join("work");
        fs::create_dir(&workspace).unwrap();
        let profiles = vec![fixture.profile(Provider::Claude, "alpha")];
        let mut app = App::from_live(test_live(workspace.clone(), profiles, None));
        send(&mut app, "Long task MAIN-HISTORY-MARKER");
        wait_for(&mut app, |app| app.crew[0].status == Status::Running);
        send(&mut app, "/btw Where are we?");
        assert!(app.input.is_empty());
        wait_for(&mut app, |app| app.side_answers.len() >= 2);
        assert!(app.mission_running());
        assert_eq!(app.crew[0].status, Status::Running);
        let feed: Vec<String> = app
            .feed_entries()
            .into_iter()
            .map(|(_, text)| text.to_owned())
            .collect();
        assert!(
            feed.contains(&"BTW  Q: Where are we?".to_owned()),
            "{feed:?}"
        );
        assert!(feed.contains(&"BTW  Side answer: still on step 2".to_owned()));
        assert!(!app.log.iter().any(|line| line.starts_with("BTW")));
        fs::write(workspace.join("release"), "").unwrap();
        wait_for(&mut app, |app| app.crew[0].status == Status::Done);
        assert!(app.log.contains(&"Commander  Main finished".to_owned()));
        assert!(!app.log.iter().any(|line| line.contains("Side answer")));
    }

    #[cfg(unix)]
    #[test]
    fn snoozed_mission_shows_state_collapses_feed_and_allows_quit() {
        use auth::Provider;
        use usage::tests::Fixture;

        let fixture = Fixture::new("snooze-view", SIDE_AND_GOAL_PROVIDER);
        let workspace = fixture.root.join("work");
        fs::create_dir(&workspace).unwrap();
        let profiles = vec![
            fixture.profile(Provider::Claude, "alpha"),
            fixture.profile(Provider::Codex, "backup"),
        ];
        assert_eq!(
            handoff_link(&profiles[0], &profiles[1]),
            "Limit: claude/alpha → codex/backup (cross-provider fallback)"
        );
        let mut app = App::from_live(test_live(workspace.clone(), profiles, None));
        send(&mut app, "Long task MAIN-HISTORY-MARKER");
        wait_for(&mut app, |app| app.crew[0].status == Status::Running);
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        app.key(quit);
        assert!(!app.quit);
        app.handle_event(
            COMMANDER,
            mission::Event::Snoozed {
                profile: 0,
                until: 4_102_444_800,
            },
        );
        assert_eq!(app.status(0), "SNOOZED");
        assert!(app
            .notice
            .starts_with("Snoozed: claude/alpha limit resets at"));
        let expanded = app.feed_entries().len();
        assert!(expanded > 1);
        app.settings.snooze_limited_missions = true;
        let collapsed = app.feed_entries();
        assert_eq!(collapsed.len(), 1);
        assert!(collapsed[0].1.starts_with("SNOOZED  Snoozed: claude/alpha"));
        send(&mut app, "Another task");
        assert!(app.notice.contains("snoozed until its limit resets"));
        app.handle_event(COMMANDER, mission::Event::Continuing { from: 0, to: 0 });
        assert_eq!(app.status(0), "RUNNING");
        assert_eq!(app.notice, "Reset passed: resumed on claude/alpha");
        assert!(app.feed_entries().len() > 1);
        app.handle_event(
            COMMANDER,
            mission::Event::Snoozed {
                profile: 0,
                until: 4_102_444_800,
            },
        );
        app.key(quit);
        assert!(app.quit);
    }

    #[cfg(unix)]
    #[test]
    fn goal_loop_runs_until_goal_met_stops_at_cap_and_cancels() {
        use auth::Provider;
        use usage::tests::Fixture;

        let fixture = Fixture::new("goal-loop", SIDE_AND_GOAL_PROVIDER);
        let workspace = fixture.root.join("work");
        fs::create_dir(&workspace).unwrap();
        let profiles = vec![fixture.profile(Provider::Claude, "alpha")];
        let mut app = App::from_live(test_live(workspace.clone(), profiles, None));
        send(&mut app, "/goal ship the release");
        assert_eq!(
            app.goal.as_ref().map(goal::Goal::status).as_deref(),
            Some("turn 1/10 · ship the release")
        );
        assert!(app
            .goal_lines(COMMANDER)
            .iter()
            .any(|line| line.to_string() == "turn 1/10 · ship the release"));
        wait_for(&mut app, |app| app.goal.is_none() && !app.mission_running());
        assert!(
            app.log.contains(
                &"SYSTEM  Goal met on Commander: turn 2/10 · ship the release".to_owned()
            ),
            "{:?}",
            app.log
        );

        app.settings.goal_turn_cap = 2;
        app.input = "half-typed".into();
        app.start_goal("endless polish");
        wait_for(&mut app, |app| app.goal.is_none() && !app.mission_running());
        assert_eq!(app.input, "half-typed");
        assert_eq!(
            fs::read_to_string(workspace.join("turns")).unwrap(),
            "turn\nturn\n"
        );
        assert!(app
            .notice
            .starts_with("Goal stopped at the 2-turn safety cap on Commander"));

        app.input.clear();
        send(&mut app, "/goal endless polish");
        send(&mut app, "/goal cancel");
        assert!(app.goal.is_none());
        assert!(app
            .notice
            .starts_with("Goal cancelled on Commander after 1 of 2"));
        wait_for(&mut app, |app| !app.mission_running());
        assert_eq!(
            fs::read_to_string(workspace.join("turns")).unwrap(),
            "turn\nturn\nturn\n"
        );
        send(&mut app, "/goal cancel");
        assert_eq!(app.notice, "No goal is pinned.");

        app.crew[0].paused = true;
        send(&mut app, "/goal endless polish");
        assert!(app.goal.is_none());
        assert_eq!(
            app.notice,
            "Goal stopped on Commander: Resume Commander before sending a message."
        );
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
    fn update_button_appears_bottom_right_from_an_injected_receiver() {
        for (width, height) in SIZES {
            let mut app = App::default();
            let (sender, receiver) = std::sync::mpsc::channel();
            app.watch_updates(Some(receiver));
            app.poll_update();
            assert!(app.update_available.is_none());
            let buffer = screen(&mut app, width, height);
            assert!(!row(&buffer, height - 1).contains("Update"));
            assert_eq!(app.update_area.width, 0);
            let crew_before = app.crew_area;
            sender.send("9.9.9".to_owned()).unwrap();
            app.poll_update();
            assert_eq!(app.update_available.as_deref(), Some("9.9.9"));
            let buffer = screen(&mut app, width, height);
            let bar = row(&buffer, height - 1);
            assert!(bar.ends_with(&update_label("9.9.9", width)), "{bar}");
            assert!(!row(&buffer, 1).contains("9.9.9"));
            assert_eq!(app.crew_area, crew_before);
            assert_eq!(app.update_area.right(), width);
            assert!(region(&buffer, app.update_area).contains("Update"));
            app.key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
            let keyboard = app.notice.clone();
            app.notice.clear();
            let button = app.update_area;
            click_center(&mut app, button);
            assert_eq!(app.notice, keyboard);
            assert_eq!(app.notice, panel::PREVIEW_BLOCKER);
            assert!(app.update_available.is_some() && !app.quit);
            drop(sender);
            app.poll_update();
            assert!(app.update_receiver.is_none());
        }
        let mut app = App::default();
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        app.watch_updates(Some(receiver));
        drop(sender);
        app.poll_update();
        assert!(app.update_receiver.is_none() && app.update_available.is_none());
    }

    #[test]
    fn shortcut_text_is_clipped_before_the_right_side_buttons() {
        assert_eq!(clipped("abcdef", 6), "abcdef");
        assert_eq!(clipped("abcdef", 4), "abc…");
        assert_eq!(clipped("abcdef", 0), "…");
        assert_eq!(update_label("1.2.3", 80), " Update ");
        assert_eq!(update_label("1.2.3", 120), " Update v1.2.3 ");
        let bar = Rect::new(0, 23, 10, 1);
        assert_eq!(right_of(bar, 10, 4), Rect::new(6, 23, 4, 1));
        assert_eq!(right_of(bar, 3, 8), Rect::new(0, 23, 3, 1));
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
        send(&mut app, "/roster");
        app.selected = 1;
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
            assert!(whole.contains("Roster:"), "{width}x{height}\n{whole}");
            assert!(whole.contains("claude · alpha"));
            assert!(region(&buffer, app.input_area).contains("Message Commander"));
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            app.editing = true;
            app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            assert_eq!(app.input, "x");
            app.input.clear();
        }
    }

    #[cfg(unix)]
    const PARALLEL_PROVIDER: &str = r#"
if test "$1" = auth; then
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
root="${CLAUDE_CONFIG_DIR%/profiles/claude/*}"
say() { printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$1"; }
prompt=$(cat)
case "$prompt" in
  "You are Commander"*"Operator: Status?")
    say "Crew is busy" ;;
  "You are nova"*"Task: Build the λ parser")
    touch "$root/started-nova"
    while test ! -f "$root/started-atlas"; do sleep 0.01; done
    while test ! -f "$root/release-nova"; do sleep 0.01; done
    printf 'λ parser\n' > parser.txt
    say "Parser ready" ;;
  "You are nova"*"Task: Polish it")
    test -f parser.txt || exit 4
    say "Polished" ;;
  "You are atlas"*"Task: Write docs")
    touch "$root/started-atlas"
    while test ! -f "$root/started-nova"; do sleep 0.01; done
    say "Drafted\nNEEDS_APPROVAL: Publish the docs?" ;;
  "You are atlas"*"Task: Hang")
    touch "$root/hanging"
    while test ! -f "$root/release-hang"; do sleep 0.01; done
    touch "$root/hang-finished" ;;
  *) exit 9 ;;
esac
"#;

    #[cfg(unix)]
    fn parallel_app(name: &str) -> (usage::tests::Fixture, PathBuf, App) {
        let fixture = usage::tests::Fixture::new(name, PARALLEL_PROVIDER);
        let repository = fixture.root.join("work");
        specialists::tests::git_repository(&repository);
        branches::tests::configure_identity(&repository);
        let profiles = vec![fixture.profile(auth::Provider::Claude, "alpha")];
        let app = App::from_live(test_live(repository.clone(), profiles, None));
        (fixture, repository, app)
    }

    #[cfg(unix)]
    fn member<'a>(app: &'a App, name: &str) -> &'a Member {
        &app.crew[specialists::find(&app.crew, name).unwrap()]
    }

    #[cfg(unix)]
    fn row_order(text: &str, names: &[&str]) -> Vec<usize> {
        names.iter().map(|name| text.find(name).unwrap()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn specialists_run_in_parallel_on_named_branches_with_an_inbox_roster() {
        let (fixture, repository, mut app) = parallel_app("parallel");
        send(&mut app, "/spawn nova builder Build the λ parser");
        send(&mut app, "/spawn atlas writer Write docs");
        wait_for(&mut app, |app| {
            member(app, "atlas").status == Status::NeedsApproval
        });
        assert_eq!(member(&app, "nova").status, Status::Running);
        assert!(app.mission_running());
        assert!(app
            .log
            .contains(&"atlas  NEEDS_APPROVAL · Publish the docs?".to_owned()));
        assert_eq!(
            member(&app, "nova").branch().unwrap().name,
            "ac/nova-build-the-parser"
        );
        assert_eq!(
            member(&app, "atlas").branch().unwrap().name,
            "ac/atlas-write-docs"
        );

        app.selected = 0;
        send(&mut app, "Status?");
        wait_for(&mut app, |app| {
            app.log.contains(&"Commander  Crew is busy".to_owned()) && app.status(0) == "DONE"
        });
        assert_eq!(member(&app, "nova").status, Status::Running);
        send(&mut app, "/ask nova Polish it");
        assert_eq!(
            app.notice,
            "nova is busy; the task is queued for its workspace."
        );

        for (width, height) in SIZES {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            let order = row_order(&crew, &["Commander", "atlas", "nova"]);
            assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{crew}");
            assert!(crew.contains("needs appr"), "{width}x{height}\n{crew}");
            if width >= 105 {
                let whole = region(&buffer, buffer.area);
                assert!(
                    whole.contains("01  RUNNING") && whole.contains("01  NEED ATTENTION"),
                    "{whole}"
                );
            }
        }
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.crew[app.selected].name, "atlas");
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.crew[app.selected].name, "nova");

        fs::write(fixture.root.join("release-nova"), "").unwrap();
        wait_for(&mut app, |app| {
            !app.mission_running() && member(app, "nova").mission == "Polish it"
        });
        assert_eq!(member(&app, "nova").status, Status::Done, "{}", app.notice);
        let feed: Vec<&str> = app.log.iter().map(String::as_str).collect();
        let parser = feed
            .iter()
            .position(|line| *line == "nova  DONE · Parser ready");
        let polished = feed
            .iter()
            .position(|line| *line == "nova  DONE · Polished");
        assert!(parser.unwrap() < polished.unwrap(), "{feed:?}");
        let nova = member(&app, "nova").clone();
        assert_eq!(nova.changes.as_ref().unwrap().label(), "+1 −0 · 1 file");
        assert!(!repository.join("parser.txt").exists());

        app.selected = specialists::find(&app.crew, "nova").unwrap();
        for (width, height) in SIZES {
            let buffer = screen(&mut app, width, height);
            let whole = region(&buffer, buffer.area);
            assert!(
                whole.contains("BUILDER  /  02  ·  +1 −0"),
                "{width}x{height}\n{whole}"
            );
            if width >= 105 {
                assert!(whole.contains("BRANCH") && whole.contains("ac/nova-build-the-parser →"));
            }
        }
        send(&mut app, "/diff");
        let report = app.log.back().unwrap().clone();
        assert!(
            report.starts_with("SYSTEM  Diff nova · ac/nova-build-the-parser vs ")
                && report.ends_with("+1 −0 · 1 file · parser.txt +1 −0"),
            "{report}"
        );
        send(&mut app, "/diff ghost");
        assert_eq!(app.notice, "No specialist named ghost.");
        app.selected = 0;
        send(&mut app, "/diff");
        assert!(app.notice.starts_with("Select a specialist or name one."));
    }

    #[cfg(unix)]
    #[test]
    fn settle_drawer_and_pr_controls_work_from_keyboard_mouse_and_commands() {
        let (fixture, repository, mut app) = parallel_app("settle-pr");
        send(&mut app, "/spawn nova builder");
        send(&mut app, "/spawn atlas writer");
        let nova = specialists::find(&app.crew, "nova").unwrap();
        fs::write(
            app.crew[nova]
                .workspace
                .as_ref()
                .unwrap()
                .path
                .join("ünï.txt"),
            "λ\n",
        )
        .unwrap();
        app.selected = nova;
        send(&mut app, "/settle nova");
        assert!(app.crew[nova].settled);
        assert_eq!(app.selected, 0);
        for (index, (width, height)) in SIZES.into_iter().enumerate() {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            assert!(
                crew.contains("▸ SETTLED (1)") && !crew.contains("nova"),
                "{crew}"
            );
            if index == 0 {
                app.key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
            } else {
                let header = 2 * app.crew_row_height;
                app.click(app.crew_area.x + 1, app.crew_area.y + header + 1);
            }
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            let order = row_order(&crew, &["atlas", "▾ SETTLED (1)"]);
            assert!(order[0] < order[1], "{crew}");
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(app.selected, nova);
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            let order = row_order(&crew, &["SETTLED", "nova"]);
            assert!(order[0] < order[1], "{width}x{height}\n{crew}");
            send(&mut app, "/settled");
            assert!(!app.show_settled);
            assert_eq!(app.selected, 0);
        }
        assert!(
            specialists::Store::new(fixture.root.join("profiles"))
                .load_roster()
                .unwrap()[0]
                .settled
        );

        send(&mut app, "/pr nova");
        assert!(
            app.notice.contains("Confirm with --commit"),
            "{}",
            app.notice
        );
        send(&mut app, "/pr nova --commit");
        assert!(app.notice.contains("No Git remote"), "{}", app.notice);
        let remote = fixture.root.join("remote.git");
        branches::git(
            &fixture.root,
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        )
        .unwrap();
        branches::git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .unwrap();
        app.live.as_mut().unwrap().tools.gh = fixture.root.join("no-gh").into_os_string();
        send(&mut app, "/pr nova");
        assert!(
            app.notice.starts_with(
                "nova: pushed ac/nova-builder to origin · PR not opened: GitHub CLI `gh`"
            ),
            "{}",
            app.notice
        );
        assert!(app
            .log
            .back()
            .unwrap()
            .starts_with("SYSTEM  PR nova: pushed"));
        assert_eq!(
            app.crew[nova].changes.as_ref().unwrap().label(),
            "+1 −0 · 1 file"
        );
        send(&mut app, "/unsettle nova");
        assert!(!app.crew[nova].settled);
        send(&mut app, "/settle commander");
        assert!(app.notice.contains("cannot be settled"));
        send(&mut app, "/remove nova");
        assert_eq!(app.log.back().unwrap(), "SYSTEM  Removed nova.");
        assert!(!branches::git(&remote, &["rev-parse", "ac/nova-builder"])
            .unwrap()
            .is_empty());
        app.crew[1].changes = None;
        app.selected = 1;
        let buffer = screen(&mut app, 120, 40);
        assert!(region(&buffer, buffer.area).contains("diff unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_app_cancels_every_running_specialist() {
        let (fixture, _repository, mut app) = parallel_app("parallel-drop");
        send(&mut app, "/spawn atlas writer Hang");
        wait_for(&mut app, |_| fixture.root.join("hanging").exists());
        app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit);
        assert_eq!(app.notice, "Wait for the running mission before quitting.");
        send(&mut app, "/settle atlas");
        assert!(app.notice.contains("is running"));
        send(&mut app, "/remove atlas");
        assert!(app.notice.contains("is running"));
        app.handle_event(
            "atlas",
            mission::Event::Snoozed {
                profile: 0,
                until: 4_102_444_800,
            },
        );
        assert_eq!(member(&app, "atlas").status, Status::Snoozed);
        assert!(app.is_snoozed("atlas") && !app.is_snoozed(COMMANDER));
        app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.quit);
        drop(app);
        std::thread::sleep(std::time::Duration::from_millis(200));
        fs::write(fixture.root.join("release-hang"), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!fixture.root.join("hang-finished").exists());
    }

    #[cfg(unix)]
    #[test]
    fn parallel_launches_are_capped_and_never_share_a_workspace() {
        let fixture = usage::tests::Fixture::new("parallel-cap", "exit 1\n");
        let mut live = test_live(fixture.root.clone(), Vec::new(), None);
        let idle = || {
            mission::Mission::launch(
                Vec::new(),
                mission::Route::single("x", None),
                fixture.root.clone(),
            )
        };
        live.runs.push(Run::new(COMMANDER, idle(), false));
        for name in ["s1", "s2", "s3", "s4"] {
            live.runs.push(Run::new(name, idle(), false));
        }
        let delegation = |name: &str, task: &str| Delegation {
            name: name.into(),
            task: task.into(),
            target: None,
            review: false,
        };
        live.queue
            .extend([delegation("s1", "again"), delegation("s5", "new")]);
        assert_eq!(live.specialist_runs(), MAX_PARALLEL_SPECIALISTS);
        assert_eq!(live.next_ready(), None);
        live.runs.remove(4);
        assert_eq!(live.next_ready(), Some(delegation("s5", "new")));
        assert_eq!(live.next_ready(), None);
        assert!(live.running("S1") && !live.running("s4"));
        assert_eq!(live.queue.len(), 1);
    }
}
