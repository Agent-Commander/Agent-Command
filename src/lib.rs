pub mod auth;
mod branches;
pub mod bridge;
mod catalog;
mod checkpoint;
mod cli;
pub mod clipboard;
mod connect;
mod control;
mod daemon;
pub mod editor;
pub mod github;
mod goal;
mod http;
mod install;
pub mod ledger;
pub mod markdown;
mod mcp;
pub mod mission;
mod native;
pub mod onboard;
pub mod palette;
mod panel;
pub mod permissions;
pub mod prodgate;
mod review;
pub mod routing;
pub mod settings;
mod side;
pub mod soul;
mod specialists;
mod targeting;
pub mod token_burn;
mod triage;
pub mod tui;
pub mod update;
pub mod usage;
pub use cli::run_cli;
pub use specialists::{
    format_goal_turn, format_help, help_entries, resolve_member_target, resolve_target,
    split_goal_prompt, CommandHelp, Entry, GoalTurn, Member, Status, TargetCandidate, TargetMatch,
    COMMANDER,
};

use std::{
    collections::{HashMap, VecDeque},
    env, fs, io,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender, TryRecvError},
    time::{Instant, SystemTime},
};

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, style::Color, Frame};
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

fn deck_palette(path: &Path) -> token_burn::TokenBurnPalette {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| token_burn::TokenBurnPalette::from_theme_toml(&text).ok())
        .unwrap_or_default()
}

fn framed(mut request: routing::Request, frame: impl Fn(&str) -> String) -> routing::Request {
    request.units = request.units.iter().map(|unit| frame(unit)).collect();
    request
}

struct Snooze {
    thread: String,
    banner: String,
}

impl Snooze {
    fn new(thread: &str, label: &str) -> Self {
        Self {
            thread: thread.into(),
            banner: format!("SNOOZED  {label} Feed collapsed until the mission resumes."),
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

pub(crate) struct Live {
    pub(crate) workspace: PathBuf,
    pub(crate) profiles: Vec<auth::Profile>,
    pub(crate) profile_root: Option<PathBuf>,
    pub(crate) preferred: Option<usize>,
    pins: native::Pins,
    last_routed: Option<usize>,
    pub(crate) profile_area: Rect,
    usage: Option<usage::Usage>,
    pub(crate) usage_checked_at: Option<u64>,
    usage_worker: Option<std::thread::JoinHandle<()>>,
    running_profile: Option<usize>,
    pub(crate) pending: Option<(usize, checkpoint::Claim)>,
    pub(crate) abandon_area: Rect,
    pub(crate) runs: Vec<Run>,
    pub(crate) store: Option<specialists::Store>,
    pub(crate) queue: VecDeque<Delegation>,
    tools: branches::Tools,
    entry: Option<(String, &'static str)>,
    routes: Vec<ProfileRoute>,
    pub(crate) allowances: Vec<usage::Allowance>,
    pub(crate) signed_in: Vec<bool>,
    pub(crate) profile_detections: HashMap<String, auth::AuthDetection>,
    pub(crate) bridge: Option<bridge::LoopbackBridge>,
    pub(crate) commander_session_id: Option<String>,
    pub(crate) commander_session_profile: Option<String>,
    pub(crate) soul_store: Option<soul::SoulStore>,
    pub(crate) recent_shell_context: VecDeque<String>,
    pub(crate) handoff_turns: Vec<soul::HandoffTurn>,
    pub(crate) pending_prod_gates: HashMap<String, String>,
    pub(crate) ask_user: Option<bridge::AskUserBroker>,
}

const MAX_PARALLEL_SPECIALISTS: usize = 4;
const EVENTS_PER_POLL: usize = 64;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum RunPhase {
    #[default]
    Working,
    Thinking,
    Tool {
        name: String,
        summary: String,
    },
    Streaming,
}

pub(crate) struct Run {
    pub(crate) name: String,
    pub(crate) mission: mission::Mission,
    reply: String,
    continuation: Option<String>,
    reviewing: bool,
    pub(crate) started_at: Instant,
    pub(crate) phase: RunPhase,
    pub(crate) thinking_started_at: Option<Instant>,
    pub(crate) tool_started_at: HashMap<String, Instant>,
    pub(crate) saw_normalized_text: bool,
    pub(crate) saw_session_not_found: bool,
}

impl Run {
    fn new(name: &str, mission: mission::Mission, reviewing: bool) -> Self {
        Self {
            name: name.into(),
            mission,
            reply: String::new(),
            continuation: None,
            reviewing,
            started_at: Instant::now(),
            phase: RunPhase::Working,
            thinking_started_at: None,
            tool_started_at: HashMap::new(),
            saw_normalized_text: false,
            saw_session_not_found: false,
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

    fn drain_normalized(&self) -> Vec<mission::NormalizedEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.mission.normalized.try_recv() {
            events.push(event);
        }
        events
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
            profile_detections: HashMap::new(),
            bridge: None,
            commander_session_id: None,
            commander_session_profile: None,
            soul_store: soul::SoulStore::from_env(),
            recent_shell_context: VecDeque::new(),
            handoff_turns: Vec::new(),
            pending_prod_gates: HashMap::new(),
            ask_user: None,
        }
    }

    pub(crate) fn clear_commander_session(&mut self) {
        self.commander_session_id = None;
        self.commander_session_profile = None;
    }

    pub(crate) fn refresh_profile_detections(&mut self) {
        self.profile_detections = self
            .profiles
            .iter()
            .map(|profile| (profile.to_string(), profile.detect_passive()))
            .collect();
    }

    pub(crate) fn refresh_usage_now(&mut self) {
        self.usage_checked_at = None;
        self.refresh_usage();
    }

    fn busy(&self) -> bool {
        !self.runs.is_empty()
    }

    pub(crate) fn running(&self, name: &str) -> bool {
        self.run_position(name).is_some()
    }

    pub(crate) fn run_position(&self, name: &str) -> Option<usize> {
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

    pub(crate) fn reset_profiles(&mut self, profiles: Vec<auth::Profile>) {
        self.profiles = profiles;
        self.preferred = None;
        self.last_routed = None;
        self.running_profile = None;
        self.clear_commander_session();
        self.usage = None;
        self.usage_checked_at = None;
        self.load_override();
        self.pending = checkpoint::Claim::pending(&self.profiles, &self.workspace)
            .ok()
            .flatten();
        let now = usage::now();
        usage::Usage::reconcile_peer_accounts(&self.profiles, now);
        self.refresh_profile_detections();
        self.refresh_routes();
        self.read_allowances(now);
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

    pub(crate) fn displayed_profile(&self) -> usize {
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

    pub(crate) fn pin_label(&self, index: usize) -> Option<String> {
        self.profiles
            .get(index)
            .and_then(|profile| self.pins.get(&profile.to_string()))
            .map(native::Pin::label)
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

    pub(crate) fn refresh_usage(&mut self) {
        let now = usage::now();
        let now_sec = now.elapsed / 1_000_000_000;
        let worker_finished = self
            .usage_worker
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished);
        if worker_finished {
            if let Some(worker) = self.usage_worker.take() {
                let _ = worker.join();
            }
        } else if self.usage_checked_at == Some(now_sec) {
            return;
        }
        self.usage_checked_at = Some(now_sec);
        self.read_allowances(now);
        let displayed = self.displayed_profile();
        self.usage = self
            .profiles
            .get(displayed)
            .and_then(|profile| usage::Usage::read(profile).ok());
        if self.busy() || self.usage_worker.is_some() {
            return;
        }
        let due: Vec<_> = self
            .profiles
            .iter()
            .filter(|profile| {
                matches!(
                    profile.provider,
                    auth::Provider::Codex | auth::Provider::Gemini
                ) && usage::Usage::read(profile).is_ok_and(|usage| usage.poll_due(now))
            })
            .cloned()
            .collect();
        if !due.is_empty() {
            let all_profiles = self.profiles.clone();
            self.usage_worker = Some(std::thread::spawn(move || {
                for profile in due {
                    let tick = usage::now();
                    let refreshed = match profile.provider {
                        auth::Provider::Codex => usage::refresh_codex(&profile, tick).is_ok(),
                        auth::Provider::Gemini => {
                            usage::refresh_gemini(&profile, tick).is_ok_and(|a| a.is_some())
                        }
                        auth::Provider::Claude => false,
                    };
                    if refreshed {
                        usage::Usage::sync_peer_accounts(&all_profiles, &profile, tick);
                    }
                }
            }));
        }
    }
}

pub struct App {
    pub(crate) live: Option<Live>,
    pub selected: usize,
    pub crew: Vec<Member>,
    pub input: String,
    pub editing: bool,
    pub quit: bool,
    pub log: VecDeque<String>,
    pub log_times: VecDeque<String>,
    pub theme: Theme,
    pub ui_palette: markdown::UiPalette,
    pub notice: String,
    theme_stamp: Option<SystemTime>,
    pub crew_area: Rect,
    pub input_area: Rect,
    pub new_area: Rect,
    pub pause_area: Rect,
    pub send_area: Rect,
    pub show_settled: bool,
    touch_clock: u64,
    pub deck_palette: token_burn::TokenBurnPalette,
    pub(crate) update_receiver: Option<Receiver<String>>,
    pub update_available: Option<String>,
    pub update_state: update::UpdateState,
    pub(crate) manual_update_check: bool,
    pub(crate) last_update_check: Option<Instant>,
    pub(crate) update_check_in_flight: bool,
    pub update_area: Rect,
    exit_message: Option<String>,
    pub(crate) relaunch_executable: Option<PathBuf>,
    pub settings_area: Rect,
    pub palette_area: Rect,
    pub profile_area: Rect,
    pub transcript_area: Rect,
    pub full_redraw: bool,
    pub(crate) panel: Option<panel::Panel>,
    panel_hits: Vec<(panel::Action, Rect)>,
    host: Option<Box<dyn panel::Host>>,
    pub(crate) settings: settings::Settings,
    pub(crate) launch: settings::LaunchConfig,
    goal: Option<goal::Goal>,
    side_chats: Vec<side::SideChat>,
    snoozes: Vec<Snooze>,
    pub composer: editor::Composer,
    pub autocomplete: editor::AutocompletePopup,
    pub palette: palette::CommandPalette,
    pub diff_modal: tui::diff_modal::DiffModal,
    pub transcript: markdown::Transcript,
    pub scroll: markdown::ScrollState,
    pub(crate) specialist_transcripts: HashMap<String, markdown::Transcript>,
    pub(crate) specialist_scrolls: HashMap<String, markdown::ScrollState>,
    pub rail_open: bool,
    pub rail_focused: bool,
    pub show_welcome: bool,
    pub github_status: github::GitHubAuthStatus,
    pub auth_detections: Vec<auth::AuthDetection>,
    pub git_snapshot: tui::events::GitSnapshot,
    pub turn_changes: Vec<tui::events::ChangedFile>,
    pub(crate) rail_hits: Vec<(Rect, tui::render::RailHit)>,
    pub(crate) focus_banner_hits: Vec<(Rect, tui::render::FocusBannerHit)>,
    pub(crate) welcome_hits: Vec<(Rect, tui::render::WelcomeHit)>,
    pub(crate) bg_tx: Option<Sender<tui::events::AppEvent>>,
    pub(crate) bg_rx: Option<Receiver<tui::events::AppEvent>>,
    pub(crate) esc_armed_at: Option<Instant>,
    pub(crate) ctrl_c_armed_at: Option<Instant>,
    pub(crate) leader_armed_at: Option<Instant>,
    pub(crate) external_editor_request: Option<String>,
    pub view_tab: tui::tabs::ViewTab,
    pub left_panel: tui::tabs::LeftPanelMode,
    pub left_focused: bool,
    pub(crate) left_hits: Vec<(Rect, tui::tabs::TabHit)>,
    pub(crate) last_frame_width: u16,
    pub(crate) ledger: ledger::Ledger,
    pub(crate) ledger_path: Option<PathBuf>,
    pub ledger_filter: ledger::LedgerFilter,
    pub ledger_selected: usize,
    pub inbox_selected: usize,
    pub(crate) inbox_dismissed: std::collections::HashSet<String>,
    pub(crate) inbox_wakes: Vec<tui::tabs::WakeNote>,
}

impl Default for App {
    fn default() -> Self {
        let mut transcript = markdown::Transcript::new();
        transcript.push(markdown::TranscriptEntry::SystemNotice {
            level: markdown::NoticeLevel::Info,
            text: "Commander online. Ask anything, /help for commands, Ctrl+P for palette.".into(),
        });
        Self {
            live: None,
            selected: 0,
            crew: vec![Member::commander()],
            input: String::new(),
            editing: true,
            quit: false,
            log: VecDeque::from([
                "SYSTEM  Commander online. Message Commander, or /spawn NAME ROLE [TASK]."
                    .to_owned(),
            ]),
            log_times: VecDeque::from([chrono::Local::now().format("%H:%M:%S").to_string()]),
            theme: Theme::default(),
            ui_palette: markdown::UiPalette::dark_deck(),
            notice: String::new(),
            theme_stamp: None,
            crew_area: Rect::default(),
            input_area: Rect::default(),
            new_area: Rect::default(),
            pause_area: Rect::default(),
            send_area: Rect::default(),
            show_settled: false,
            touch_clock: 0,
            deck_palette: token_burn::TokenBurnPalette::default(),
            update_receiver: None,
            update_available: None,
            update_state: update::UpdateState::Idle,
            manual_update_check: false,
            last_update_check: None,
            update_check_in_flight: false,
            update_area: Rect::default(),
            exit_message: None,
            relaunch_executable: None,
            settings_area: Rect::default(),
            palette_area: Rect::default(),
            profile_area: Rect::default(),
            transcript_area: Rect::default(),
            full_redraw: false,
            panel: None,
            panel_hits: Vec::new(),
            host: None,
            settings: settings::Settings::default(),
            launch: settings::LaunchConfig::default(),
            goal: None,
            side_chats: Vec::new(),
            snoozes: Vec::new(),
            composer: editor::Composer::new(),
            autocomplete: editor::AutocompletePopup::new(),
            palette: palette::CommandPalette::default(),
            diff_modal: tui::diff_modal::DiffModal::new(),
            transcript,
            scroll: markdown::ScrollState::new(),
            specialist_transcripts: HashMap::new(),
            specialist_scrolls: HashMap::new(),
            rail_open: true,
            rail_focused: false,
            show_welcome: false,
            github_status: github::GitHubAuthStatus::default(),
            auth_detections: Vec::new(),
            git_snapshot: tui::events::GitSnapshot::default(),
            turn_changes: Vec::new(),
            rail_hits: Vec::new(),
            focus_banner_hits: Vec::new(),
            welcome_hits: Vec::new(),
            bg_tx: None,
            bg_rx: None,
            esc_armed_at: None,
            ctrl_c_armed_at: None,
            leader_armed_at: None,
            external_editor_request: None,
            view_tab: tui::tabs::ViewTab::Chat,
            left_panel: tui::tabs::LeftPanelMode::Auto,
            left_focused: false,
            left_hits: Vec::new(),
            last_frame_width: 0,
            ledger: ledger::Ledger::default(),
            ledger_path: None,
            ledger_filter: ledger::LedgerFilter::All,
            ledger_selected: 0,
            inbox_selected: 0,
            inbox_dismissed: std::collections::HashSet::new(),
            inbox_wakes: Vec::new(),
        }
    }
}

impl App {
    pub fn live_fast(workspace: PathBuf) -> Self {
        let root = auth::root().ok();
        let mut live = Live::new(workspace, Vec::new(), root);
        let _ = live.load_pins();
        let _ = live.load_override();
        Self::from_live(live)
    }

    pub(crate) fn attach_fast(
        &mut self,
        host: Box<dyn panel::Host>,
        launch: settings::LaunchConfig,
    ) {
        if let Some(path) = &launch.settings_path {
            match settings::Settings::load(path) {
                Ok(loaded) => self.settings = loaded,
                Err(error) => {
                    self.notice = format!("Settings unreadable: {error}. Using defaults.")
                }
            }
        }
        self.crew[0].permission = self.settings.commander_permission;
        if let Some(live) = &mut self.live {
            live.soul_store = self
                .settings
                .effective_soul_path()
                .map(soul::SoulStore::new)
                .or_else(soul::SoulStore::from_env);
        }
        self.launch = launch;
        self.host = Some(host);
        self.editing = true;
    }

    pub fn live(workspace: PathBuf) -> Self {
        let mut notice = String::new();
        let root = auth::root();
        let profiles = root
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
            .and_then(|root| auth::all_profiles(root))
            .unwrap_or_else(|error| {
                notice = format!("Cannot load profiles: {error}");
                Vec::new()
            });
        let pending = checkpoint::Claim::pending(&profiles, &workspace).unwrap_or_else(|error| {
            notice = format!("Cannot read continuation state: {error}");
            None
        });
        usage::Usage::reconcile_peer_accounts(&profiles, usage::now());
        let mut live = Live::new(workspace, profiles, root.ok());
        live.pending = pending;
        live.refresh_profile_detections();
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

    #[cfg(test)]
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
        if let Some(live) = &mut self.live {
            live.soul_store = self
                .settings
                .effective_soul_path()
                .map(soul::SoulStore::new)
                .or_else(soul::SoulStore::from_env);
        }
        self.launch = launch;
        self.host = Some(host);
        self.refresh_sign_in();
        if self.no_provider_connected() {
            self.show_welcome = true;
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
        app.transcript.clear();
        if let Some(store) = &live.store {
            let legacy_entries = match store.load_commander_for_workspace(&live.workspace) {
                Ok(entries) => {
                    for entry in &entries {
                        app.log.push_back(entry.text.clone());
                        app.log_times.push_back(entry.at.clone());
                    }
                    entries
                }
                Err(error) => {
                    app.notice = format!("Cannot read Commander transcript: {error}");
                    Vec::new()
                }
            };
            match store.load_transcript_for_workspace(&live.workspace) {
                Ok(Some(structured)) => {
                    app.transcript.entries = structured;
                }
                Ok(None) => {
                    for entry in legacy_entries {
                        app.push_transcript_log_entry(COMMANDER, &entry.text, Some(entry.at));
                    }
                }
                Err(error) => {
                    app.notice = format!("Cannot read Commander transcript: {error}");
                    for entry in legacy_entries {
                        app.push_transcript_log_entry(COMMANDER, &entry.text, Some(entry.at));
                    }
                }
            }
            match store.load_roster() {
                Ok(members) => {
                    for member in members {
                        let name = member.name.clone();
                        for entry in &member.history {
                            app.push_transcript_log_entry(
                                &name,
                                &entry.text,
                                Some(entry.at.clone()),
                            );
                        }
                        app.crew.push(member);
                    }
                }
                Err(error) => app.notice = format!("Cannot read specialist roster: {error}"),
            }
            app.ledger_path = store.ledger_path_for_workspace(&live.workspace).ok();
            if let Some(path) = &app.ledger_path {
                app.ledger = ledger::Ledger::load(path);
            }
        }
        live.refresh_routes();
        live.read_allowances(usage::now());
        app.live = Some(live);
        app
    }

    pub(crate) fn select_profile(&mut self) {
        let Some(live) = &mut self.live else { return };
        if live.busy() || live.profiles.is_empty() {
            return;
        }
        live.preferred = match live.preferred {
            None => Some(0),
            Some(index) if index + 1 < live.profiles.len() => Some(index + 1),
            Some(_) => None,
        };
        live.refresh_usage_now();
        live.refresh_routes();
        self.notice = match live.save_override() {
            Ok(()) => match live.preferred {
                Some(index) => format!("Override saved: {}", live.profiles[index]),
                None => "Auto routing: highest-ranked eligible profile".into(),
            },
            Err(error) => format!("Override not saved: {error}"),
        };
    }

    pub fn status(&self, index: usize) -> &'static str {
        self.crew[index].status.label()
    }

    pub fn poll_mission(&mut self) {
        self.refresh_usage_and_sync();
        self.poll_side();

        for _ in 0..EVENTS_PER_POLL {
            let events = self.next_events();
            if events.is_empty() {
                break;
            }
            for (name, event) in events {
                if matches!(
                    event,
                    mission::Event::Output(_) | mission::Event::Finished(_)
                ) {
                    self.drain_run_normalized(&name);
                }
                self.handle_event(&name, event);
            }
        }

        self.drain_all_normalized();
    }

    fn drain_run_normalized(&mut self, name: &str) {
        let batch = self
            .live
            .as_ref()
            .and_then(|live| {
                let pos = live.run_position(name)?;
                Some(live.runs[pos].drain_normalized())
            })
            .unwrap_or_default();
        for ev in batch {
            self.handle_normalized_event(name, ev);
        }
    }

    fn drain_all_normalized(&mut self) {
        let normalized_batch: Vec<(String, mission::NormalizedEvent)> = self
            .live
            .as_ref()
            .map(|live| {
                live.runs
                    .iter()
                    .flat_map(|run| {
                        let name = run.name.clone();
                        run.drain_normalized()
                            .into_iter()
                            .map(move |ev| (name.clone(), ev))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (name, ev) in normalized_batch {
            self.handle_normalized_event(&name, ev);
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
                live.refresh_usage_now();
                self.touch(index);
                if let Some(reason) = reason {
                    let concise = tui::app_ops::format_routing_notice(&reason);
                    self.notice = concise.clone();
                    self.push_log(format!("SYSTEM  {concise}"));
                }
            }
            mission::Event::Continuing { from, to } => {
                let link = handoff_link(&live.profiles[from], &live.profiles[to]);
                live.runs[position].continuation = Some(link.clone());
                if name == COMMANDER {
                    live.pending = None;
                }
                live.running_profile = Some(to);
                live.last_routed = Some(to);
                live.refresh_usage_now();
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
                let saw_norm = run.saw_normalized_text;
                let speaker = if run.reviewing {
                    format!("{name} review")
                } else {
                    name.to_owned()
                };
                if !saw_norm {
                    let clean = text.trim_end_matches('\r');
                    if !clean.trim().is_empty() {
                        self.append_transcript_assistant_chunk(name, &speaker, clean);
                    }
                }
                for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
                    self.record_log_only(index, format!("{speaker}  {line}"));
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
        live.refresh_usage_now();
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
        self.transcript.finish_assistant_stream();
        self.composer.set_running(false);
        self.crew[0].status = if result.is_ok() {
            Status::Done
        } else {
            Status::Error
        };
        let fallback_reply = (run.reply.trim().is_empty() && run.saw_normalized_text)
            .then(|| self.transcript.latest_assistant_text().map(str::to_owned))
            .flatten();
        let session_not_found = matches!(
            &result,
            Err(error)
                if run.saw_session_not_found
                    || tui::app_ops::is_session_not_found_error(&error.to_string())
        );
        if let Some(live) = &mut self.live {
            if session_not_found {
                live.clear_commander_session();
            }
            let seq = (live.handoff_turns.len() + 1) as u64;
            let reply_text = if !run.reply.trim().is_empty() {
                Some(run.reply.trim().to_owned())
            } else {
                fallback_reply
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
            };
            if let Some(content) = reply_text {
                live.handoff_turns.push(soul::HandoffTurn {
                    seq,
                    role: soul::TurnRole::Assistant,
                    content,
                    command_outcome: None,
                });
            }
            live.pending_prod_gates
                .retain(|_, run_name| run_name != COMMANDER);
            live.refresh_usage_now();
        }
        self.notice = match &result {
            Ok(()) => run.continuation.map_or_else(
                || format!("{COMMANDER} replied."),
                |link| format!("Done · {link}"),
            ),
            Err(error) => {
                self.mark_provider_stderr_failed(COMMANDER);
                let msg = format!("{COMMANDER}: {error}");
                self.transcript
                    .push(markdown::TranscriptEntry::SystemNotice {
                        level: markdown::NoticeLevel::Error,
                        text: msg.clone(),
                    });
                msg
            }
        };
        if result.is_ok() {
            self.apply_directives(&run.reply);
        }
        self.persist_commander_transcript();
        if let (Some(live), Some(tx)) = (&self.live, &self.bg_tx) {
            tui::events::spawn_git_refresh(live.workspace.clone(), tx.clone());
        }
        if let Some(queued) = self.composer.pop_next_queued() {
            self.message_commander(queued);
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
        let _ = goal.begin();
        let turn =
            specialists::format_goal_turn(&goal.criteria, goal.turns, goal.cap, goal::GOAL_MET);
        self.notice = format!("Goal pinned on {member}: {}", goal.status());
        self.push_log(format!(
            "SYSTEM  Goal pinned on {member}: {}",
            goal.status()
        ));
        self.ledger_record(ledger::TaskKind::Goal, &member, &goal.criteria);
        self.goal = Some(goal);
        self.send_goal_turn(&member, turn);
    }

    fn cancel_goal(&mut self) {
        self.notice = match self.goal.take() {
            Some(goal) => {
                self.ledger_finish_goal(&goal.member, ledger::TaskStatus::Cancelled);
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
            goal::Step::Continue(_) => {
                let status = goal.status();
                let turn = specialists::format_goal_turn(
                    &goal.criteria,
                    goal.turns,
                    goal.cap,
                    goal::GOAL_MET,
                );
                self.push_log(format!("SYSTEM  Goal continues: {status}"));
                self.send_goal_turn(member, turn);
                return;
            }
            goal::Step::Met => format!("Goal met on {member}: {}", goal.status()),
            goal::Step::CapReached => format!(
                "Goal stopped at the {}-turn safety cap on {member}: {}",
                goal.cap, goal.criteria
            ),
            goal::Step::Failed(error) => {
                let message = format!("Goal stopped on {member}: {error}");
                self.goal = None;
                self.ledger_finish_goal(member, ledger::TaskStatus::Error);
                self.notice = message.clone();
                self.push_log(format!("SYSTEM  {message}"));
                return;
            }
        };
        self.ledger_finish_goal(member, ledger::TaskStatus::Done);
        self.goal = None;
        self.notice = message.clone();
        self.push_log(format!("SYSTEM  {message}"));
    }

    fn send_goal_turn(&mut self, member: &str, turn: specialists::GoalTurn) {
        let typed = std::mem::take(&mut self.input);
        match specialists::find(&self.crew, member) {
            Some(0) => self.message_commander_with_display(turn.model_prompt, turn.display_label),
            Some(index) => {
                self.delegate(index, turn.model_prompt);
                if let Some(slot) = self.crew.get_mut(index) {
                    slot.mission = turn.display_label;
                }
            }
            None => {}
        }
        self.input = typed;
        if !self.turn_scheduled(member) {
            let reason = std::mem::take(&mut self.notice);
            self.goal = None;
            self.ledger_finish_goal(member, ledger::TaskStatus::Error);
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
        let mut lines = vec![format!("**Q:** {}", chat.question)];
        match result {
            Ok(answer) if answer.is_empty() => lines.push("(no answer)".into()),
            Ok(answer) => lines.extend(answer),
            Err(error) => lines.push(format!("Side chat failed: {error}")),
        }
        self.transcript_for_mut(&chat.thread)
            .push(markdown::TranscriptEntry::AssistantMarkdown {
                speaker: "Side Q&A (/btw)".into(),
                markdown: lines.join("\n\n"),
                streaming: false,
            });
        self.notice = format!("Side answer ready in the {} feed.", chat.thread);
    }

    fn is_snoozed(&self, thread: &str) -> bool {
        self.snoozes.iter().any(|snooze| snooze.thread == thread)
    }

    pub(crate) fn collapsed_snooze(&self, thread: &str) -> Option<&str> {
        self.snoozes
            .iter()
            .find(|snooze| self.settings.snooze_limited_missions && snooze.thread == thread)
            .map(|snooze| snooze.banner.as_str())
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
        if let Err(error) = store.save_roster_merging(&self.crew, &[]) {
            self.notice = format!("Roster not saved: {error}");
        }
    }

    fn record(&mut self, index: usize, text: String) {
        if self.crew[index].is_commander() {
            return self.push_log(text);
        }
        let name = self.crew[index].name.clone();
        let entry = Entry::now(text);
        self.push_transcript_log_entry(&name, &entry.text, Some(entry.at.clone()));
        if let Some(store) = self.live.as_ref().and_then(|live| live.store.as_ref()) {
            if let Err(error) = store.append_history(&name, &entry) {
                self.notice = format!("History not saved: {error}");
            }
        }
        self.crew[index].remember(entry);
        self.touch(index);
    }

    fn record_log_only(&mut self, index: usize, text: String) {
        if self.crew[index].is_commander() {
            let entry = Entry::now(text);
            if let Some(live) = &self.live {
                if let Some(store) = &live.store {
                    let _ = store.append_commander_for_workspace(&live.workspace, &entry);
                }
            }
            self.log.push_back(entry.text);
            self.log_times.push_back(entry.at);
            if self.log.len() > specialists::FEED_LIMIT {
                self.log.pop_front();
                self.log_times.pop_front();
            }
            return;
        }
        let entry = Entry::now(text);
        if let Some(store) = self.live.as_ref().and_then(|live| live.store.as_ref()) {
            let _ = store.append_history(&self.crew[index].name, &entry);
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
        let title = targeted.text.clone();
        live.queue.push_back(Delegation {
            name: name.clone(),
            task: targeted.text,
            target: targeted.target,
            review: false,
        });
        if !self.goal_pinned_on(&name) {
            let status = if busy {
                ledger::TaskStatus::Queued
            } else {
                ledger::TaskStatus::Running
            };
            self.ledger_record_with(ledger::TaskKind::Spawn, &name, &title, status);
        }
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
        let title = format!(
            "Review round {}/{} · {}",
            plan.round,
            review::MAX_REVIEW_ROUNDS,
            plan.note
        );
        self.ledger_record(ledger::TaskKind::Review, &name, &title);
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
        let mut options = mission::LaunchOptions {
            policy: permissions::PermissionPolicy::for_mode(mode),
            ..mission::LaunchOptions::default()
        };
        let uniform_provider = live.profiles.first().map(|p| p.provider).filter(|first| {
            live.profiles
                .iter()
                .all(|candidate| candidate.provider == *first)
        });
        if let Some(provider) = uniform_provider {
            options.extra_args =
                bridge::worker_mcp_diet_args(bridge::BridgeProvider::from(provider));
        }
        let mission = mission::Mission::launch_with_options(
            live.profiles.clone(),
            live.route(request, delegation.target, mode, limits),
            workspace.path.clone(),
            options,
        );
        let worker_name = member.name.clone();
        let worker_role = member.role.clone();
        let worker_branch = member.branch().map(|b| b.name.clone()).unwrap_or_default();
        let worker_provider = uniform_provider
            .map(tui::provider_short_name)
            .unwrap_or("auto")
            .to_owned();
        live.runs
            .push(Run::new(&worker_name, mission, delegation.review));
        self.crew[index].status = Status::Running;
        self.transcript
            .push(markdown::TranscriptEntry::DelegationCard {
                worker: worker_name,
                role: worker_role,
                provider: worker_provider,
                branch: worker_branch,
                status: markdown::DelegationStatus::Running,
                summary: self.crew[index].mission.clone(),
            });
        Ok(())
    }

    pub fn poll_theme(&mut self, path: &Path) {
        let stamp = fs::metadata(path).and_then(|m| m.modified()).ok();
        if stamp != self.theme_stamp {
            self.theme_stamp = stamp;
            self.deck_palette = deck_palette(path);
            match Theme::load(path) {
                Ok(theme) => {
                    self.ui_palette = markdown::UiPalette::from_theme(&theme);
                    self.theme = theme;
                    self.notice.clear();
                }
                Err(e) => self.notice = format!("Theme unchanged: {e}"),
            }
        }
    }
    pub fn watch_updates(&mut self, receiver: Option<Receiver<String>>) {
        self.update_receiver = receiver;
        if self.update_receiver.is_some() && self.update_state == update::UpdateState::Idle {
            self.update_state = update::UpdateState::Checking;
        }
    }

    pub fn poll_update(&mut self) {
        let Some(receiver) = &self.update_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(latest) => {
                if !matches!(self.update_state, update::UpdateState::Downloading { .. }) {
                    self.update_state = update::UpdateState::Available {
                        tag: format!("v{latest}"),
                        current: update::CURRENT.to_owned(),
                    };
                    self.update_available = Some(latest);
                }
                if self.last_update_check.is_some() {
                    self.last_update_check = Some(Instant::now());
                }
                self.update_receiver = None;
            }
            Err(TryRecvError::Disconnected) => {
                if self.update_available.is_none()
                    && !self.update_check_in_flight
                    && self.update_state == update::UpdateState::Checking
                {
                    self.update_state = update::UpdateState::UpToDate {
                        current: update::CURRENT.to_owned(),
                    };
                }
                if !self.update_check_in_flight && self.last_update_check.is_some() {
                    self.last_update_check = Some(Instant::now());
                }
                self.update_receiver = None;
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    pub fn take_exit_message(&mut self) -> Option<String> {
        self.exit_message.take()
    }

    fn push_log(&mut self, text: String) {
        let entry = Entry::now(text);
        self.push_transcript_log_entry(COMMANDER, &entry.text, Some(entry.at.clone()));
        self.persist_commander_transcript();
        if let Some(live) = &self.live {
            if let Some(store) = &live.store {
                if let Err(error) = store.append_commander_for_workspace(&live.workspace, &entry) {
                    self.notice = format!("Transcript not saved: {error}");
                }
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
        let from_composer = self.composer.take_submission();
        let command = match from_composer {
            Some(text) => {
                self.input.clear();
                text
            }
            None => {
                let raw = self.input.trim().to_owned();
                if !raw.is_empty() {
                    self.composer.clear();
                }
                raw
            }
        };
        if command.trim().is_empty() {
            return;
        }
        self.submit_text(command);
    }

    pub(crate) fn submit_text(&mut self, command: String) {
        let trimmed = command.trim().to_owned();
        if trimmed.is_empty() {
            return;
        }
        if let Some(shell_cmd) = trimmed.strip_prefix('!') {
            self.input.clear();
            self.run_shell_command(shell_cmd.to_owned());
            return;
        }
        match specialists::parse_command(&trimmed) {
            Err(usage) => {
                self.notice = usage.clone();
                self.transcript
                    .push(markdown::TranscriptEntry::SystemNotice {
                        level: markdown::NoticeLevel::Warning,
                        text: usage,
                    });
            }
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
            specialists::Command::Diff { name } => {
                if name.is_none() && self.selected == 0 {
                    self.open_diff_viewer(None);
                    self.notice = "Opened workspace unified diff.".into();
                } else {
                    self.show_diff(name.as_deref());
                    let target = match name.as_deref() {
                        Some(n) => specialists::find(&self.crew, n).is_some(),
                        None => self.crew[self.selected].workspace.is_some(),
                    };
                    if target {
                        self.open_diff_viewer(name.as_deref());
                    }
                }
            }
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
            specialists::Command::Help => {
                self.show_help_in_transcript();
                self.record_log_only(0, format!("SYSTEM  {}", specialists::format_help()));
            }
            specialists::Command::Unknown {
                command,
                suggestion,
            } => {
                let msg = specialists::unknown_command_message(&command, suggestion.as_deref());
                self.notice = msg.clone();
                self.transcript
                    .push(markdown::TranscriptEntry::SystemNotice {
                        level: markdown::NoticeLevel::Warning,
                        text: msg,
                    });
            }
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
        if let Some(store) = self.live.as_ref().and_then(|live| live.store.as_ref()) {
            if let Err(error) = store.save_roster_merging(&self.crew, &[&member.name]) {
                self.notice = format!("Roster not saved: {error}");
            }
        }
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
        live.refresh_usage_now();
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
        self.message_commander_with_display(command.clone(), command);
    }

    fn message_commander_with_display(&mut self, command: String, display: String) {
        let limits = self.limit_policy();
        let commander_mode = self.settings.commander_permission;
        let prod_gate_enabled = self.settings.prod_gate;
        if self.live.is_none() {
            self.crew[0].mission = display.clone();
            self.push_log(format!("YOU  {display}"));
            self.input.clear();
            self.notice = PREVIEW_NOTICE.into();
            return;
        }
        if self.is_running(COMMANDER) {
            self.notice = if self.is_snoozed(COMMANDER) {
                "A mission is snoozed until its limit resets; /btw still works.".into()
            } else {
                "Commander is still working; follow-up queued.".into()
            };
            return;
        }
        if self.crew[0].paused {
            self.notice = "Resume Commander before sending a message.".into();
            return;
        }
        if let Some((index, claim)) = self.live.as_ref().and_then(|live| live.pending.clone()) {
            let live = self.live.as_mut().expect("checked live");
            let mission = mission::Mission::resume(
                live.profiles.clone(),
                index,
                live.workspace.clone(),
                command,
                claim,
            );
            live.runs.push(Run::new(COMMANDER, mission, false));
            self.record_commander_mission(&display);
            self.crew[0].mission = display;
            self.composer.set_running(true);
            self.input.clear();
            return;
        }
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.profiles.is_empty())
        {
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
        let parsed = match routing::parse(&targeted.text) {
            Ok(request) => request,
            Err(error) => {
                self.notice = error;
                return;
            }
        };
        let request = framed(parsed, |unit| self.build_commander_prompt_text(unit));
        let mcp_config = self.ensure_loopback_bridge();
        let (resume_session, resume_session_profile, portable_handoff) =
            if let Some(live) = &self.live {
                let handoff = (!live.handoff_turns.is_empty()).then(|| {
                    soul::build_portable_handoff(&live.handoff_turns, soul::DEFAULT_HANDOFF_TOKENS)
                });
                (
                    live.commander_session_id.clone(),
                    live.commander_session_profile.clone(),
                    handoff,
                )
            } else {
                (None, None, None)
            };
        let mut policy = permissions::PermissionPolicy::for_mode(commander_mode);
        if !prod_gate_enabled {
            policy.restrictions.clear();
        }
        let options = mission::LaunchOptions {
            resume_session,
            resume_session_profile,
            portable_handoff,
            mcp_config,
            extra_args: Vec::new(),
            policy,
            persistent_stdio: true,
        };
        self.turn_changes.clear();
        let live = self.live.as_mut().expect("checked live");
        let seq = (live.handoff_turns.len() + 1) as u64;
        live.handoff_turns.push(soul::HandoffTurn {
            seq,
            role: soul::TurnRole::User,
            content: display.clone(),
            command_outcome: None,
        });
        self.notice = format!("Routing in {}", live.workspace.display());
        let mission = mission::Mission::launch_with_options(
            live.profiles.clone(),
            live.route(request, targeted.target, commander_mode, limits),
            live.workspace.clone(),
            options,
        );
        live.runs.push(Run::new(COMMANDER, mission, false));
        self.crew[0].status = Status::Running;
        self.record_commander_mission(&display);
        self.crew[0].mission = display.clone();
        self.composer.set_running(true);
        self.input.clear();
        self.push_log(format!("YOU  {display}"));
    }

    pub(crate) fn abandon_continuation(&mut self) {
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
                self.composer.clear();
                "Interrupted continuation abandoned.".into()
            }
            Err(error) => error.to_string(),
        };
    }

    pub fn key(&mut self, key: KeyEvent) {
        self.handle_key_single_canvas(key);
    }

    pub fn click(&mut self, x: u16, y: u16) {
        self.handle_click_single_canvas(x, y);
    }

    pub fn draw(&mut self, f: &mut Frame) {
        self.selected = self.selected.min(self.crew.len().saturating_sub(1));
        self.sync_ledger();
        tui::render::draw_single_canvas(self, f);
    }
}

pub fn snapshot(path: &Path) -> io::Result<()> {
    let cols = env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|&n| n >= 40)
        .unwrap_or(120);
    let rows = env::var("LINES")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|&n| n >= 16)
        .unwrap_or(40);
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(cols, rows)).unwrap();
    let mut app = App::default();
    terminal.draw(|f| app.draw(f)).unwrap();
    let svg = tui::render_compact_svg(terminal.backend().buffer(), "#101011");
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
    use crossterm::event::{KeyCode, KeyModifiers};

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
        fs::write(&helper, format!("#!/bin/sh\n{}\n{}", usage::tests::READ_PROMPT, r#"
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
prompt=$(read_prompt "$@")
case "$prompt" in "You are Commander"*"Operator: Mission input") ;; *) exit 9 ;; esac
test -f workspace-marker || exit 9
long=
remaining=58
while test "$remaining" -gt 0; do
    long="${long}wide "
    remaining=$((remaining - 1))
done
if test "$1" = -p || test "$1" = --print; then
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}]}}\n' "$long"
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Claude output"},{"type":"tool_use","input":"internal event"}]}}'
else
    test "$1" = -a && test "$3" = exec || exit 10
    printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$long"
    printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Codex output"}}'
fi
while test ! -f release-mission; do sleep 0.01; done
"#)).unwrap();
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
            if !key {
                app.click(app.input_area.x + 1, app.input_area.y + 1);
            }
            type_and_enter(&mut app, "Mission input");
            wait_for(&mut app, |app| {
                app.log
                    .back()
                    .is_some_and(|line| line.contains(&format!("{provider} output")))
                    && transcript_has(app, &format!("{provider} output"))
            });
            assert_eq!(app.status(0), "RUNNING");
            let text = words(&region(
                &screen(&mut app, width, height),
                app.transcript_area,
            ));
            assert!(
                text.contains(&format!("{provider} output")),
                "{width}×{height}: {provider} tail hidden during streaming: {text}"
            );
            let live_spinner =
                |text: &str| text.contains("Working…") || text.contains("Streaming response…");
            assert!(live_spinner(&text), "{text}");
            assert!(!text.contains("internal event"));
            fs::write(workspace.join("release-mission"), "").unwrap();
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);
            let text = words(&region(
                &screen(&mut app, width, height),
                app.transcript_area,
            ));
            assert!(!live_spinner(&text), "{text}");
            assert!(
                text.contains(&format!("{provider} output")),
                "{width}×{height}: {provider} tail hidden after completion: {text}"
            );
            fs::remove_file(workspace.join("release-mission")).unwrap();
        }
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    fn transcript_has(app: &App, needle: &str) -> bool {
        app.active_transcript().entries.iter().any(|entry| {
            matches!(
                entry,
                markdown::TranscriptEntry::AssistantMarkdown { markdown, .. }
                    if markdown.contains(needle)
            )
        })
    }

    #[cfg(unix)]
    fn submit_with(app: &mut App, keyboard: bool, text: &str) {
        if !keyboard {
            app.click(app.input_area.x + 1, app.input_area.y + 1);
            assert!(app.editing);
        }
        type_and_enter(app, text);
    }

    #[cfg(unix)]
    fn type_and_enter(app: &mut App, text: &str) {
        app.editing = true;
        app.composer.set_text(text);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[cfg(unix)]
    fn wait_for(app: &mut App, ready: impl Fn(&App) -> bool) {
        use std::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready(app) {
            assert!(
                Instant::now() < deadline,
                "{} [{}]",
                app.notice,
                app.status(0)
            );
            app.poll_mission();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn continuation_receipt_is_visible_after_keyboard_and_mouse_submission() {
        for (width, height, keyboard) in [(80, 24, true), (120, 40, false)] {
            let (_fixture, workspace, profiles) =
                mission::tests::limit_fixture(&format!("continuation-panel-{width}"), usage::now());
            let mut app = App {
                live: Some(test_live(workspace.clone(), profiles, Some(0))),
                editing: keyboard,
                ..App::default()
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            if !keyboard {
                app.click(app.input_area.x + 1, app.input_area.y + 1);
                assert!(app.editing);
            }
            type_and_enter(&mut app, "Keep the finished step");
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let notice: String = (0..width)
                .map(|x| buffer[(x, height - 1)].symbol())
                .collect();
            assert!(
                app.notice
                    .contains("Done · Limit: claude/first → claude/standby"),
                "{}",
                app.notice
            );
            assert!(notice.contains("Done · Limit: claude/first →"), "{notice}");
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
            let top = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
                region(terminal.backend().buffer(), Rect::new(0, 0, width, 1))
            };
            let initial = top(&terminal);
            assert!(initial.contains("Commander [Claude · ready]"), "{initial}");
            assert!(!initial.contains("5h "), "{initial}");
            assert!(!app.live.as_ref().unwrap().allowances[0].latched);
            let mut press = |app: &mut App| {
                let area = app.live.as_ref().unwrap().profile_area;
                if key {
                    app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
                } else {
                    app.click(area.x + area.width / 2, area.y);
                }
                terminal.draw(|f| app.draw(f)).unwrap();
                top(&terminal)
            };
            let first = press(&mut app);
            assert_eq!(app.notice, "Override saved: claude/ready");
            assert!(first.contains("Commander [Claude · ready]"), "{first}");
            let changed = press(&mut app);
            assert_eq!(app.notice, format!("Override saved: claude/{long}"));
            assert!(changed.contains("Commander [Claude · aaaa"), "{changed}");
            let allowance = &app.live.as_ref().unwrap().allowances[1];
            assert!(allowance.latched);
            assert_eq!((allowance.five, allowance.week), (Some(0.0), Some(40.0)));
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
read_prompt "$@" > /dev/null
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
            assert!(row(&terminal, height - 1).contains("Override saved: claude/zulu"));

            let mut reopened = test_live(fixture.root.clone(), profiles.clone(), None);
            assert_eq!(reopened.load_override(), None);
            assert_eq!(reopened.preferred, Some(1));
            let mut app = App {
                live: Some(reopened),
                editing: keyboard,
                ..App::default()
            };
            app.poll_mission();
            terminal.draw(|f| app.draw(f)).unwrap();
            let area = app.live.as_ref().unwrap().profile_area;
            assert!(app.live.as_ref().unwrap().allowances[1].latched);
            let top = row(&terminal, area.y);
            assert!(top.contains("Commander ["), "{top}");
            assert_eq!(app.top_bar_commander_badge(), "Commander [Claude · zulu]");
            submit_with(&mut app, keyboard, "Do work");
            wait_for(&mut app, |app| {
                app.status(0) == "RUNNING" && !app.notice.starts_with("Routing in")
            });
            terminal.draw(|f| app.draw(f)).unwrap();
            assert!(
                app.notice
                    .contains("Override claude/zulu skipped: latched · rotated → claude/alpha"),
                "{}",
                app.notice
            );
            let notice = row(&terminal, height - 1);
            assert!(notice.contains("Override claude/zulu skipped"), "{notice}");
            assert_eq!(
                app.top_bar_commander_badge(),
                "Commander [Claude · alpha · running]"
            );
            fs::write(fixture.root.join("release"), "").unwrap();
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);

            fs::write(
                profiles[0].usage_directory().unwrap().join("state.json"),
                b"{",
            )
            .unwrap();
            app.editing = keyboard;
            submit_with(&mut app, keyboard, "Do work");
            wait_for(&mut app, |app| !app.mission_running());
            terminal.draw(|f| app.draw(f)).unwrap();
            assert_eq!(app.status(0), "ERROR");
            assert_eq!(
                app.notice,
                "Commander: No eligible signed-in subscription profile. Skipped: claude/alpha usage state unreadable; claude/zulu latched."
            );
            assert!(row(&terminal, height - 1).contains("Commander: No eligible"));
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
read_prompt "$@" > /dev/null
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
            let mut press = |app: &mut App| {
                let area = app.live.as_ref().unwrap().profile_area;
                if keyboard {
                    app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
                } else {
                    click_center(app, area);
                }
                terminal.draw(|f| app.draw(f)).unwrap();
                assert!(row(&terminal, area.y).contains("Commander ["));
                app.top_bar_commander_badge()
            };
            let alpha = "Commander [Claude · alpha · claude-opus-5-5 high]";
            assert_eq!(press(&mut app), alpha);
            assert_eq!(press(&mut app), "Commander [Claude · beta]");
            press(&mut app);
            assert_eq!(press(&mut app), alpha);
            submit_with(&mut app, keyboard, "/hard Do work");
            wait_for(&mut app, |app| {
                app.status(0) == "RUNNING" && !app.notice.starts_with("Routing in")
            });
            assert!(app.notice.starts_with(
                "Override → claude/alpha claude-opus-5-5 high · AA intelligence index 63.0 · 52 tok/s"
            ), "{}", app.notice);
            terminal.draw(|f| app.draw(f)).unwrap();
            let notice = row(&terminal, height - 1);
            assert!(notice.contains("Override → claude/alpha"), "{notice}");
            assert!(app
                .log
                .iter()
                .any(|line| line.contains("AA intelligence index 63.0")));
            assert_eq!(
                app.top_bar_commander_badge(),
                "Commander [Claude · alpha · claude-opus-5-5 high · running]"
            );
            fs::write(fixture.root.join("release"), "").unwrap();
            wait_for(&mut app, |app| !app.mission_running());
            assert_eq!(app.status(0), "DONE", "{}", app.notice);
            let arguments = fs::read_to_string(fixture.root.join("arguments")).unwrap();
            assert!(
                arguments.starts_with(
                    "--model claude-opus-5-5 --effort high -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio --setting-sources user --mcp-config "
                ) && arguments.ends_with(
                    ".json --permission-mode acceptEdits --allowedTools Bash,Edit,Write,NotebookEdit\n"
                ) && arguments.lines().count() == 1,
                "{arguments}"
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
            assert_eq!(reopened.pin_label(0), None);
        }
    }

    #[cfg(unix)]
    const ORCHESTRATOR_PROVIDER: &str = r#"
if test "$1" = auth; then
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
prompt=$(read_prompt "$@")
case "$prompt" in
  "You are Commander"*"Operator: Review the patch please")
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Spawning a reviewer.\nSPAWN_SPECIALIST: nova | reviewer | Check the README"}]}}' ;;
  "You are Commander"*"nova (reviewer, DONE)"*"Operator: Hi")
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
prompt=$(read_prompt "$@")
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

    #[cfg(unix)]
    fn words(text: &str) -> String {
        text.split_whitespace()
            .filter(|word| !matches!(*word, "│" | "┃"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(unix)]
    fn rail_rect(app: &App, target: tui::render::RailHit) -> Rect {
        app.rail_hits
            .iter()
            .find(|(_, hit)| *hit == target)
            .map(|(rect, _)| *rect)
            .unwrap_or_else(|| panic!("no rail hit for {target:?}"))
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
    fn side_answers(app: &App) -> Vec<String> {
        app.transcript
            .entries
            .iter()
            .filter_map(|entry| match entry {
                markdown::TranscriptEntry::AssistantMarkdown {
                    speaker, markdown, ..
                } if speaker == "Side Q&A (/btw)" => Some(markdown.clone()),
                _ => None,
            })
            .collect()
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
prompt=$(read_prompt "$@")
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
        wait_for(&mut app, |app| !side_answers(app).is_empty());
        assert!(app.mission_running());
        assert_eq!(app.crew[0].status, Status::Running);
        let feed = side_answers(&app);
        assert_eq!(feed.len(), 1, "{feed:?}");
        assert!(feed[0].contains("**Q:** Where are we?"), "{feed:?}");
        assert!(feed[0].contains("Side answer: still on step 2"), "{feed:?}");
        assert!(!app.log.iter().any(|line| line.contains("Where are we?")));
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
        assert_eq!(app.collapsed_snooze(COMMANDER), None);
        let buffer = screen(&mut app, 120, 40);
        let expanded = region(&buffer, app.transcript_area);
        assert!(
            expanded.contains("MAIN-HISTORY-MARKER") && !expanded.contains("SNOOZED"),
            "{expanded}"
        );
        app.settings.snooze_limited_missions = true;
        assert!(app
            .collapsed_snooze(COMMANDER)
            .is_some_and(|banner| banner.starts_with("SNOOZED  Snoozed: claude/alpha")));
        let buffer = screen(&mut app, 120, 40);
        let collapsed = region(&buffer, app.transcript_area);
        assert!(
            collapsed.contains("SNOOZED") && !collapsed.contains("MAIN-HISTORY-MARKER"),
            "{collapsed}"
        );
        send(&mut app, "Another task");
        assert!(app.notice.contains("snoozed until its limit resets"));
        app.handle_event(COMMANDER, mission::Event::Continuing { from: 0, to: 0 });
        assert_eq!(app.status(0), "RUNNING");
        assert_eq!(app.notice, "Reset passed: resumed on claude/alpha");
        assert_eq!(app.collapsed_snooze(COMMANDER), None);
        let buffer = screen(&mut app, 120, 40);
        assert!(region(&buffer, app.transcript_area).contains("MAIN-HISTORY-MARKER"));
        app.handle_event(
            COMMANDER,
            mission::Event::Snoozed {
                profile: 0,
                until: 4_102_444_800,
            },
        );
        app.key(quit);
        assert!(app.quit && !app.mission_running());
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
        assert!(app.log.contains(
            &"SYSTEM  Goal pinned on Commander: turn 1/10 · ship the release".to_owned()
        ));
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

        for (width, height, keyboard) in RAIL_INPUT_MATRIX {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            assert!(
                crew.contains("Commander") && crew.contains("nova"),
                "{crew}"
            );
            assert!(crew.contains("DONE"), "{width}x{height}: {crew}");
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
                app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            } else {
                let row = rail_rect(&app, tui::render::RailHit::SelectMember(1));
                app.click(row.x, row.y);
            }
            assert_eq!(app.selected, 1);
            let buffer = screen(&mut app, width, height);
            let feed = words(&region(&buffer, app.transcript_area));
            assert!(feed.contains("README looks good"), "{feed}");
            let banner = words(&region(&buffer, Rect::new(0, 0, width, height)));
            assert!(
                banner.contains("Focused: nova (review") && banner.contains("✓ nova DONE"),
                "{banner}"
            );
            if keyboard {
                app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            } else {
                let row = rail_rect(&app, tui::render::RailHit::SelectMember(0));
                app.click(row.x, row.y);
            }
            assert_eq!(app.selected, 0);
            let buffer = screen(&mut app, width, height);
            let commander_feed = words(&region(&buffer, app.transcript_area));
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
        let entry = app.live.as_ref().unwrap().entry.clone().unwrap();
        assert_eq!(entry.1, "auto");
        let name = entry
            .0
            .split_whitespace()
            .next()
            .unwrap()
            .trim_start_matches("claude/");
        let badge = format!("Commander [Claude · {name}]");
        assert_eq!(app.top_bar_commander_badge(), badge);
        let buffer = screen(&mut app, 120, 40);
        let top = region(&buffer, Rect::new(0, 0, 120, 1));
        assert!(top.contains(&badge), "{top}");

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
    const RAIL_SIZES: [(u16, u16); 2] = [(100, 30), (120, 40)];

    #[cfg(unix)]
    const RAIL_INPUT_MATRIX: [(u16, u16, bool); 4] = [
        (100, 30, true),
        (100, 30, false),
        (120, 40, true),
        (120, 40, false),
    ];

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
        assert_eq!(a.selected, 1);
        a.toggle_pause();
        assert!(a.crew[1].paused);
        a.toggle_pause();
        assert!(!a.crew[1].paused);
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
        for (w, h) in [(60, 20), (80, 24), (120, 40), (150, 46), (210, 54)] {
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
            assert!(text.contains("agent-command"));
            assert!(!text.contains("PRIVATEER"));
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
            assert!(region(&buffer, app.input_area).contains("Ask Commander"));
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
            assert_eq!(app.update_area, Rect::default());
            assert!(!row(&buffer, height - 1).contains("Update"));
            sender.send("9.9.9".to_owned()).unwrap();
            app.poll_update();
            assert_eq!(app.update_available.as_deref(), Some("9.9.9"));
            let buffer = screen(&mut app, width, height);
            let bar = row(&buffer, height - 1);
            assert!(bar.contains("Update v9.9.9"), "{bar}");
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
    fn update_button_label_exists_only_for_detected_releases() {
        let mut app = App::default();
        for state in [
            update::UpdateState::Idle,
            update::UpdateState::Checking,
            update::UpdateState::UpToDate {
                current: update::CURRENT.to_owned(),
            },
            update::UpdateState::Failed("offline".into()),
        ] {
            app.update_state = state.clone();
            assert!(!app.update_button_visible(), "{state:?}");
            for width in [60, 80, 120] {
                assert_eq!(app.update_button_label(width), None, "{state:?}");
            }
        }
        app.update_available = Some("1.2.3".into());
        assert!(app.update_button_visible());
        assert_eq!(
            app.update_button_label(80).as_deref(),
            Some("⬆ Update v1.2.3")
        );
        assert_eq!(
            app.update_button_label(120).as_deref(),
            Some("⬆ Update v1.2.3 (F6)")
        );
        app.update_available = None;
        app.update_state = update::UpdateState::Available {
            tag: "v2.0.0".into(),
            current: update::CURRENT.to_owned(),
        };
        assert_eq!(
            app.update_button_label(80).as_deref(),
            Some("⬆ Update v2.0.0")
        );
        app.update_state = update::UpdateState::Downloading { pct: 42 };
        assert!(app.update_button_visible());
        assert_eq!(
            app.update_button_label(120).as_deref(),
            Some("⬇ Updating 42%")
        );
    }

    #[test]
    fn manual_update_check_reports_clear_feedback_when_completed() {
        let mut app = App {
            manual_update_check: true,
            update_state: update::UpdateState::Checking,
            ..App::default()
        };
        app.apply_background_event(tui::events::AppEvent::UpdateChecked(
            update::UpdateState::UpToDate {
                current: update::CURRENT.to_owned(),
            },
        ));
        assert!(!app.manual_update_check);
        assert_eq!(app.notice, format!("Up to date (v{})", update::CURRENT));

        app.manual_update_check = true;
        app.apply_background_event(tui::events::AppEvent::UpdateChecked(
            update::UpdateState::Available {
                tag: "v0.9.0".into(),
                current: update::CURRENT.to_owned(),
            },
        ));
        assert_eq!(app.update_available.as_deref(), Some("0.9.0"));
        assert!(app.notice.contains("Update v0.9.0 available"));

        app.manual_update_check = true;
        app.apply_background_event(tui::events::AppEvent::UpdateChecked(
            update::UpdateState::Failed("network offline".into()),
        ));
        assert!(app.update_available.is_none());
        assert!(app.notice.contains("network offline"));
    }

    #[test]
    fn workspace_scoped_commander_history_isolates_directories() {
        let base =
            std::env::temp_dir().join(format!("agent-command-ws-history-{}", uuid::Uuid::new_v4()));
        let root = base.join("profiles");
        let dir_a = base.join("repo-a");
        let dir_b = base.join("repo-b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        let store = specialists::Store::new(root);
        store
            .append_commander_for_workspace(&dir_a, &Entry::now("YOU  secret-a".into()))
            .unwrap();
        store
            .append_commander_for_workspace(&dir_b, &Entry::now("YOU  secret-b".into()))
            .unwrap();
        let a_entries = store.load_commander_for_workspace(&dir_a).unwrap();
        let b_entries = store.load_commander_for_workspace(&dir_b).unwrap();
        assert_eq!(a_entries.len(), 1);
        assert_eq!(b_entries.len(), 1);
        assert_eq!(a_entries[0].text, "YOU  secret-a");
        assert_eq!(b_entries[0].text, "YOU  secret-b");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn structured_transcript_persists_and_reloads_with_tables_thinking_and_tool_cards() {
        let base = std::env::temp_dir().join(format!(
            "agent-command-structured-tx-{}",
            uuid::Uuid::new_v4()
        ));
        let root = base.join("profiles");
        let workspace = base.join("repo");
        fs::create_dir_all(&workspace).unwrap();
        let store = specialists::Store::new(root.clone());
        let saved = vec![
            markdown::TranscriptEntry::UserMessage {
                text: "Build gridfall.html".into(),
                timestamp: Some("01:25:00".into()),
            },
            markdown::TranscriptEntry::ThinkingBlock {
                text: "Design canvas loop and controls".into(),
                duration_ms: 2100,
                collapsed: true,
            },
            markdown::TranscriptEntry::ToolCallCard {
                call_id: "tool-write-1".into(),
                kind: markdown::ToolKind::Write,
                name: "Write".into(),
                summary: "C:\\Users\\j\\gridfall.html".into(),
                output: "Wrote 8420 bytes".into(),
                ok: Some(true),
                exit_code: Some(0),
                duration_ms: Some(120),
                collapsed: true,
            },
            markdown::TranscriptEntry::AssistantMarkdown {
                speaker: COMMANDER.into(),
                markdown: "I built Gridfall.\n\n| Key | Action |\n| --- | --- |\n| ← → | Move |"
                    .into(),
                streaming: true,
            },
        ];
        store
            .save_transcript_for_workspace(&workspace, &saved)
            .unwrap();

        let live = Live::new(workspace, Vec::new(), Some(root));
        let mut app = App::from_live(live);
        assert_eq!(app.transcript.entries.len(), 4);
        assert!(matches!(
            &app.transcript.entries[1],
            markdown::TranscriptEntry::ThinkingBlock { text, .. } if text.contains("Design canvas")
        ));
        assert!(matches!(
            &app.transcript.entries[2],
            markdown::TranscriptEntry::ToolCallCard { name, .. } if name == "Write"
        ));
        assert!(matches!(
            &app.transcript.entries[3],
            markdown::TranscriptEntry::AssistantMarkdown {
                streaming: false,
                ..
            }
        ));

        let buf = screen(&mut app, 100, 30);
        let rendered = region(&buf, app.transcript_area);
        assert!(rendered.contains("Write · C:\\Users\\j\\gridfall.html"));
        assert!(rendered.contains("─┼─"), "{rendered}");
        assert!(!rendered.contains("| --- |"), "{rendered}");
        assert_eq!(rendered.matches("Commander").count(), 1, "{rendered}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn legacy_commander_log_coalesces_multiline_reply_and_tables_into_one_block() {
        let base = std::env::temp_dir().join(format!(
            "agent-command-legacy-coalesce-{}",
            uuid::Uuid::new_v4()
        ));
        let root = base.join("profiles");
        let workspace = base.join("repo");
        fs::create_dir_all(&workspace).unwrap();
        let store = specialists::Store::new(root.clone());
        for line in [
            "YOU  Build a Tetris clone",
            "SYSTEM  Override → claude/default provider default · unscored · speed n/a · headroom unknown",
            "Commander  I built Gridfall.",
            "Commander  • Pieces: I, O, T, S, Z, J, L",
            "Commander  | Key | Action |",
            "Commander  |---|---|",
            "Commander  | ← → | Move |",
            "Commander  | Space | Hard drop |",
        ] {
            store
                .append_commander_for_workspace(&workspace, &Entry::now(line.into()))
                .unwrap();
        }

        let live = Live::new(workspace, Vec::new(), Some(root));
        let mut app = App::from_live(live);
        assert_eq!(app.transcript.entries.len(), 3);
        assert!(matches!(
            &app.transcript.entries[1],
            markdown::TranscriptEntry::SystemNotice { text, .. } if text == "Override → claude/default"
        ));
        assert!(matches!(
            &app.transcript.entries[2],
            markdown::TranscriptEntry::AssistantMarkdown { speaker, .. } if speaker == COMMANDER
        ));

        let buf = screen(&mut app, 100, 30);
        let rendered = region(&buf, app.transcript_area);
        assert_eq!(rendered.matches("Commander").count(), 1, "{rendered}");
        assert!(rendered.contains("─┼─"), "{rendered}");
        assert!(!rendered.contains("|---|"), "{rendered}");
        assert!(!rendered.contains("unscored"), "{rendered}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn live_normalized_events_update_spinner_phase_upsert_tools_and_expand_thinking() {
        let (mission, _sender) = mission::Mission::held();
        let mut live = Live::new(std::env::temp_dir(), Vec::new(), None);
        live.runs.push(Run::new(COMMANDER, mission, false));
        let mut app = App::from_live(live);
        app.crew[0].status = Status::Running;

        let initial = app.active_live_activity(COMMANDER).unwrap();
        assert!(initial.label.starts_with("Working…"));

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::ThinkingDelta("Planning HTML canvas".into()),
        );
        let thinking = app.active_live_activity(COMMANDER).unwrap();
        assert!(thinking.label.starts_with("Thinking…"));

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::ToolStart {
                call_id: "tool-1".into(),
                kind: mission::ToolKind::Write,
                name: "Write".into(),
                summary: "Write".into(),
                raw_input: serde_json::json!({}),
            },
        );
        let initial_tool = app.active_live_activity(COMMANDER).unwrap();
        assert!(initial_tool.label.ends_with(" · Write"));
        assert!(!initial_tool.label.contains("Write Write"));

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::ToolStart {
                call_id: "tool-1".into(),
                kind: mission::ToolKind::Write,
                name: "Write".into(),
                summary: "Write: C:\\Users\\j\\gridfall.html".into(),
                raw_input: serde_json::json!({"file_path": "C:\\Users\\j\\gridfall.html"}),
            },
        );
        let tool_phase = app.active_live_activity(COMMANDER).unwrap();
        assert!(tool_phase
            .label
            .contains("Write C:\\Users\\j\\gridfall.html"));
        assert!(!tool_phase.label.contains("Write: "));
        let tool_cards: Vec<_> = app
            .transcript
            .entries
            .iter()
            .filter(|e| matches!(e, markdown::TranscriptEntry::ToolCallCard { .. }))
            .collect();
        assert_eq!(tool_cards.len(), 1);

        let running_buf = screen(&mut app, 120, 30);
        let whole_running = region(&running_buf, Rect::new(0, 0, 120, 30));
        assert!(whole_running.contains("RUNNING 0s") || whole_running.contains("RUNNING "));
        assert!(whole_running.contains("Write · C:\\Users\\j\\gridfall.html"));

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::ToolEnd {
                call_id: "tool-1".into(),
                ok: true,
                output_preview: "Wrote gridfall.html".into(),
                exit_code: Some(0),
            },
        );
        let after_tool = app.active_live_activity(COMMANDER).unwrap();
        assert!(after_tool.label.starts_with("Working…"));

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::TextDelta("Built ".into()),
        );
        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::TextDelta("gridfall.html!".into()),
        );
        let streaming = app.active_live_activity(COMMANDER).unwrap();
        assert!(streaming.label.starts_with("Streaming response…"));
        let assistant_entries: Vec<_> = app
            .transcript
            .entries
            .iter()
            .filter(|e| matches!(e, markdown::TranscriptEntry::AssistantMarkdown { .. }))
            .collect();
        assert_eq!(assistant_entries.len(), 1);
    }

    #[test]
    fn format_routing_notice_strips_unscored_noise_and_preserves_scored_metrics() {
        assert_eq!(
            tui::app_ops::format_routing_notice(
                "Override → claude/work provider default · unscored · speed n/a · headroom unknown"
            ),
            "Override → claude/work"
        );
        assert_eq!(
            tui::app_ops::format_routing_notice(
                "Override claude/main skipped: latched · rotated → claude/default provider default · unscored · speed n/a · 23% left"
            ),
            "Override claude/main skipped: latched · rotated → claude/default · 23% left"
        );
        assert_eq!(
            tui::app_ops::format_routing_notice(
                "Override claude/main skipped: latched · Auto → codex/default provider default · unscored · speed n/a · 50% left"
            ),
            "Override claude/main skipped: latched · Auto → codex/default · 50% left"
        );
        assert_eq!(
            tui::app_ops::format_routing_notice(
                "Auto → claude/a claude-opus-5-5 high · AA intelligence index 55.0 · speed n/a · 100% left"
            ),
            "Auto → claude/a claude-opus-5-5 high · AA intelligence index 55.0 · 100% left"
        );
        assert_eq!(
            tui::app_ops::format_routing_notice(
                "Override → claude/alpha claude-opus-5-5 high · AA intelligence index 63.0 · 52 tok/s"
            ),
            "Override → claude/alpha claude-opus-5-5 high · AA intelligence index 63.0 · 52 tok/s"
        );
    }

    #[test]
    fn commander_session_profile_tracks_ownership_and_clears_on_failure_reset_and_clear() {
        let base = std::env::temp_dir().join(format!(
            "agent-command-session-profile-{}",
            uuid::Uuid::new_v4()
        ));
        let root = base.join("profiles");
        let workspace = base.join("work");
        fs::create_dir_all(&workspace).unwrap();
        let main = auth::Profile::new(root.clone(), auth::Provider::Claude, "main").unwrap();
        let backup = auth::Profile::new(root.clone(), auth::Provider::Claude, "backup").unwrap();
        let mut live = Live::new(workspace, vec![main.clone(), backup.clone()], None);
        live.running_profile = Some(0);
        let mut app = App {
            live: Some(live),
            ..App::default()
        };

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::SessionStarted {
                session_id: "sess-main".into(),
                model: Some("claude-sonnet".into()),
                profile: None,
            },
        );
        assert_eq!(
            app.live.as_ref().unwrap().commander_session_id.as_deref(),
            Some("sess-main")
        );
        assert_eq!(
            app.live
                .as_ref()
                .unwrap()
                .commander_session_profile
                .as_deref(),
            Some("claude/main")
        );

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::SessionStarted {
                session_id: "sess-backup".into(),
                model: None,
                profile: Some("claude/backup".into()),
            },
        );
        assert_eq!(
            app.live.as_ref().unwrap().commander_session_id.as_deref(),
            Some("sess-backup")
        );
        assert_eq!(
            app.live
                .as_ref()
                .unwrap()
                .commander_session_profile
                .as_deref(),
            Some("claude/backup")
        );

        let (mission, _tx) = mission::Mission::held();
        app.finish_commander(
            Run::new(COMMANDER, mission, false),
            Err(io::Error::other(
                "No conversation found with session ID: sess-backup",
            )),
        );
        assert_eq!(app.live.as_ref().unwrap().commander_session_id, None);
        assert_eq!(app.live.as_ref().unwrap().commander_session_profile, None);

        app.live.as_mut().unwrap().commander_session_id = Some("sess-2".into());
        app.live.as_mut().unwrap().commander_session_profile = Some("claude/main".into());
        app.live
            .as_mut()
            .unwrap()
            .reset_profiles(vec![main.clone(), backup]);
        assert_eq!(app.live.as_ref().unwrap().commander_session_id, None);
        assert_eq!(app.live.as_ref().unwrap().commander_session_profile, None);

        let live = app.live.as_mut().unwrap();
        live.commander_session_id = Some("sess-3".into());
        live.commander_session_profile = Some("claude/main".into());
        live.handoff_turns.push(soul::HandoffTurn {
            seq: 1,
            role: soul::TurnRole::User,
            content: "Prior question".into(),
            command_outcome: None,
        });
        app.clear_active_transcript();
        let live = app.live.as_ref().unwrap();
        assert_eq!(live.commander_session_id, None);
        assert_eq!(live.commander_session_profile, None);
        assert!(live.handoff_turns.is_empty());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn poll_mission_processes_running_event_before_normalized_session_and_text() {
        let base =
            std::env::temp_dir().join(format!("agent-command-poll-order-{}", uuid::Uuid::new_v4()));
        let root = base.join("profiles");
        let workspace = base.join("work");
        fs::create_dir_all(&workspace).unwrap();
        let alpha = auth::Profile::new(root.clone(), auth::Provider::Claude, "alpha").unwrap();
        let beta = auth::Profile::new(root, auth::Provider::Claude, "beta").unwrap();
        let (mut mission, event_tx) = mission::Mission::held();
        let (norm_tx, norm_rx) = std::sync::mpsc::channel();
        mission.normalized = norm_rx;

        let mut live = Live::new(workspace, vec![alpha, beta], None);
        live.preferred = Some(0);
        live.runs.push(Run::new(COMMANDER, mission, false));
        let mut app = App {
            live: Some(live),
            ..App::default()
        };

        event_tx
            .send(mission::Event::Running {
                profile: 1,
                reason: Some(
                    "Override claude/alpha skipped: latched · rotated → claude/beta provider default · unscored · speed n/a · 23% left"
                        .into(),
                ),
            })
            .unwrap();
        norm_tx
            .send(mission::NormalizedEvent::SessionStarted {
                session_id: "sess-beta".into(),
                model: None,
                profile: None,
            })
            .unwrap();
        norm_tx
            .send(mission::NormalizedEvent::TextDelta(
                "Hello from beta".into(),
            ))
            .unwrap();
        event_tx
            .send(mission::Event::Output("Hello from beta".into()))
            .unwrap();
        norm_tx
            .send(mission::NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: false,
                error_code: None,
                final_text: "Hello from beta".into(),
            })
            .unwrap();
        event_tx.send(mission::Event::Finished(Ok(()))).unwrap();

        app.poll_mission();

        let live = app.live.as_ref().unwrap();
        assert_eq!(live.commander_session_id.as_deref(), Some("sess-beta"));
        assert_eq!(
            live.commander_session_profile.as_deref(),
            Some("claude/beta")
        );
        assert_eq!(live.last_routed, Some(1));
        assert_eq!(app.crew[0].status, Status::Done);
        let assistant_entries: Vec<(&str, &str)> = app
            .transcript
            .entries
            .iter()
            .filter_map(|entry| match entry {
                markdown::TranscriptEntry::AssistantMarkdown {
                    speaker, markdown, ..
                } => Some((speaker.as_str(), markdown.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant_entries,
            vec![("Commander · Claude (beta)", "Hello from beta")]
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn top_bar_quota_badge_floors_percentages_respects_stale_and_refreshes_per_profile_state() {
        let base = std::env::temp_dir().join(format!(
            "agent-command-quota-badge-{}",
            uuid::Uuid::new_v4()
        ));
        let root = base.join("profiles");
        let workspace = base.join("work");
        fs::create_dir_all(&workspace).unwrap();
        let main = auth::Profile::new(root.clone(), auth::Provider::Claude, "main").unwrap();
        let backup = auth::Profile::new(root, auth::Provider::Claude, "backup").unwrap();
        main.prepare().unwrap();
        backup.prepare().unwrap();

        let mut live = Live::new(workspace, vec![main.clone(), backup], None);
        live.allowances = vec![
            usage::Allowance {
                five: Some(0.9),
                week: Some(23.8),
                reset_in: Some(3600),
                latched: false,
                stale: true,
            },
            usage::Allowance {
                five: Some(64.9),
                week: Some(88.7),
                reset_in: Some(7200),
                latched: false,
                stale: false,
            },
        ];
        live.preferred = Some(0);
        let mut app = App {
            live: Some(live),
            ..App::default()
        };
        assert_eq!(
            app.top_bar_quota_badge().as_deref(),
            Some("5h 0% 7d 23% stale")
        );

        app.live.as_mut().unwrap().preferred = Some(1);
        assert_eq!(app.top_bar_quota_badge().as_deref(), Some("5h 64% 7d 88%"));

        let now = usage::now();
        usage::Usage::observe_claude(
            &main,
            &serde_json::json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": "seven_day",
                    "resetsAt": now.wall + 3600
                }
            }),
            now,
        )
        .unwrap();
        app.live.as_mut().unwrap().preferred = Some(0);
        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::RateLimit {
                five_hour_pct: None,
                seven_day_pct: Some(100.0),
                resets_at: None,
            },
        );
        assert_eq!(app.top_bar_quota_badge().as_deref(), Some("7d 0%"));
        assert!(app.live.as_ref().unwrap().allowances[0].latched);
        assert!(!app.live.as_ref().unwrap().allowances[1].latched);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refresh_usage_polls_gemini_async_and_reloads_immediately_when_worker_completes() {
        let base = std::env::temp_dir().join(format!(
            "agent-command-gemini-async-usage-{}",
            uuid::Uuid::new_v4()
        ));
        let root = base.join("profiles");
        let workspace = base.join("work");
        fs::create_dir_all(&workspace).unwrap();
        let gemini = auth::Profile::new(root, auth::Provider::Gemini, "main")
            .unwrap()
            .without_provider();
        gemini.prepare().unwrap();

        let mut live = Live::new(workspace, vec![gemini.clone()], None);
        live.refresh_usage();
        assert!(live.usage_worker.is_some());
        if let Some(worker) = live.usage_worker.take() {
            let _ = worker.join();
        }

        let now = usage::now();
        usage::Usage::record_gemini_limit(&gemini, Some(now.wall + 1800), now).unwrap();
        let completed = std::thread::spawn(|| {});
        while !completed.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        live.usage_checked_at = Some(usage::now().elapsed / 1_000_000_000);
        live.usage_worker = Some(completed);
        live.refresh_usage();

        assert!(live.usage_worker.is_none());
        assert!(live.allowances[0].latched);
        assert!(live.allowances[0].reset_in.is_some());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn double_esc_arms_and_cancels_active_mission() {
        let (mission, _sender) = mission::Mission::held();
        let mut live = Live::new(std::env::temp_dir(), Vec::new(), None);
        live.runs.push(Run::new(COMMANDER, mission, false));
        let mut app = App::from_live(live);
        app.crew[0].status = Status::Running;
        app.composer.set_running(true);
        assert!(app.mission_running());

        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.esc_armed_active());
        assert!(app.notice.contains("Esc again to cancel"), "{}", app.notice);

        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.esc_armed_active());
        assert!(!app.mission_running());
        assert_eq!(app.crew[0].status, Status::Idle);
        assert!(!app.composer.is_running());
        assert!(
            app.notice.contains("Cancelled active mission"),
            "{}",
            app.notice
        );
    }

    #[test]
    fn provider_stderr_collapses_when_nonfatal_and_expands_on_error() {
        let mut app = App::default();
        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::StderrLine("WARN config line 1".into()),
        );
        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::StderrLine("WARN config line 2".into()),
        );
        let stderr_cards: Vec<_> = app
            .transcript
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    markdown::TranscriptEntry::ToolCallCard { name, .. } if name == "Provider stderr"
                )
            })
            .collect();
        assert_eq!(stderr_cards.len(), 1);
        match stderr_cards[0] {
            markdown::TranscriptEntry::ToolCallCard {
                collapsed,
                ok,
                summary,
                output,
                ..
            } => {
                assert!(*collapsed);
                assert_eq!(*ok, Some(true));
                assert!(summary.contains("2 lines"), "{summary}");
                assert!(output.contains("WARN config line 2"));
            }
            _ => panic!("expected stderr tool card"),
        }

        app.handle_normalized_event(
            COMMANDER,
            mission::NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: true,
                error_code: None,
                final_text: "fatal failure".into(),
            },
        );
        let expanded = app.transcript.entries.iter().find(|e| {
            matches!(
                e,
                markdown::TranscriptEntry::ToolCallCard {
                    name,
                    collapsed: false,
                    ok: Some(false),
                    ..
                } if name == "Provider stderr"
            )
        });
        assert!(expanded.is_some());
    }

    #[test]
    fn slash_diff_on_commander_opens_modal_without_specialist_error() {
        let mut app = App {
            input: "/diff".into(),
            ..App::default()
        };
        app.submit();
        assert!(app.diff_modal.is_open());
        assert!(
            !app.notice.contains("Select a specialist"),
            "{}",
            app.notice
        );
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
        let cards = deck_cards(app.live.as_ref().unwrap());

        let mut compact = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 24));
        token_burn::render_deck_as(
            token_burn::DeckMode::Compact,
            &cards,
            Rect::new(1, 1, 78, 3),
            &mut compact,
            &palette,
        );
        let [known_row, fresh_row, held_row] = [1, 2, 3].map(|y| row(&compact, y));
        assert!(known_row.contains("claude · known"), "{known_row}");
        assert!(
            known_row.contains("64%") && known_row.contains("12%"),
            "{known_row}"
        );
        assert!(
            known_row.contains("↻ 1d 0h") || known_row.contains("↻ 23h 59m"),
            "{known_row}"
        );
        assert!(fresh_row.contains("claude · fresh") && fresh_row.contains("--"));
        assert!(fresh_row.contains("↻ -"), "{fresh_row}");
        assert!(held_row.contains("claude · held") && held_row.contains("0%"));
        let (x, y) = cell_at(&compact, "--").unwrap();
        assert_eq!((y, compact[(x, y)].fg), (2, palette.unknown));
        let (x, y) = cell_at(&compact, " - ").unwrap();
        assert_eq!(compact[(x, y)].bg, palette.unknown);
        let (x, y) = cell_at(&compact, "↻ -").unwrap();
        assert_eq!(compact[(x, y)].fg, palette.muted);

        let mut full = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 40));
        let columns = ratatui::layout::Layout::horizontal(vec![
            ratatui::layout::Constraint::Fill(1);
            cards.len()
        ])
        .spacing(1)
        .split(Rect::new(3, 2, 114, token_burn::FULL_CARD_HEIGHT));
        for (card, column) in cards.iter().zip(columns.iter()) {
            token_burn::render_deck_as(
                token_burn::DeckMode::Full,
                std::slice::from_ref(card),
                *column,
                &mut full,
                &palette,
            );
        }
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

        let top = row(&screen(&mut app, 120, 40), 0);
        assert!(
            top.contains("Commander [Claude · known] · ○ 5h 64% 7d 12%"),
            "{top}"
        );
    }

    #[cfg(unix)]
    fn deck_cards(live: &Live) -> Vec<token_burn::AccountCard> {
        live.profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let label = profile.to_string();
                let (provider, name) = label.split_once('/').unwrap_or((&label, ""));
                let allowance = live.allowances.get(index).copied().unwrap_or_default();
                let (route_model, route_metric) = match live.routes.get(index) {
                    _ if allowance.latched => (None, Some("latched".to_owned())),
                    Some(ProfileRoute::Routed { model, metric }) => {
                        (Some(model.clone()), Some(metric.clone()))
                    }
                    Some(ProfileRoute::Excluded(reason)) => (None, Some(reason.clone())),
                    None => (None, None),
                };
                token_burn::AccountCard {
                    provider: provider.into(),
                    profile: name.into(),
                    tier: String::new(),
                    primary_remaining: allowance.five,
                    secondary_remaining: allowance.week,
                    seconds_until_reset: allowance.reset_in,
                    route_model,
                    route_metric,
                }
            })
            .collect()
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
        let deck = "Commander [Claude · alpha] · ◑ 5h 50% 7d 75%";
        let rail = |app: &mut App, code: KeyCode| {
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.key(KeyEvent::new(code, KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        };
        for (width, height) in RAIL_SIZES {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            assert!(
                crew.contains("Commander") && crew.contains("atlas"),
                "{crew}"
            );
            assert!(row(&buffer, 0).contains(deck), "{}", row(&buffer, 0));
            assert!(region(&buffer, app.input_area).contains("Give atlas a task"));
            rail(&mut app, KeyCode::Home);
            assert_eq!(app.selected, 0);
            let buffer = screen(&mut app, width, height);
            let whole = region(&buffer, buffer.area);
            assert!(whole.contains("Roster:"), "{width}x{height}\n{whole}");
            assert!(row(&buffer, 0).contains(deck));
            assert!(region(&buffer, app.input_area).contains("Ask Commander anything"));
            rail(&mut app, KeyCode::Down);
            assert_eq!(app.selected, 1);
            assert!(app.editing);
            app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            assert_eq!(app.input, "x");
            app.composer.clear();
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
prompt=$(read_prompt "$@")
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

        for (width, height) in RAIL_SIZES {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            let order = row_order(&crew, &["Commander", "atlas", "nova"]);
            assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{crew}");
            let rows: Vec<&str> = crew.lines().collect();
            let atlas = rows.iter().find(|line| line.contains("atlas")).unwrap();
            let nova = rows.iter().find(|line| line.contains("nova")).unwrap();
            assert!(atlas.contains("! atlas"), "{width}x{height}\n{crew}");
            assert!(atlas.contains(Status::NeedsApproval.label()), "{crew}");
            assert!(
                nova.contains("● nova") && nova.contains("RUNNING"),
                "{crew}"
            );
            assert!(region(&buffer, buffer.area).contains("WORKERS (2)"));
        }
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.crew[app.selected].name, "atlas");
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.crew[app.selected].name, "nova");
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

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
        for (width, height) in RAIL_SIZES {
            let buffer = screen(&mut app, width, height);
            let banner_row = if app.left_panel_visible() { 1 } else { 2 };
            let banner = row(&buffer, banner_row);
            assert!(
                banner.contains("Focused: nova (builder") && banner.contains("[d: Diff]"),
                "{width}x{height}\n{banner}"
            );
            let whole = words(&region(&buffer, buffer.area));
            assert!(
                whole.contains("CHANGES (1)") && whole.contains("parser.txt +1 -0"),
                "{width}x{height}\n{whole}"
            );
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
        assert!(app.diff_modal.is_open());
        assert_eq!(app.notice, "Opened workspace unified diff.");
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
        for (index, (width, height)) in RAIL_SIZES.into_iter().enumerate() {
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            assert!(
                crew.contains("▸ Settled (1)") && !crew.contains("nova"),
                "{crew}"
            );
            if index == 0 {
                app.key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
            } else {
                let header = rail_rect(&app, tui::render::RailHit::ToggleSettled);
                app.click(header.x + 1, header.y);
            }
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            let order = row_order(&crew, &["atlas", "▾ Settled (1)"]);
            assert!(order[0] < order[1], "{crew}");
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(app.selected, nova);
            let buffer = screen(&mut app, width, height);
            let crew = region(&buffer, app.crew_area);
            let order = row_order(&crew, &["Settled", "nova"]);
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
        let whole = words(&region(&buffer, buffer.area));
        assert!(whole.contains("Focused: atlas (writer"), "{whole}");
        assert!(
            whole.contains("CHANGES (0)") && !whole.contains("+1 -0"),
            "{whole}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_app_cancels_every_running_specialist() {
        let (fixture, _repository, mut app) = parallel_app("parallel-drop");
        send(&mut app, "/spawn atlas writer Hang");
        wait_for(&mut app, |_| fixture.root.join("hanging").exists());
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
        assert!(app.quit && !app.mission_running());
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
