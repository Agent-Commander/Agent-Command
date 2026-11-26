use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, mpsc, Arc},
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};

use crate::{
    auth::Provider,
    branches, bridge, clipboard,
    editor::{EditorAction, TriggerKind},
    markdown::{
        DelegationStatus, LiveActivity, NoticeLevel, ScrollState, ToolKind, Transcript,
        TranscriptEntry, TranscriptHitAction,
    },
    mission::{HealthCode, NormalizedEvent, PatchKind},
    palette::PaletteAction,
    panel, specialists,
    tui::{
        diff_modal::DiffModalAction,
        events::{self, AppEvent, ChangedFile},
        render::{FocusBannerHit, RailHit, WelcomeHit},
    },
    update::UpdateState,
    usage, App, RunPhase,
};

const ARM_WINDOW: Duration = Duration::from_secs(2);
pub(crate) const UPDATE_POLL_INTERVAL: Duration = Duration::from_secs(10 * 60);
const MAX_SHELL_CONTEXT_SNIPPETS: usize = 4;
const MAX_SHELL_SNIPPET_CHARS: usize = 2000;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) fn is_session_not_found_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no conversation found")
        || lower.contains("session not found")
        || lower.contains("no such session")
        || lower.contains("thread not found")
        || lower.contains("no such thread")
        || lower.contains("conversation not found")
        || (lower.contains("session")
            && (lower.contains("not found")
                || lower.contains("invalid session")
                || lower.contains("does not exist")))
        || (lower.contains("thread") && lower.contains("not found"))
}

pub(crate) fn format_routing_notice(reason: &str) -> String {
    let trimmed = reason.trim();
    if !(trimmed.starts_with("Override →")
        || trimmed.starts_with("Auto →")
        || trimmed.starts_with("Override "))
    {
        return trimmed.to_owned();
    }
    let unscored = trimmed.contains(" · unscored");
    let mut parts: Vec<String> = trimmed
        .split(" · ")
        .filter(|seg| !matches!(*seg, "unscored" | "speed n/a" | "headroom unknown"))
        .map(str::to_owned)
        .collect();
    for part in &mut parts {
        if unscored || part.contains("→ ") {
            if let Some(stripped) = part.strip_suffix(" provider default") {
                *part = stripped.to_owned();
            }
        }
    }
    parts.join(" · ")
}

fn format_elapsed_tenths(elapsed_ms: u64) -> String {
    format!("{:.1}s", elapsed_ms as f64 / 1000.0)
}

fn clean_tool_summary(tool_name: &str, summary: &str) -> String {
    let trimmed = summary.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(tool_name) {
        return String::new();
    }
    if let Some((prefix, rest)) = trimmed.split_once(':') {
        if prefix.trim().eq_ignore_ascii_case(tool_name) {
            return rest.trim().to_owned();
        }
    }
    trimmed.to_owned()
}

fn legacy_line_needs_blank_separator(prev_line: &str, next_line: &str) -> bool {
    let prev_table = prev_line.starts_with('|');
    let next_table = next_line.starts_with('|');
    let next_heading = next_line.starts_with('#');
    let next_fence = next_line.starts_with("```");
    (prev_table != next_table) || next_heading || next_fence
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.right()
        && row >= rect.y
        && row < rect.bottom()
}

fn current_turn_start(entries: &[TranscriptEntry]) -> usize {
    entries
        .iter()
        .rposition(|entry| matches!(entry, TranscriptEntry::UserMessage { .. }))
        .map_or(0, |idx| idx + 1)
}

fn provider_stderr_prefix(name: &str) -> String {
    format!("stderr-{name}-turn-")
}

fn is_open_provider_stderr_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix) && !id.ends_with("-done")
}

fn parse_stderr_turn_number(id: &str, prefix: &str) -> Option<usize> {
    let rest = id.strip_prefix(prefix)?;
    let number = rest.strip_suffix("-done").unwrap_or(rest);
    number.parse().ok()
}

fn next_provider_stderr_call_id(entries: &[TranscriptEntry], name: &str) -> String {
    let prefix = provider_stderr_prefix(name);
    let max_existing = entries
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::ToolCallCard { call_id, .. } => {
                parse_stderr_turn_number(call_id, &prefix)
            }
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let user_turns = entries
        .iter()
        .filter(|entry| matches!(entry, TranscriptEntry::UserMessage { .. }))
        .count();
    let next_turn = (max_existing + 1).max(user_turns.max(1));
    format!("{prefix}{next_turn}")
}

fn pop_trailing_turn_error_notice(entries: &mut Vec<TranscriptEntry>) {
    let turn_start = current_turn_start(entries);
    if entries.len() > turn_start
        && matches!(
            entries.last(),
            Some(TranscriptEntry::SystemNotice {
                level: NoticeLevel::Error,
                ..
            })
        )
    {
        entries.pop();
    }
}

fn summarize_permission_command(input: &serde_json::Value) -> String {
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return cmd.to_owned();
    }
    if let Some(cmd) = input.get("cmd").and_then(|v| v.as_str()) {
        return cmd.to_owned();
    }
    if let Some(text) = input.as_str() {
        return text.to_owned();
    }
    let rendered = input.to_string();
    if rendered.len() > 160 {
        format!("{}…", rendered.chars().take(160).collect::<String>())
    } else {
        rendered
    }
}

impl App {
    pub fn esc_armed_active(&self) -> bool {
        self.esc_armed_at
            .is_some_and(|at| at.elapsed() <= ARM_WINDOW)
    }

    pub fn ctrl_c_armed_active(&self) -> bool {
        self.ctrl_c_armed_at
            .is_some_and(|at| at.elapsed() <= ARM_WINDOW)
    }

    pub fn leader_armed_active(&self) -> bool {
        self.leader_armed_at
            .is_some_and(|at| at.elapsed() <= ARM_WINDOW)
    }

    pub(crate) fn persist_commander_transcript(&self) {
        if let Some(live) = &self.live {
            if let Some(store) = &live.store {
                let _ =
                    store.save_transcript_for_workspace(&live.workspace, &self.transcript.entries);
            }
        }
    }

    pub fn active_live_activity(&self, name: &str) -> Option<LiveActivity> {
        if let Some(live) = &self.live {
            if let Some(pos) = live.run_position(name) {
                let run = &live.runs[pos];
                let elapsed_ms = run.started_at.elapsed().as_millis() as u64;
                let spinner = SPINNER_FRAMES[((elapsed_ms / 100) % 10) as usize];
                let dur = format_elapsed_tenths(elapsed_ms);
                let label = match &run.phase {
                    RunPhase::Working => format!("Working… {dur}"),
                    RunPhase::Thinking => format!("Thinking… {dur}"),
                    RunPhase::Tool {
                        name: tool_name,
                        summary,
                    } => {
                        if summary.is_empty() {
                            format!("Working… {dur} · {tool_name}")
                        } else {
                            format!("Working… {dur} · {tool_name} {summary}")
                        }
                    }
                    RunPhase::Streaming => format!("Streaming response… {dur}"),
                };
                return Some(LiveActivity {
                    spinner,
                    label,
                    elapsed_ms,
                });
            }
        }
        let member_running = self
            .crew
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(name))
            .is_some_and(|m| m.status == specialists::Status::Running);
        if member_running {
            return Some(LiveActivity {
                spinner: SPINNER_FRAMES[0],
                label: "Working… 0.0s".into(),
                elapsed_ms: 0,
            });
        }
        None
    }

    pub fn running_elapsed_secs(&self, name: &str) -> Option<u64> {
        let live = self.live.as_ref()?;
        let pos = live.run_position(name)?;
        Some(live.runs[pos].started_at.elapsed().as_secs())
    }

    pub fn active_transcript(&self) -> &Transcript {
        let name = self.crew[self.selected].name.as_str();
        if name == specialists::COMMANDER {
            &self.transcript
        } else {
            self.specialist_transcripts
                .get(name)
                .unwrap_or(&self.transcript)
        }
    }

    pub(crate) fn transcript_for_mut(&mut self, name: &str) -> &mut Transcript {
        if name == specialists::COMMANDER {
            &mut self.transcript
        } else {
            self.specialist_transcripts
                .entry(name.to_owned())
                .or_default()
        }
    }

    pub(crate) fn active_scroll_mut(&mut self) -> &mut ScrollState {
        let name = self.crew[self.selected].name.clone();
        if name == specialists::COMMANDER {
            &mut self.scroll
        } else {
            self.specialist_scrolls.entry(name).or_default()
        }
    }

    pub fn workspace_display_name(&self) -> String {
        self.live
            .as_ref()
            .and_then(|live| live.workspace.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "preview".into())
    }

    pub fn top_bar_commander_badge(&self) -> String {
        let Some(live) = &self.live else {
            return "Commander [Preview]".into();
        };
        if live.profiles.is_empty() || self.no_provider_connected() {
            return "Commander [No provider]".into();
        }
        let idx = live.displayed_profile().min(live.profiles.len() - 1);
        let profile = &live.profiles[idx];
        let provider = crate::tui::provider_short_name(profile.provider);
        let pin = live
            .pin_label(idx)
            .map(|label| format!(" · {label}"))
            .unwrap_or_default();
        let status = if live.running(specialists::COMMANDER) {
            " · running"
        } else {
            ""
        };
        format!("Commander [{provider} · {}{pin}{status}]", profile.name())
    }

    pub fn top_bar_github_badge(&self) -> Option<String> {
        if !self.github_status.authenticated {
            return None;
        }
        let login = self.github_status.login.as_deref().unwrap_or("connected");
        Some(format!("gh:@{login}"))
    }

    pub fn top_bar_quota_badge(&self) -> Option<String> {
        let live = self.live.as_ref()?;
        let idx = live.displayed_profile();
        let allowance = live.allowances.get(idx)?;
        if live
            .profiles
            .get(idx)
            .is_some_and(|profile| profile.provider == Provider::Gemini)
        {
            return allowance.latched.then(|| {
                if allowance.reset_in.is_none() {
                    "reset unknown".to_owned()
                } else {
                    "latched".to_owned()
                }
            });
        }
        let mut parts = Vec::new();
        if let Some(five) = allowance.five {
            parts.push(format!("5h {:.0}%", five.clamp(0.0, 100.0).floor()));
        }
        if let Some(week) = allowance.week {
            parts.push(format!("7d {:.0}%", week.clamp(0.0, 100.0).floor()));
        }
        if parts.is_empty() {
            return allowance.latched.then(|| {
                if allowance.reset_in.is_none() {
                    "reset unknown".to_owned()
                } else {
                    "latched".to_owned()
                }
            });
        }
        if allowance.stale {
            parts.push("stale".to_owned());
        }
        Some(parts.join(" "))
    }

    pub fn footer_mode_label(&self) -> String {
        let mode = self.settings.commander_permission.flag();
        if self.settings.prod_gate {
            format!("{mode} · ProdGate on")
        } else {
            format!("{mode} · ProdGate off")
        }
    }

    pub fn update_button_visible(&self) -> bool {
        self.update_available.is_some()
            || matches!(
                self.update_state,
                UpdateState::Available { .. } | UpdateState::Downloading { .. }
            )
    }

    pub fn update_button_label(&self, bar_width: u16) -> Option<String> {
        if let UpdateState::Downloading { pct } = &self.update_state {
            return Some(format!("⬇ Updating {pct}%"));
        }
        let version = self
            .update_available
            .clone()
            .or_else(|| self.update_state.available_version().map(str::to_owned))?;
        Some(if bar_width >= 100 {
            format!("⬆ Update v{version} (F6)")
        } else {
            format!("⬆ Update v{version}")
        })
    }

    pub fn connection_summary(&self, provider: Provider) -> (bool, String) {
        let short_method = |raw: &str| -> &'static str {
            match raw.to_ascii_lowercase().as_str() {
                "oauth_token" | "claude.ai" => "oauth",
                "oauth-personal" => "google",
                "chatgpt" => "chatgpt",
                "api_key" | "gemini-api-key" => "api",
                "bedrock" => "bedrock",
                "vertex" | "vertex-ai" => "vertex",
                "foundry" => "foundry",
                _ => "ok",
            }
        };
        if let Some(live) = &self.live {
            if let Some((profile, _)) = live
                .profiles
                .iter()
                .zip(&live.signed_in)
                .find(|(p, signed)| **signed && p.provider == provider)
            {
                let det = live
                    .profile_detections
                    .get(&profile.to_string())
                    .filter(|d| d.logged_in)
                    .or_else(|| {
                        self.auth_detections
                            .iter()
                            .find(|d| d.provider == provider && d.logged_in)
                    });
                let suffix = det
                    .and_then(|d| d.auth_method.as_deref().or(d.tier.as_deref()))
                    .map(|m| format!(" · {}", short_method(m)))
                    .unwrap_or_default();
                return (true, format!("{}{suffix}", profile.name()));
            }
        }
        if let Some(det) = self
            .auth_detections
            .iter()
            .find(|d| d.provider == provider && d.logged_in)
        {
            let detail = det
                .auth_method
                .as_deref()
                .or(det.tier.as_deref())
                .map(short_method)
                .unwrap_or("ok");
            return (true, format!("default · {detail}"));
        }
        (false, "not signed in".into())
    }

    pub fn visible_changes(&self) -> Vec<ChangedFile> {
        if self.selected > 0 {
            if let Some(changes) = &self.crew[self.selected].changes {
                return changes
                    .files
                    .iter()
                    .map(|f| ChangedFile {
                        status: if f.removed == 0 && f.added > 0 {
                            "A".into()
                        } else {
                            "M".into()
                        },
                        path: f.path.clone(),
                        added: f.added,
                        removed: f.removed,
                    })
                    .collect();
            }
        }
        if !self.turn_changes.is_empty() {
            return self.turn_changes.clone();
        }
        self.git_snapshot.files.clone()
    }

    pub(crate) fn automatic_update_check_enabled(&self) -> bool {
        !cfg!(debug_assertions) && self.launch_update_check()
    }

    pub(crate) fn periodic_update_check_due_at(&self, now: Instant) -> bool {
        self.bg_tx.is_some()
            && self.launch_update_check()
            && !self.update_check_in_flight
            && self.update_receiver.is_none()
            && !matches!(
                self.update_state,
                UpdateState::Checking | UpdateState::Downloading { .. }
            )
            && self
                .last_update_check
                .is_some_and(|last| now.saturating_duration_since(last) >= UPDATE_POLL_INTERVAL)
    }

    pub(crate) fn begin_periodic_update_check(
        &mut self,
        now: Instant,
        spawn: impl FnOnce(mpsc::Sender<AppEvent>),
    ) -> bool {
        if !self.periodic_update_check_due_at(now) {
            return false;
        }
        let Some(tx) = self.bg_tx.clone() else {
            return false;
        };
        self.last_update_check = Some(now);
        self.update_check_in_flight = true;
        self.manual_update_check = false;
        if !matches!(self.update_state, UpdateState::Available { .. })
            && self.update_available.is_none()
        {
            self.update_state = UpdateState::Checking;
        }
        spawn(tx);
        true
    }

    pub(crate) fn maybe_poll_periodic_update(&mut self, now: Instant) -> bool {
        if !self.automatic_update_check_enabled() {
            return false;
        }
        self.begin_periodic_update_check(now, events::spawn_update_check)
    }

    pub fn start_background_probes(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.bg_tx = Some(tx.clone());
        self.bg_rx = Some(rx);
        let workspace = self
            .live
            .as_ref()
            .map(|live| live.workspace.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        let profile_root = self
            .live
            .as_ref()
            .and_then(|live| live.profile_root.clone());
        let check_updates = self.automatic_update_check_enabled();
        self.last_update_check = Some(Instant::now());
        if check_updates {
            self.manual_update_check = false;
            self.update_check_in_flight = true;
            if !matches!(self.update_state, UpdateState::Available { .. })
                && self.update_available.is_none()
            {
                self.update_state = UpdateState::Checking;
            }
        }
        events::spawn_startup_probes(workspace, profile_root, check_updates, tx);
    }

    pub(crate) fn trigger_live_update_check(&mut self) {
        self.trigger_live_update_check_with(Instant::now(), events::spawn_update_check);
    }

    pub(crate) fn trigger_live_update_check_with(
        &mut self,
        now: Instant,
        spawn: impl FnOnce(mpsc::Sender<AppEvent>),
    ) {
        if matches!(self.update_state, UpdateState::Downloading { .. }) {
            return;
        }
        self.manual_update_check = true;
        self.last_update_check = Some(now);
        self.update_state = UpdateState::Checking;
        self.notice = "Checking GitHub releases…".into();
        if self.update_check_in_flight && self.bg_tx.is_some() {
            return;
        }
        self.update_check_in_flight = true;
        let tx = if let Some(tx) = &self.bg_tx {
            tx.clone()
        } else {
            let (tx, rx) = mpsc::channel();
            self.bg_tx = Some(tx.clone());
            self.bg_rx = Some(rx);
            tx
        };
        spawn(tx);
    }

    pub fn poll_background_events(&mut self) {
        let mut drained = Vec::new();
        if let Some(rx) = &self.bg_rx {
            while let Ok(ev) = rx.try_recv() {
                drained.push(ev);
            }
        }
        for ev in drained {
            self.apply_background_event(ev);
        }

        let wakes: Vec<bridge::BackgroundResult> = self
            .live
            .as_ref()
            .and_then(|live| live.bridge.as_ref())
            .and_then(bridge::LoopbackBridge::wake_queue)
            .map(bridge::WakeQueue::drain)
            .unwrap_or_default();
        for wake in wakes {
            self.apply_background_event(AppEvent::WorkerWake(wake));
        }

        self.maybe_poll_periodic_update(Instant::now());
    }

    pub(crate) fn apply_background_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::AuthDetected {
                detections,
                profiles,
                profile_detections,
            } => {
                self.auth_detections = detections;
                if let Some(live) = &mut self.live {
                    if !profiles.is_empty() {
                        live.reset_profiles(profiles);
                    }
                    for (label, det) in profile_detections {
                        live.profile_detections.insert(label, det);
                    }
                    for profile in &live.profiles {
                        let label = profile.to_string();
                        if profile.is_system_default() {
                            if let Some(det) = self
                                .auth_detections
                                .iter()
                                .find(|d| d.provider == profile.provider)
                            {
                                live.profile_detections.insert(label, det.clone());
                            }
                        } else {
                            live.profile_detections
                                .entry(label)
                                .or_insert_with(|| profile.detect_passive());
                        }
                    }
                    live.signed_in = live
                        .profiles
                        .iter()
                        .map(|profile| {
                            if let Some(host) = &self.host {
                                host.signed_in(profile)
                            } else {
                                live.profile_detections
                                    .get(&profile.to_string())
                                    .map_or_else(
                                        || profile.detect_passive().logged_in,
                                        |det| det.logged_in,
                                    )
                            }
                        })
                        .collect();
                    let any_ready = live.signed_in.contains(&true)
                        || self.auth_detections.iter().any(|d| d.logged_in);
                    self.show_welcome = !any_ready;
                }
            }
            AppEvent::GitHubDetected(status) => {
                self.github_status = status;
            }
            AppEvent::UpdateChecked(state) => {
                self.update_check_in_flight = false;
                let was_manual = std::mem::take(&mut self.manual_update_check);
                if matches!(self.update_state, UpdateState::Downloading { .. }) {
                    return;
                }
                if !was_manual
                    && matches!(state, UpdateState::Failed(_))
                    && (matches!(self.update_state, UpdateState::Available { .. })
                        || self.update_available.is_some())
                {
                    return;
                }
                if let Some(ver) = state.available_version() {
                    self.update_available = Some(ver.to_owned());
                } else {
                    self.update_available = None;
                }
                if was_manual {
                    self.notice = match &state {
                        UpdateState::UpToDate { current } => format!("Up to date (v{current})"),
                        UpdateState::Available { tag, .. } => {
                            format!("Update {tag} available — press F6 or click Update")
                        }
                        UpdateState::Failed(err) => format!("Update check failed: {err}"),
                        UpdateState::Idle
                        | UpdateState::Checking
                        | UpdateState::Downloading { .. } => String::new(),
                    };
                }
                self.update_state = state;
            }
            AppEvent::GitSnapshotReady(snapshot) => {
                self.git_snapshot = snapshot;
            }
            AppEvent::ShellFinished {
                command,
                stdout,
                stderr,
                exit_code,
                duration_ms,
            } => {
                let ok = exit_code == Some(0);
                let mut combined = String::new();
                if !stdout.trim().is_empty() {
                    combined.push_str(stdout.trim_end());
                }
                if !stderr.trim().is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(stderr.trim_end());
                }
                let call_id = format!("shell-{}", uuid::Uuid::new_v4().simple());
                self.transcript.push(TranscriptEntry::ToolCallCard {
                    call_id,
                    kind: ToolKind::Bash,
                    name: "Shell".into(),
                    summary: command.clone(),
                    output: combined.clone(),
                    ok: Some(ok),
                    exit_code,
                    duration_ms: Some(duration_ms),
                    collapsed: ok,
                });
                let clipped: String = combined.chars().take(MAX_SHELL_SNIPPET_CHARS).collect();
                if let Some(live) = &mut self.live {
                    live.recent_shell_context
                        .push_back(format!("$ {command} (exit {:?})\n{clipped}", exit_code));
                    while live.recent_shell_context.len() > MAX_SHELL_CONTEXT_SNIPPETS {
                        live.recent_shell_context.pop_front();
                    }
                }
                self.notice = if ok {
                    format!("!{command} finished (0)")
                } else {
                    format!("!{command} exited ({:?})", exit_code)
                };
                self.persist_commander_transcript();
            }
            AppEvent::WorkerWake(result) => {
                self.note_worker_wake(&result);
                let status = if result.is_error {
                    DelegationStatus::Failed
                } else {
                    DelegationStatus::Completed
                };
                self.transcript.push(TranscriptEntry::DelegationCard {
                    worker: result.worker_id.clone(),
                    role: result.role.clone(),
                    provider: "mcp".into(),
                    branch: result.branch.clone().unwrap_or_default(),
                    status,
                    summary: result.summary.clone(),
                });
                self.persist_commander_transcript();
                self.notice = format!("Worker {} finished ({})", result.worker_id, result.status);
                if let Some(live) = &self.live {
                    if let Some(store) = &live.store {
                        if let Ok(roster) = store.load_roster() {
                            for member in roster {
                                if specialists::find(&self.crew, &member.name).is_none() {
                                    self.crew.push(member);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn ensure_loopback_bridge(&mut self) -> Option<PathBuf> {
        let live = self.live.as_mut()?;
        let root = live.profile_root.clone()?;
        if live.bridge.is_none() {
            let soul = self
                .settings
                .effective_soul_path()
                .map(crate::soul::SoulStore::new)
                .or_else(crate::soul::SoulStore::from_env);
            let ask_user = bridge::AskUserBroker::new(false);
            live.ask_user = Some(ask_user.clone());
            let config = bridge::BridgeSessionConfig {
                workspace_root: live.workspace.clone(),
                profile_root: root,
                soul,
                catalog: bridge::ExternalToolCatalog::new(),
                ask_user,
                wake_queue: bridge::WakeQueue::new(),
                shutdown: Arc::new(AtomicBool::new(false)),
            };
            if let Ok(started) = bridge::LoopbackBridge::start_session(config) {
                live.bridge = Some(started);
            }
        }
        let bridge = live.bridge.as_ref()?;
        bridge
            .injection_for(bridge::BridgeProvider::Claude)
            .ok()
            .and_then(|inj| inj.config_file)
    }

    pub(crate) fn handle_normalized_event(&mut self, name: &str, event: NormalizedEvent) {
        let is_commander = name == specialists::COMMANDER;
        match event {
            NormalizedEvent::SessionStarted {
                session_id,
                model,
                profile,
            } => {
                if is_commander {
                    if let Some(live) = &mut self.live {
                        let running_label = live
                            .running_profile
                            .and_then(|idx| live.profiles.get(idx))
                            .map(ToString::to_string);
                        live.commander_session_id = Some(session_id);
                        live.commander_session_profile = profile.or(running_label);
                    }
                }
                if let Some(model_name) = model {
                    self.notice = format!("{name} · {model_name}");
                }
            }
            NormalizedEvent::TextDelta(delta) => {
                if let Some(live) = &mut self.live {
                    if let Some(pos) = live.run_position(name) {
                        live.runs[pos].saw_normalized_text = true;
                        live.runs[pos].phase = RunPhase::Streaming;
                    }
                }
                let speaker = self.speaker_label(name);
                self.transcript_for_mut(name)
                    .append_assistant_delta(&speaker, &delta);
            }
            NormalizedEvent::ThinkingDelta(delta) => {
                let elapsed = self
                    .live
                    .as_mut()
                    .and_then(|live| {
                        let pos = live.run_position(name)?;
                        let run = &mut live.runs[pos];
                        run.phase = RunPhase::Thinking;
                        let started = run.thinking_started_at.get_or_insert_with(Instant::now);
                        Some(started.elapsed().as_millis() as u64)
                    })
                    .unwrap_or(0);
                self.transcript_for_mut(name)
                    .append_thinking_delta(&delta, elapsed);
            }
            NormalizedEvent::ToolStart {
                call_id,
                kind,
                name: tool_name,
                summary,
                ..
            } => {
                let cleaned_summary = clean_tool_summary(&tool_name, &summary);
                if let Some(live) = &mut self.live {
                    if let Some(pos) = live.run_position(name) {
                        let run = &mut live.runs[pos];
                        run.phase = RunPhase::Tool {
                            name: tool_name.clone(),
                            summary: cleaned_summary.clone(),
                        };
                        run.tool_started_at
                            .entry(call_id.clone())
                            .or_insert_with(Instant::now);
                    }
                }
                self.transcript_for_mut(name).upsert_tool_call(
                    call_id,
                    crate::tui::map_tool_kind(kind),
                    tool_name,
                    cleaned_summary,
                );
                if is_commander {
                    self.persist_commander_transcript();
                }
            }
            NormalizedEvent::ToolEnd {
                call_id,
                ok,
                output_preview,
                exit_code,
            } => {
                let duration = self.live.as_mut().and_then(|live| {
                    let pos = live.run_position(name)?;
                    let run = &mut live.runs[pos];
                    run.phase = RunPhase::Working;
                    run.tool_started_at
                        .remove(&call_id)
                        .map(|started| started.elapsed().as_millis() as u64)
                });
                let updated = self.transcript_for_mut(name).complete_tool_call(
                    &call_id,
                    output_preview.clone(),
                    ok,
                    exit_code,
                    duration,
                );
                if !updated && !ok && !output_preview.is_empty() {
                    self.transcript_for_mut(name)
                        .push(TranscriptEntry::SystemNotice {
                            level: NoticeLevel::Error,
                            text: output_preview,
                        });
                }
                if is_commander {
                    self.persist_commander_transcript();
                }
            }
            NormalizedEvent::FileChanged { path, kind } => {
                let status = match kind {
                    PatchKind::Add => "A",
                    PatchKind::Update => "M",
                    PatchKind::Delete => "D",
                };
                if let Some(existing) = self.turn_changes.iter_mut().find(|c| c.path == path) {
                    existing.status = status.into();
                } else {
                    self.turn_changes.push(ChangedFile {
                        status: status.into(),
                        path,
                        added: 0,
                        removed: 0,
                    });
                }
            }
            NormalizedEvent::PermissionAsked {
                req_id,
                tool,
                input,
                prod_gate_reason,
            } => {
                let command = summarize_permission_command(&input);
                let gate_decision = if self.settings.prod_gate {
                    crate::prodgate::ProdGate::check(&tool, &input)
                } else {
                    None
                };
                let reason = prod_gate_reason.or_else(|| gate_decision.map(|d| d.reason));
                if let Some(reason) = reason {
                    if let Some(live) = &mut self.live {
                        live.pending_prod_gates
                            .insert(req_id.clone(), name.to_owned());
                    }
                    self.transcript_for_mut(name)
                        .push(TranscriptEntry::ProdGateCard {
                            req_id,
                            tool: tool.clone(),
                            command,
                            reason,
                            resolved: false,
                        });
                    self.notice = format!(
                        "ProdGate approval required for {tool} · click Allow/Deny or reply to steer"
                    );
                } else if let Some(live) = &self.live {
                    if let Some(pos) = live.run_position(name) {
                        let _ = live.runs[pos]
                            .mission
                            .respond_permission(&req_id, true, None);
                    }
                }
            }
            NormalizedEvent::RateLimit { .. } => {
                if let Some(live) = &mut self.live {
                    live.refresh_usage_now();
                }
            }
            NormalizedEvent::UsageDelta { .. } => {}
            NormalizedEvent::StderrLine(line) => {
                if is_commander && is_session_not_found_error(&line) {
                    if let Some(live) = &mut self.live {
                        if let Some(pos) = live.run_position(name) {
                            live.runs[pos].saw_session_not_found = true;
                        }
                    }
                }
                self.append_provider_stderr(name, &line);
            }
            NormalizedEvent::TurnFinished {
                interrupted,
                is_error,
                error_code,
                final_text,
            } => {
                self.transcript_for_mut(name).finish_assistant_stream();
                if interrupted {
                    self.finish_turn_provider_stderr(name, false);
                    self.transcript_for_mut(name)
                        .push(TranscriptEntry::SystemNotice {
                            level: NoticeLevel::Warning,
                            text: "Turn interrupted.".into(),
                        });
                } else if is_error {
                    if is_commander && is_session_not_found_error(&final_text) {
                        if let Some(live) = &mut self.live {
                            if let Some(pos) = live.run_position(name) {
                                live.runs[pos].saw_session_not_found = true;
                            }
                        }
                    }
                    self.finish_turn_provider_stderr(name, true);
                    if error_code == Some(HealthCode::AgentAuth) {
                        self.transcript_for_mut(name)
                            .push(TranscriptEntry::InlineConnectCard {
                                id: format!("auth-{}", uuid::Uuid::new_v4().simple()),
                                title: "Provider authentication required".into(),
                                detail: if final_text.is_empty() {
                                    "Sign in to your CLI provider and retry.".into()
                                } else {
                                    final_text.clone()
                                },
                                provider_or_tool: "provider".into(),
                                resolved: false,
                            });
                    } else if !final_text.is_empty() {
                        let transcript = self.transcript_for_mut(name);
                        let turn_start = current_turn_start(&transcript.entries);
                        let already_shown =
                            transcript.entries[turn_start..]
                                .last()
                                .is_some_and(|entry| {
                                    matches!(
                                        entry,
                                        TranscriptEntry::SystemNotice {
                                            level: NoticeLevel::Error,
                                            text,
                                        } if text == &final_text
                                    )
                                });
                        if !already_shown {
                            transcript.push(TranscriptEntry::SystemNotice {
                                level: NoticeLevel::Error,
                                text: final_text,
                            });
                        }
                    }
                } else {
                    self.finish_turn_provider_stderr(name, false);
                }
                if is_commander {
                    self.persist_commander_transcript();
                }
                if let (Some(live), Some(tx)) = (&self.live, &self.bg_tx) {
                    events::spawn_git_refresh(live.workspace.clone(), tx.clone());
                }
            }
        }
    }

    fn speaker_label(&self, name: &str) -> String {
        if name != specialists::COMMANDER {
            return name.to_owned();
        }
        let Some(live) = &self.live else {
            return specialists::COMMANDER.to_owned();
        };
        if let Some(profile) = live.profiles.get(live.displayed_profile()) {
            format!(
                "Commander · {} ({})",
                crate::tui::provider_short_name(profile.provider),
                profile.name()
            )
        } else {
            specialists::COMMANDER.to_owned()
        }
    }

    pub fn open_command_palette(&mut self) {
        let profiles = self
            .live
            .as_ref()
            .map(|live| live.profiles.as_slice())
            .unwrap_or(&[]);
        let items = crate::tui::build_palette_items(
            &self.crew,
            profiles,
            crate::tui::PaletteState {
                rail_open: self.rail_open,
                left_panel_open: self.left_panel_visible(),
                update_available: self.update_available.as_deref(),
            },
        );
        self.palette.set_items(items);
        self.palette.open();
    }

    pub(crate) fn execute_palette_action(&mut self, id: &str) {
        match id {
            "view.diff" => self.open_diff_viewer(None),
            "view.rail" => {
                self.rail_open = !self.rail_open;
                if !self.rail_open {
                    self.rail_focused = false;
                }
                self.editing = true;
            }
            "view.left" => {
                self.toggle_left_panel();
                self.editing = true;
            }
            "view.tab.chat" => self.set_view_tab(crate::tui::tabs::ViewTab::Chat),
            "view.tab.inbox" => self.set_view_tab(crate::tui::tabs::ViewTab::Inbox),
            "view.tab.ledger" => self.set_view_tab(crate::tui::tabs::ViewTab::Ledger),
            "settings.open" => self.open_settings(),
            "transcript.copy" => self.copy_latest_reply(),
            "update.trigger" => self.request_update(),
            "profile.cycle" => self.select_profile(),
            "mode.cycle" => {
                let next = self.settings.commander_permission.next_commander();
                self.set_commander_mode(next);
            }
            "prodgate.toggle" => {
                self.settings.prod_gate = !self.settings.prod_gate;
                let state = if self.settings.prod_gate { "on" } else { "off" };
                self.notice = format!("ProdGate floor: {state}");
                if let Some(path) = &self.launch.settings_path {
                    let _ = self.settings.save(path);
                }
            }
            "transcript.toggle_expand" => {
                self.toggle_latest_expand();
            }
            "transcript.clear" => {
                self.clear_active_transcript();
            }
            "editor.external" => self.request_external_editor(),
            "git.pr" => self.open_pr(None, false),
            "connect.open" => self.open_connect(panel::Origin::Direct),
            "help.show" => self.show_help_in_transcript(),
            "app.quit" => {
                self.interrupt_all_runs();
                self.quit = true;
            }
            other if other.starts_with("entry:") => {
                if let Some(profile) = other.strip_prefix("entry:") {
                    self.set_entry(profile);
                }
            }
            other if other.starts_with("member:") => {
                if let Ok(idx) = other["member:".len()..].parse::<usize>() {
                    if idx < self.crew.len() {
                        self.select_member_thread(idx);
                    }
                }
            }
            _ => {}
        }
        if self.panel.is_none() && !self.diff_modal.is_open() {
            self.editing = true;
        }
    }

    pub fn select_member_thread(&mut self, new_index: usize) {
        let clamped = new_index.min(self.crew.len().saturating_sub(1));
        if clamped == self.selected {
            return;
        }
        let from = self.crew[self.selected].name.clone();
        let to = self.crew[clamped].name.clone();
        self.composer.switch_thread(&from, &to);
        self.input = self.composer.text().to_owned();
        self.selected = clamped;
        self.refresh_autocomplete();
    }

    pub fn open_diff_viewer(&mut self, target_name: Option<&str>) {
        if let Some(name) = target_name {
            if let Some(idx) = specialists::find(&self.crew, name) {
                let member = &self.crew[idx];
                if let Some(ws) = &member.workspace {
                    let diff = branches::unified_diff(&ws.path, ws.branch.as_ref())
                        .unwrap_or_else(|e| format!("Diff unavailable: {e}"));
                    self.diff_modal
                        .open(format!("Diff · {}", member.name), diff);
                    return;
                }
            }
            self.notice = format!("No diff available for {name}.");
            return;
        }
        if self.selected > 0 {
            let member = &self.crew[self.selected];
            if let Some(ws) = &member.workspace {
                let diff = branches::unified_diff(&ws.path, ws.branch.as_ref())
                    .unwrap_or_else(|e| format!("Diff unavailable: {e}"));
                self.diff_modal
                    .open(format!("Diff · {}", member.name), diff);
                return;
            }
        }
        let diff = if !self.git_snapshot.unified_diff.is_empty() {
            self.git_snapshot.unified_diff.clone()
        } else if let Some(live) = &self.live {
            branches::unified_diff(&live.workspace, None).unwrap_or_default()
        } else {
            String::new()
        };
        let title = format!("Unified Diff · {}", self.workspace_display_name());
        self.diff_modal.open(title, diff);
    }

    pub fn copy_latest_reply(&mut self) {
        let payload = self
            .active_transcript()
            .latest_assistant_text()
            .map(str::to_owned)
            .or_else(|| {
                let text = self.active_transcript().plain_text();
                (!text.trim().is_empty()).then_some(text)
            });
        let Some(text) = payload else {
            self.notice = "Nothing to copy yet.".into();
            return;
        };
        match clipboard::copy_text(&text) {
            Ok(clipboard::CopyBackend::Osc52Only) => {
                self.notice = "Copied reply to terminal clipboard (OSC 52).".into();
            }
            Ok(_) => {
                self.notice = "Copied reply to clipboard.".into();
            }
            Err(error) => {
                self.notice = format!("Clipboard unavailable: {error}");
            }
        }
    }

    pub fn toggle_latest_expand(&mut self) {
        let name = self.crew[self.selected].name.clone();
        if self
            .transcript_for_mut(&name)
            .toggle_expand_at_or_latest(None)
        {
            self.notice = "Toggled card expansion.".into();
        }
    }

    pub fn clear_active_transcript(&mut self) {
        let name = self.crew[self.selected].name.clone();
        if name == specialists::COMMANDER {
            self.log.clear();
            self.log_times.clear();
            self.transcript.clear();
            self.scroll.jump_to_top();
            if let Some(live) = &mut self.live {
                live.clear_commander_session();
                live.handoff_turns.clear();
            }
            self.persist_commander_transcript();
        } else {
            self.crew[self.selected].history.clear();
            self.transcript_for_mut(&name).clear();
            self.active_scroll_mut().jump_to_top();
        }
        self.notice = "Cleared transcript view.".into();
    }

    pub fn show_help_in_transcript(&mut self) {
        let mut md =
            String::from("### Slash Commands\n\n| Command | Description |\n| --- | --- |\n");
        for entry in specialists::help_entries() {
            let usage = entry.usage.replace('|', "\\|");
            let summary = entry.summary.replace('|', "\\|");
            md.push_str(&format!("| `{usage}` | {summary} |\n"));
        }
        md.push_str(
            "\n### Shortcuts\n\n\
             - `Ctrl+P` / `Ctrl+K` — Fuzzy command palette\n\
             - `Ctrl+B` — Toggle collapsible right rail\n\
             - `F8` / `Ctrl+\\` — Toggle left panel · `Shift+Tab` focus it\n\
             - `Alt+1` / `Alt+2` / `Alt+3` — Chat / Inbox / Ledger tabs\n\
             - `Ctrl+E` — Expand or collapse thinking / tool card\n\
             - `Ctrl+Y` — Copy latest reply (host clipboard + OSC 52)\n\
             - `Shift+Enter` / `Alt+Enter` / `Ctrl+J` — Multiline newline\n\
             - `Enter` / `Ctrl+Enter` — Send (or Queue / Steer while running)\n\
             - `Esc×2` — Cancel active turn cleanly\n\
             - `F2` profile · `F3` settings · `F6` update · `!cmd` local shell · `Ctrl+X e` `$EDITOR`\n",
        );
        self.transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: "Help".into(),
            markdown: md,
            streaming: false,
        });
        self.scroll.jump_to_bottom();
        self.notice = "Command reference shown in transcript.".into();
    }

    pub fn request_external_editor(&mut self) {
        self.external_editor_request = Some(self.composer.text().to_owned());
    }

    pub fn take_external_editor_request(&mut self) -> Option<String> {
        self.external_editor_request.take()
    }

    pub fn apply_external_editor_text(&mut self, text: String) {
        self.composer.set_text(text);
        self.input = self.composer.text().to_owned();
        self.refresh_autocomplete();
        self.notice = "Updated draft from $EDITOR.".into();
    }

    pub fn take_relaunch_executable(&mut self) -> Option<PathBuf> {
        self.relaunch_executable.take()
    }

    pub fn interrupt_all_runs(&mut self) {
        let mut cancelled_names = Vec::new();
        if let Some(live) = &mut self.live {
            let cancelled = std::mem::take(&mut live.runs);
            for run in cancelled {
                cancelled_names.push(run.name.clone());
                run.mission.interrupt();
                drop(run);
            }
            live.running_profile = None;
            live.pending_prod_gates.clear();
            live.queue.clear();
        }
        for name in &cancelled_names {
            self.finish_turn_provider_stderr(name, false);
        }
        let had_runs = !cancelled_names.is_empty();
        self.composer.clear_queued();
        self.composer.set_running(false);
        for member in &mut self.crew {
            if member.status == specialists::Status::Running {
                member.status = specialists::Status::Idle;
            }
        }
        if had_runs {
            self.transcript.finish_assistant_stream();
            self.transcript.push(TranscriptEntry::SystemNotice {
                level: NoticeLevel::Warning,
                text: "Turn cancelled.".into(),
            });
            self.persist_commander_transcript();
        }
    }

    pub(crate) fn append_provider_stderr(&mut self, name: &str, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let prefix = provider_stderr_prefix(name);
        let transcript = self.transcript_for_mut(name);
        let turn_start = current_turn_start(&transcript.entries);
        if let Some(TranscriptEntry::ToolCallCard {
            summary, output, ..
        }) = transcript.entries[turn_start..]
            .iter_mut()
            .rev()
            .find(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::ToolCallCard { call_id, .. }
                        if is_open_provider_stderr_id(call_id, &prefix)
                )
            })
        {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(trimmed);
            let count = output.lines().count();
            let noun = if count == 1 { "line" } else { "lines" };
            *summary = format!("{count} {noun} · Ctrl+E to expand");
            return;
        }
        let call_id = next_provider_stderr_call_id(&transcript.entries, name);
        transcript.push(TranscriptEntry::ToolCallCard {
            call_id,
            kind: ToolKind::Other,
            name: "Provider stderr".into(),
            summary: "1 line · Ctrl+E to expand".into(),
            output: trimmed.to_owned(),
            ok: Some(true),
            exit_code: None,
            duration_ms: None,
            collapsed: true,
        });
    }

    fn finish_turn_provider_stderr(&mut self, name: &str, failed: bool) {
        let prefix = provider_stderr_prefix(name);
        let transcript = self.transcript_for_mut(name);
        let turn_start = current_turn_start(&transcript.entries);
        if let Some(TranscriptEntry::ToolCallCard {
            call_id,
            ok,
            collapsed,
            ..
        }) = transcript.entries[turn_start..]
            .iter_mut()
            .rev()
            .find(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::ToolCallCard { call_id, .. }
                        if is_open_provider_stderr_id(call_id, &prefix)
                )
            })
        {
            if failed {
                *ok = Some(false);
                *collapsed = false;
            }
            call_id.push_str("-done");
        }
    }

    pub(crate) fn mark_provider_stderr_failed(&mut self, name: &str) {
        self.finish_turn_provider_stderr(name, true);
        pop_trailing_turn_error_notice(&mut self.transcript_for_mut(name).entries);
    }

    pub fn run_shell_command(&mut self, command: String) {
        let trimmed = command.trim().to_owned();
        if trimmed.is_empty() {
            self.notice = "Enter a shell command after `!`.".into();
            return;
        }
        let workspace = self
            .live
            .as_ref()
            .map(|live| live.workspace.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        if let Some(tx) = &self.bg_tx {
            events::spawn_shell_command(workspace, trimmed.clone(), tx.clone());
            self.notice = format!("Running `!{trimmed}`…");
        } else {
            let (tx, rx) = mpsc::channel();
            self.bg_tx = Some(tx.clone());
            self.bg_rx = Some(rx);
            events::spawn_shell_command(workspace, trimmed.clone(), tx);
            self.notice = format!("Running `!{trimmed}`…");
        }
    }

    pub fn refresh_autocomplete(&mut self) {
        let Some(trigger) = self.composer.active_trigger() else {
            self.autocomplete.close();
            return;
        };
        let candidates = match trigger.kind {
            TriggerKind::Command => crate::tui::build_slash_candidates(),
            TriggerKind::Mention => {
                let workspace = self
                    .live
                    .as_ref()
                    .map(|live| live.workspace.clone())
                    .unwrap_or_else(|| PathBuf::from("."));
                crate::tui::build_mention_candidates(&workspace, &self.crew)
            }
        };
        self.autocomplete.update(&trigger.query, &candidates);
    }

    pub fn paste(&mut self, text: &str) {
        if self.panel.is_some() || self.palette.is_open() || self.diff_modal.is_open() {
            return;
        }
        self.rail_focused = false;
        self.editing = true;
        self.composer.handle_paste(text);
        self.input = self.composer.text().to_owned();
        self.refresh_autocomplete();
    }

    pub fn scroll_mouse(&mut self, col: u16, row: u16, delta: isize) {
        if self.panel.is_some() {
            self.panel_scroll(delta);
            return;
        }
        if self.palette.is_open() {
            self.palette.handle_scroll(delta);
            return;
        }
        if self.diff_modal.is_open() {
            self.diff_modal.handle_scroll(delta);
            return;
        }
        if rect_contains(self.transcript_area, col, row) {
            let scroll = self.active_scroll_mut();
            if delta < 0 {
                scroll.scroll_up(delta.unsigned_abs());
            } else if delta > 0 {
                scroll.scroll_down(delta as usize);
            }
        }
    }

    pub(crate) fn handle_key_single_canvas(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && key.code == KeyCode::Char('q') {
            self.interrupt_all_runs();
            self.quit = true;
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            if self.panel.is_some() {
                self.panel = None;
                self.editing = true;
                return;
            }
            if self.palette.is_open() {
                self.palette.close();
                self.editing = true;
                return;
            }
            if self.diff_modal.is_open() {
                self.diff_modal.close();
                self.editing = true;
                return;
            }
            if !self.composer.is_empty() && !self.ctrl_c_armed_active() {
                self.composer.clear();
                self.input.clear();
                self.autocomplete.close();
                self.ctrl_c_armed_at = Some(Instant::now());
                self.notice = "Input cleared · press Ctrl+C again to quit".into();
                return;
            }
            self.interrupt_all_runs();
            self.quit = true;
            return;
        }

        if self.panel.is_some() {
            self.panel_key(key);
            return;
        }

        if self.palette.is_open() {
            match self.palette.handle_key(key) {
                PaletteAction::Executed(id) => self.execute_palette_action(&id),
                PaletteAction::Closed => self.editing = true,
                PaletteAction::None => {}
            }
            return;
        }

        if self.diff_modal.is_open() {
            match self.diff_modal.handle_key(key) {
                DiffModalAction::CopyDiff => {
                    let text = self.diff_modal.diff_text().to_owned();
                    match clipboard::copy_text(&text) {
                        Ok(clipboard::CopyBackend::Osc52Only) => {
                            self.notice = "Copied diff to terminal clipboard (OSC 52).".into();
                        }
                        Ok(_) => self.notice = "Copied diff to clipboard.".into(),
                        Err(err) => self.notice = format!("Clipboard unavailable: {err}"),
                    }
                }
                DiffModalAction::Closed => self.editing = true,
                DiffModalAction::None => {}
            }
            return;
        }

        if self.handle_view_key(key) {
            return;
        }

        if self.leader_armed_active() {
            self.leader_armed_at = None;
            match key.code {
                KeyCode::Char('e') => return self.request_external_editor(),
                KeyCode::Char('d') => return self.open_diff_viewer(None),
                KeyCode::Char('b') => {
                    self.rail_open = !self.rail_open;
                    return;
                }
                KeyCode::Char('c') => return self.clear_active_transcript(),
                KeyCode::Char('m') => return self.select_profile(),
                _ => {}
            }
        }

        if ctrl && !alt {
            match key.code {
                KeyCode::Char('p') => {
                    self.open_command_palette();
                    return;
                }
                KeyCode::Char('k') if self.composer.is_empty() => {
                    self.open_command_palette();
                    return;
                }
                KeyCode::Char('b') => {
                    self.rail_open = !self.rail_open;
                    if !self.rail_open {
                        self.rail_focused = false;
                    }
                    return;
                }
                KeyCode::Char('y') => {
                    self.copy_latest_reply();
                    return;
                }
                KeyCode::Char('x') => {
                    self.leader_armed_at = Some(Instant::now());
                    self.notice = "Leader Ctrl+X: e $EDITOR · d diff · b rail · m profile".into();
                    return;
                }
                KeyCode::Char('l') => {
                    self.clear_active_transcript();
                    return;
                }
                KeyCode::Char('d') => {
                    self.active_scroll_mut().half_page_down();
                    return;
                }
                KeyCode::Char('u') if self.composer.is_empty() => {
                    self.active_scroll_mut().half_page_up();
                    return;
                }
                KeyCode::Char('e')
                    if self.composer.is_empty()
                        || self.composer.cursor() == self.composer.text().len() =>
                {
                    self.toggle_latest_expand();
                    return;
                }
                KeyCode::Char('n') => {
                    self.rail_focused = false;
                    self.editing = true;
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::F(2) => return self.select_profile(),
            KeyCode::F(3) => return self.open_settings(),
            KeyCode::F(4) => return self.abandon_continuation(),
            KeyCode::F(6) => return self.request_update(),
            KeyCode::F(7) => return self.toggle_settled_drawer(),
            KeyCode::PageUp => {
                self.active_scroll_mut().page_up();
                return;
            }
            KeyCode::PageDown => {
                self.active_scroll_mut().page_down();
                return;
            }
            KeyCode::Esc => {
                if self.autocomplete.is_open() {
                    self.autocomplete.close();
                    return;
                }
                if self.rail_focused {
                    self.rail_focused = false;
                    self.editing = true;
                    return;
                }
                if self.mission_running() {
                    if self.esc_armed_active() {
                        self.esc_armed_at = None;
                        self.interrupt_all_runs();
                        self.notice = "Cancelled active mission.".into();
                    } else {
                        self.esc_armed_at = Some(Instant::now());
                        self.notice = "Press Esc again to cancel active turn.".into();
                    }
                    return;
                }
                if self.selected > 0 {
                    self.select_member_thread(0);
                    return;
                }
                self.notice.clear();
                return;
            }
            KeyCode::Tab => {
                if self.autocomplete.is_open() {
                    if self.autocomplete.apply_to(&mut self.composer) {
                        self.input = self.composer.text().to_owned();
                        self.refresh_autocomplete();
                    }
                    return;
                }
                if self.rail_open {
                    self.rail_focused = !self.rail_focused;
                    self.editing = !self.rail_focused;
                    return;
                }
            }
            _ => {}
        }

        if self.rail_focused {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    let prev = self.selected;
                    self.step_selection(false);
                    if self.selected != prev {
                        let target = self.selected;
                        self.selected = prev;
                        self.select_member_thread(target);
                    }
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let prev = self.selected;
                    self.step_selection(true);
                    if self.selected != prev {
                        let target = self.selected;
                        self.selected = prev;
                        self.select_member_thread(target);
                    }
                    return;
                }
                KeyCode::Home => {
                    self.select_member_thread(0);
                    return;
                }
                KeyCode::Enter => {
                    self.rail_focused = false;
                    self.editing = true;
                    return;
                }
                KeyCode::Char('d') => {
                    self.open_diff_viewer(None);
                    return;
                }
                KeyCode::Char('p') => {
                    self.open_pr(None, false);
                    return;
                }
                _ => {
                    self.rail_focused = false;
                    self.editing = true;
                }
            }
        }

        if self.autocomplete.is_open() {
            match key.code {
                KeyCode::Up => {
                    self.autocomplete.move_up();
                    return;
                }
                KeyCode::Down => {
                    self.autocomplete.move_down();
                    return;
                }
                _ => {}
            }
        }

        self.editing = true;
        self.esc_armed_at = None;
        self.ctrl_c_armed_at = None;

        match self.composer.handle_key(key) {
            EditorAction::Submit { text, steer } => {
                self.autocomplete.close();
                self.input.clear();
                self.handle_composer_submit(text, steer);
            }
            EditorAction::OpenExternalEditor => {
                self.request_external_editor();
            }
            EditorAction::RecallQueued(_) => {
                self.input = self.composer.text().to_owned();
                self.refresh_autocomplete();
                self.notice = "Recalled queued message for editing.".into();
            }
            EditorAction::Changed => {
                self.input = self.composer.text().to_owned();
                self.refresh_autocomplete();
            }
            EditorAction::None => {}
        }
    }

    pub(crate) fn handle_composer_submit(&mut self, text: String, steer: bool) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.show_welcome = false;

        if !trimmed.starts_with('/') {
            if let Some((req_id, run_name)) = self.live.as_mut().and_then(|live| {
                let key = live.pending_prod_gates.keys().next()?.clone();
                let val = live.pending_prod_gates.remove(&key)?;
                Some((key, val))
            }) {
                if let Some(live) = &self.live {
                    if let Some(pos) = live.run_position(&run_name) {
                        let _ = live.runs[pos].mission.respond_permission(
                            &req_id,
                            false,
                            Some(trimmed),
                        );
                    }
                }
                self.transcript_for_mut(&run_name)
                    .resolve_prod_gate(&req_id);
                self.transcript_for_mut(&run_name)
                    .push_user(format!("[Steer decline] {trimmed}"), None);
                self.notice = "Declined ProdGate action with steering feedback.".into();
                return;
            }
        }

        if let Some(shell_cmd) = trimmed.strip_prefix('!') {
            self.run_shell_command(shell_cmd.to_owned());
            return;
        }

        if self.selected == 0
            && self.is_running(specialists::COMMANDER)
            && !trimmed.starts_with('/')
        {
            if steer {
                self.composer.push_queued(trimmed.to_owned());
                if let Some(live) = &self.live {
                    if let Some(pos) = live.run_position(specialists::COMMANDER) {
                        live.runs[pos].mission.interrupt();
                    }
                }
                self.notice = "Steering Commander with updated instruction…".into();
            } else {
                self.composer.push_queued(trimmed.to_owned());
                self.notice = "Queued follow-up (Alt+↑ to recall).".into();
            }
            return;
        }

        self.submit_text(trimmed.to_owned());
    }

    pub(crate) fn handle_click_single_canvas(&mut self, x: u16, y: u16) {
        if self.panel.is_some() {
            self.panel_click(x, y);
            return;
        }
        if self.palette.is_open() {
            match self.palette.handle_click(x, y) {
                PaletteAction::Executed(id) => self.execute_palette_action(&id),
                PaletteAction::Closed => self.editing = true,
                PaletteAction::None => {}
            }
            return;
        }
        if self.diff_modal.is_open() {
            if self.diff_modal.handle_click(x, y) == DiffModalAction::Closed {
                self.editing = true;
            }
            return;
        }
        if self.autocomplete.is_open() && self.autocomplete.handle_click(x, y, &mut self.composer) {
            self.input = self.composer.text().to_owned();
            self.refresh_autocomplete();
            return;
        }

        let point = Position::new(x, y);
        if self.settings_area.contains(point) {
            self.open_settings();
            return;
        }
        if self.palette_area.contains(point) {
            self.open_command_palette();
            return;
        }
        if self.update_area.contains(point) {
            self.request_update();
            return;
        }
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.abandon_area.contains(point))
        {
            self.abandon_continuation();
            return;
        }
        if self.profile_area.contains(point) {
            self.select_profile();
            return;
        }
        if let Some((_, hit)) = self
            .focus_banner_hits
            .iter()
            .find(|(rect, _)| rect_contains(*rect, x, y))
            .copied()
        {
            match hit {
                FocusBannerHit::OpenDiff => self.open_diff_viewer(None),
                FocusBannerHit::OpenPr => self.open_pr(None, false),
                FocusBannerHit::BackToCommander => self.select_member_thread(0),
            }
            return;
        }
        if let Some((_, hit)) = self
            .welcome_hits
            .iter()
            .find(|(rect, _)| rect_contains(*rect, x, y))
            .copied()
        {
            match hit {
                WelcomeHit::OpenConnect => self.open_connect(panel::Origin::Direct),
                WelcomeHit::OpenSettings => self.open_settings(),
                WelcomeHit::DismissWelcome => self.show_welcome = false,
            }
            return;
        }
        if let Some((_, hit)) = self
            .left_hits
            .iter()
            .find(|(rect, _)| rect_contains(*rect, x, y))
            .copied()
        {
            self.handle_tab_hit(hit);
            return;
        }
        if let Some((_, hit)) = self
            .rail_hits
            .iter()
            .find(|(rect, _)| rect_contains(*rect, x, y))
            .copied()
        {
            match hit {
                RailHit::ToggleRail => {
                    self.rail_open = !self.rail_open;
                    self.rail_focused = false;
                    self.editing = true;
                }
                RailHit::SelectMember(idx) => {
                    self.select_member_thread(idx);
                    self.rail_focused = false;
                    self.editing = true;
                }
                RailHit::ToggleSettled => self.toggle_settled_drawer(),
                RailHit::OpenDiff => self.open_diff_viewer(None),
                RailHit::OpenPr => self.open_pr(None, false),
                RailHit::OpenSettings => self.open_settings(),
                RailHit::OpenConnect(provider) => {
                    match self.live.as_ref().and_then(|live| {
                        live.profiles
                            .iter()
                            .position(|profile| profile.provider == provider)
                    }) {
                        Some(index) => self.open_subscription(index, false),
                        None => self.open_settings(),
                    }
                }
            }
            return;
        }
        if self.input_area.contains(point) {
            self.rail_focused = false;
            self.editing = true;
            return;
        }

        if self.view_tab != crate::tui::tabs::ViewTab::Chat {
            return;
        }
        let name = self.crew[self.selected].name.clone();
        let hit_action = if name == specialists::COMMANDER {
            self.scroll.hit_test(x, y).cloned()
        } else {
            self.specialist_scrolls
                .get(&name)
                .and_then(|s| s.hit_test(x, y))
                .cloned()
        };
        if let Some(action) = hit_action {
            self.handle_transcript_hit(&name, action);
        }
    }

    fn handle_transcript_hit(&mut self, thread_name: &str, action: TranscriptHitAction) {
        match action {
            TranscriptHitAction::ToggleExpand { entry_index } => {
                self.transcript_for_mut(thread_name)
                    .toggle_expand_at_or_latest(Some(entry_index));
            }
            TranscriptHitAction::Connect {
                card_id,
                provider_or_tool,
            } => {
                self.transcript_for_mut(thread_name)
                    .resolve_connect_card(&card_id);
                if provider_or_tool.eq_ignore_ascii_case("github") {
                    self.notice = "Run `gh auth login` or open Settings (F3).".into();
                }
                self.open_connect(panel::Origin::Direct);
            }
            TranscriptHitAction::DismissConnect { card_id } => {
                self.transcript_for_mut(thread_name)
                    .resolve_connect_card(&card_id);
                self.notice = "Dismissed connection prompt.".into();
            }
            TranscriptHitAction::AllowProdGate { req_id } => {
                self.resolve_prod_gate(&req_id, thread_name, true);
            }
            TranscriptHitAction::DenyProdGate { req_id } => {
                self.resolve_prod_gate(&req_id, thread_name, false);
            }
            TranscriptHitAction::SelectDelegation { worker } => {
                if let Some(idx) = specialists::find(&self.crew, &worker) {
                    self.select_member_thread(idx);
                }
            }
        }
    }

    pub(crate) fn append_transcript_assistant_chunk(
        &mut self,
        thread_name: &str,
        speaker: &str,
        chunk: &str,
    ) {
        let transcript = self.transcript_for_mut(thread_name);
        if let Some(TranscriptEntry::AssistantMarkdown {
            speaker: current_speaker,
            markdown,
            ..
        }) = transcript.entries.last_mut()
        {
            if current_speaker == speaker {
                if !markdown.is_empty() && !markdown.ends_with('\n') {
                    markdown.push('\n');
                }
                markdown.push_str(chunk);
                return;
            }
        }
        transcript.push(TranscriptEntry::AssistantMarkdown {
            speaker: speaker.to_owned(),
            markdown: chunk.to_owned(),
            streaming: false,
        });
    }

    pub(crate) fn push_transcript_log_entry(
        &mut self,
        thread_name: &str,
        text: &str,
        at: Option<String>,
    ) {
        let (source, body) = text.split_once("  ").unwrap_or(("SYSTEM", text));
        let clean = body.trim();
        match source {
            "YOU" | "TASK" => {
                self.transcript_for_mut(thread_name)
                    .push(TranscriptEntry::UserMessage {
                        text: clean.to_owned(),
                        timestamp: at,
                    });
            }
            "SYSTEM" | "SNOOZED" => {
                let notice = format_routing_notice(clean);
                self.transcript_for_mut(thread_name)
                    .push(TranscriptEntry::SystemNotice {
                        level: if notice.contains("failed") || notice.contains("ERROR") {
                            NoticeLevel::Error
                        } else {
                            NoticeLevel::Info
                        },
                        text: notice,
                    });
            }
            other => {
                let transcript = self.transcript_for_mut(thread_name);
                if let Some(TranscriptEntry::AssistantMarkdown {
                    speaker,
                    markdown,
                    streaming: false,
                }) = transcript.entries.last_mut()
                {
                    if speaker == other {
                        if !markdown.is_empty() {
                            let prev_line = markdown.lines().last().unwrap_or("");
                            if legacy_line_needs_blank_separator(prev_line, clean) {
                                markdown.push_str("\n\n");
                            } else {
                                markdown.push('\n');
                            }
                        }
                        markdown.push_str(clean);
                        return;
                    }
                }
                transcript.push(TranscriptEntry::AssistantMarkdown {
                    speaker: other.to_owned(),
                    markdown: clean.to_owned(),
                    streaming: false,
                });
            }
        }
    }

    pub(crate) fn build_commander_prompt_text(&self, unit: &str) -> String {
        let Some(live) = &self.live else {
            return specialists::commander_prompt(
                self.log.iter().map(String::as_str),
                &self.crew,
                unit,
            );
        };
        let soul_text = live
            .soul_store
            .as_ref()
            .and_then(|store| store.read().ok())
            .unwrap_or_default();
        let gh_ctx = crate::github::gh_prompt_context(&self.github_status);
        let workers = specialists::roster_summary(&self.crew);
        let tools_summary = "spawn_worker, message_worker, read_worker, list_workers, interrupt_worker, ask_user, remember, search_memory, github_pr_*, github_issue_*";
        let system = crate::soul::commander_system_prompt(
            &soul_text,
            &live.workspace,
            gh_ctx.as_deref(),
            Some(&workers),
            Some(tools_summary),
        );
        let mut sections = vec![system];
        if !live.recent_shell_context.is_empty() {
            let shell_block = live
                .recent_shell_context
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n");
            sections.push(format!("Recent local shell output:\n{shell_block}"));
        }
        sections.push(format!("Operator: {unit}"));
        sections.join("\n\n")
    }

    pub(crate) fn refresh_usage_and_sync(&mut self) {
        if let Some(live) = &mut self.live {
            live.refresh_usage();
        }
        let running = self.is_running(specialists::COMMANDER);
        self.composer.set_running(running);
        if let Some(live) = &self.live {
            let idx = live.displayed_profile();
            if let Some(allowance) = live.allowances.get(idx) {
                let _ = (allowance.five, allowance.week, usage::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn app_with_background_channel(last_check: Option<Instant>) -> App {
        let (tx, rx) = mpsc::channel();
        App {
            bg_tx: Some(tx),
            bg_rx: Some(rx),
            last_update_check: last_check,
            ..App::default()
        }
    }

    #[test]
    fn periodic_update_poll_due_only_after_interval_with_background_channel() {
        assert_eq!(UPDATE_POLL_INTERVAL, Duration::from_secs(10 * 60));

        let t0 = Instant::now();
        let before_due = t0 + UPDATE_POLL_INTERVAL - Duration::from_secs(1);
        let at_due = t0 + UPDATE_POLL_INTERVAL;

        let mut without_bg = App {
            last_update_check: Some(t0),
            ..App::default()
        };
        assert!(!without_bg.periodic_update_check_due_at(at_due));
        assert!(!without_bg
            .begin_periodic_update_check(at_due, |_| { panic!("must not spawn without bg_tx") }));

        let mut uninitialized_timer = app_with_background_channel(None);
        assert!(!uninitialized_timer.periodic_update_check_due_at(at_due));
        assert!(
            !uninitialized_timer.begin_periodic_update_check(at_due, |_| {
                panic!("must not spawn without last_update_check")
            })
        );

        let mut app = app_with_background_channel(Some(t0));
        app.notice = "Commander replied.".into();
        assert!(!app.periodic_update_check_due_at(t0));
        assert!(!app.periodic_update_check_due_at(before_due));
        assert!(!app.begin_periodic_update_check(before_due, |_| {
            panic!("must not spawn before interval elapses")
        }));

        assert!(app.periodic_update_check_due_at(at_due));
        let mut spawned = 0usize;
        let started = app.begin_periodic_update_check(at_due, |tx| {
            spawned += 1;
            tx.send(AppEvent::UpdateChecked(UpdateState::UpToDate {
                current: crate::update::CURRENT.to_owned(),
            }))
            .unwrap();
        });
        assert!(started);
        assert_eq!(spawned, 1);
        assert!(app.update_check_in_flight);
        assert!(!app.manual_update_check);
        assert_eq!(app.last_update_check, Some(at_due));
        assert_eq!(app.update_state, UpdateState::Checking);
        assert_eq!(app.notice, "Commander replied.");
        assert!(!app.periodic_update_check_due_at(at_due + UPDATE_POLL_INTERVAL));

        app.poll_background_events();
        assert!(!app.update_check_in_flight);
        assert_eq!(
            app.update_state,
            UpdateState::UpToDate {
                current: crate::update::CURRENT.to_owned()
            }
        );
        assert_eq!(app.notice, "Commander replied.");
        assert!(!app
            .periodic_update_check_due_at(at_due + UPDATE_POLL_INTERVAL - Duration::from_secs(1)));
        assert!(app.periodic_update_check_due_at(at_due + UPDATE_POLL_INTERVAL));
    }

    #[test]
    fn periodic_update_poll_respects_settings_env_and_debug_opt_outs() {
        let t0 = Instant::now();
        let at_due = t0 + UPDATE_POLL_INTERVAL;

        let mut app = app_with_background_channel(Some(t0));
        assert!(app.launch_update_check());
        assert_eq!(
            app.automatic_update_check_enabled(),
            !cfg!(debug_assertions)
        );
        if cfg!(debug_assertions) {
            assert!(!app.maybe_poll_periodic_update(at_due));
            assert!(!app.update_check_in_flight);
        }

        app.settings.check_for_updates = false;
        assert!(!app.launch_update_check());
        assert!(!app.automatic_update_check_enabled());
        assert!(!app.periodic_update_check_due_at(at_due));
        assert!(!app.begin_periodic_update_check(at_due, |_| {
            panic!("must not spawn when check_for_updates is false")
        }));

        app.settings.check_for_updates = true;
        app.launch.no_update_check = Some(OsString::from("1"));
        assert!(!app.launch_update_check());
        assert!(!app.automatic_update_check_enabled());
        assert!(!app.periodic_update_check_due_at(at_due));
        assert!(!app.begin_periodic_update_check(at_due, |_| {
            panic!("must not spawn when AGENT_COMMAND_NO_UPDATE_CHECK=1")
        }));
    }

    #[test]
    fn periodic_update_poll_guards_in_flight_checking_downloading_and_legacy_receiver() {
        let t0 = Instant::now();
        let at_due = t0 + UPDATE_POLL_INTERVAL;

        let mut in_flight = app_with_background_channel(Some(t0));
        in_flight.update_check_in_flight = true;
        assert!(!in_flight.periodic_update_check_due_at(at_due));
        assert!(!in_flight
            .begin_periodic_update_check(at_due, |_| { panic!("must not spawn while in flight") }));

        let mut checking = app_with_background_channel(Some(t0));
        checking.update_state = UpdateState::Checking;
        assert!(!checking.periodic_update_check_due_at(at_due));
        assert!(!checking
            .begin_periodic_update_check(at_due, |_| { panic!("must not spawn while Checking") }));

        let mut downloading = app_with_background_channel(Some(t0));
        downloading.update_state = UpdateState::Downloading { pct: 40 };
        assert!(!downloading.periodic_update_check_due_at(at_due));
        assert!(!downloading.begin_periodic_update_check(at_due, |_| {
            panic!("must not spawn while Downloading")
        }));

        let mut legacy = app_with_background_channel(Some(t0));
        let (_sender, receiver) = mpsc::channel();
        legacy.watch_updates(Some(receiver));
        legacy.update_state = UpdateState::Idle;
        assert!(!legacy.periodic_update_check_due_at(at_due));
        assert!(!legacy.begin_periodic_update_check(at_due, |_| {
            panic!("must not spawn while legacy update_receiver is active")
        }));
    }

    #[test]
    fn manual_update_check_resets_periodic_timer_and_prevents_overlap() {
        let t0 = Instant::now();
        let manual_at = t0 + Duration::from_secs(9 * 60);
        let retry_at = manual_at + Duration::from_secs(1);

        let mut app = app_with_background_channel(Some(t0));
        let mut spawns = 0usize;
        app.trigger_live_update_check_with(manual_at, |tx| {
            spawns += 1;
            tx.send(AppEvent::UpdateChecked(UpdateState::UpToDate {
                current: crate::update::CURRENT.to_owned(),
            }))
            .unwrap();
        });
        assert_eq!(spawns, 1);
        assert!(app.manual_update_check);
        assert!(app.update_check_in_flight);
        assert_eq!(app.last_update_check, Some(manual_at));
        assert_eq!(app.update_state, UpdateState::Checking);
        assert_eq!(app.notice, "Checking GitHub releases…");

        app.trigger_live_update_check_with(retry_at, |_| {
            spawns += 1;
        });
        assert_eq!(spawns, 1);
        assert_eq!(app.last_update_check, Some(retry_at));

        app.poll_background_events();
        assert!(!app.update_check_in_flight);
        assert!(!app.manual_update_check);
        assert_eq!(
            app.notice,
            format!("Up to date (v{})", crate::update::CURRENT)
        );
        assert!(!app.periodic_update_check_due_at(t0 + UPDATE_POLL_INTERVAL));
        assert!(!app.periodic_update_check_due_at(
            retry_at + UPDATE_POLL_INTERVAL - Duration::from_secs(1)
        ));
        assert!(app.periodic_update_check_due_at(retry_at + UPDATE_POLL_INTERVAL));

        app.update_state = UpdateState::Downloading { pct: 25 };
        app.notice = "Downloading update…".into();
        app.trigger_live_update_check_with(retry_at + UPDATE_POLL_INTERVAL, |_| {
            spawns += 1;
        });
        assert_eq!(spawns, 1);
        assert_eq!(app.update_state, UpdateState::Downloading { pct: 25 });
        assert_eq!(app.notice, "Downloading update…");
    }

    #[test]
    fn silent_periodic_available_updates_button_without_flicker_or_notice_clobber() {
        let t0 = Instant::now();
        let t1 = t0 + UPDATE_POLL_INTERVAL;
        let t2 = t1 + UPDATE_POLL_INTERVAL;

        let mut app = app_with_background_channel(Some(t0));
        app.update_state = UpdateState::UpToDate {
            current: crate::update::CURRENT.to_owned(),
        };
        app.notice = "Commander replied.".into();

        assert!(!app.update_button_visible());
        assert_eq!(app.update_button_label(120), None);

        let started = app.begin_periodic_update_check(t1, |tx| {
            tx.send(AppEvent::UpdateChecked(UpdateState::Available {
                tag: "v0.9.0".into(),
                current: crate::update::CURRENT.to_owned(),
            }))
            .unwrap();
        });
        assert!(started);
        assert_eq!(app.notice, "Commander replied.");

        app.poll_background_events();
        assert!(!app.update_check_in_flight);
        assert_eq!(app.update_available.as_deref(), Some("0.9.0"));
        assert!(app.update_button_visible());
        assert_eq!(
            app.update_button_label(80).as_deref(),
            Some("⬆ Update v0.9.0")
        );
        assert_eq!(
            app.update_button_label(120).as_deref(),
            Some("⬆ Update v0.9.0 (F6)")
        );
        assert_eq!(app.notice, "Commander replied.");

        let recheck_started = app.begin_periodic_update_check(t2, |tx| {
            tx.send(AppEvent::UpdateChecked(UpdateState::Available {
                tag: "v0.10.0".into(),
                current: crate::update::CURRENT.to_owned(),
            }))
            .unwrap();
        });
        assert!(recheck_started);
        assert!(app.update_check_in_flight);
        assert_eq!(
            app.update_state,
            UpdateState::Available {
                tag: "v0.9.0".into(),
                current: crate::update::CURRENT.to_owned(),
            }
        );
        assert!(app.update_button_visible());
        assert_eq!(
            app.update_button_label(120).as_deref(),
            Some("⬆ Update v0.9.0 (F6)")
        );
        assert_eq!(app.notice, "Commander replied.");

        app.poll_background_events();
        assert!(!app.update_check_in_flight);
        assert_eq!(app.update_available.as_deref(), Some("0.10.0"));
        assert!(app.update_button_visible());
        assert_eq!(
            app.update_button_label(120).as_deref(),
            Some("⬆ Update v0.10.0 (F6)")
        );
        assert_eq!(app.notice, "Commander replied.");
    }

    #[test]
    fn silent_failed_check_preserves_existing_available_and_notice() {
        let mut app = App {
            update_available: Some("0.9.0".into()),
            update_state: UpdateState::Available {
                tag: "v0.9.0".into(),
                current: crate::update::CURRENT.to_owned(),
            },
            update_check_in_flight: true,
            manual_update_check: false,
            notice: "Keep existing notice".into(),
            ..App::default()
        };

        app.apply_background_event(AppEvent::UpdateChecked(UpdateState::Failed(
            "temporarily offline".into(),
        )));

        assert!(!app.update_check_in_flight);
        assert_eq!(app.update_available.as_deref(), Some("0.9.0"));
        assert_eq!(
            app.update_state,
            UpdateState::Available {
                tag: "v0.9.0".into(),
                current: crate::update::CURRENT.to_owned(),
            }
        );
        assert!(app.update_button_visible());
        assert_eq!(
            app.update_button_label(120).as_deref(),
            Some("⬆ Update v0.9.0 (F6)")
        );
        assert_eq!(app.notice, "Keep existing notice");

        let mut no_prior = App {
            update_state: UpdateState::Checking,
            update_check_in_flight: true,
            manual_update_check: false,
            notice: "Keep existing notice".into(),
            ..App::default()
        };
        no_prior.apply_background_event(AppEvent::UpdateChecked(UpdateState::Failed(
            "offline".into(),
        )));
        assert!(!no_prior.update_check_in_flight);
        assert!(no_prior.update_available.is_none());
        assert_eq!(no_prior.update_state, UpdateState::Failed("offline".into()));
        assert!(!no_prior.update_button_visible());
        assert_eq!(no_prior.notice, "Keep existing notice");
    }

    #[test]
    fn late_update_checked_and_legacy_poll_never_clobber_downloading() {
        for late_state in [
            UpdateState::UpToDate {
                current: crate::update::CURRENT.to_owned(),
            },
            UpdateState::Available {
                tag: "v0.9.1".into(),
                current: crate::update::CURRENT.to_owned(),
            },
            UpdateState::Failed("network reset".into()),
        ] {
            let mut app = App {
                update_available: Some("0.9.0".into()),
                update_state: UpdateState::Downloading { pct: 60 },
                update_check_in_flight: true,
                manual_update_check: true,
                notice: "Updating…".into(),
                ..App::default()
            };
            app.apply_background_event(AppEvent::UpdateChecked(late_state));
            assert!(!app.update_check_in_flight);
            assert!(!app.manual_update_check);
            assert_eq!(app.update_state, UpdateState::Downloading { pct: 60 });
            assert_eq!(app.update_available.as_deref(), Some("0.9.0"));
            assert_eq!(
                app.update_button_label(120).as_deref(),
                Some("⬇ Updating 60%")
            );
            assert_eq!(app.notice, "Updating…");
        }

        let mut app = App {
            update_available: Some("0.9.0".into()),
            update_state: UpdateState::Downloading { pct: 60 },
            ..App::default()
        };
        let (sender, receiver) = mpsc::channel();
        app.watch_updates(Some(receiver));
        sender.send("9.9.9".into()).unwrap();
        app.poll_update();
        assert_eq!(app.update_state, UpdateState::Downloading { pct: 60 });
        assert_eq!(app.update_available.as_deref(), Some("0.9.0"));
        assert!(app.update_receiver.is_none());

        let mut in_flight_app = App {
            update_state: UpdateState::Checking,
            update_check_in_flight: true,
            ..App::default()
        };
        let (sender, receiver) = mpsc::channel::<String>();
        in_flight_app.watch_updates(Some(receiver));
        drop(sender);
        in_flight_app.poll_update();
        assert_eq!(in_flight_app.update_state, UpdateState::Checking);
        assert!(in_flight_app.update_receiver.is_none());
    }

    #[test]
    fn provider_stderr_cards_are_scoped_per_turn_and_never_glue_across_turns() {
        let mut app = App::default();
        app.transcript.push(TranscriptEntry::ToolCallCard {
            call_id: "stderr-Commander".into(),
            kind: ToolKind::Other,
            name: "Provider stderr".into(),
            summary: "1 line · Ctrl+E to expand".into(),
            output: "legacy prior session stderr".into(),
            ok: Some(true),
            exit_code: None,
            duration_ms: None,
            collapsed: true,
        });

        app.transcript.push_user("first turn", None);
        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::StderrLine("18:06 claude warning".into()),
        );
        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: false,
                error_code: None,
                final_text: String::new(),
            },
        );

        app.transcript.push_user("second turn", None);
        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::StderrLine("19:15 gemini warning".into()),
        );
        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::StderrLine("19:15 gemini fatal".into()),
        );
        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: true,
                error_code: Some(HealthCode::AgentFailed),
                final_text: "gemini failed".into(),
            },
        );

        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::StderrLine("third turn without user prompt".into()),
        );
        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: false,
                error_code: None,
                final_text: String::new(),
            },
        );

        app.transcript.push_user("fourth turn with no stderr", None);
        app.handle_normalized_event(
            specialists::COMMANDER,
            NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: true,
                error_code: Some(HealthCode::AgentFailed),
                final_text: "spawn failed".into(),
            },
        );

        let cards: Vec<(&str, &str, Option<bool>, bool)> = app
            .transcript
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::ToolCallCard {
                    call_id,
                    name,
                    output,
                    ok,
                    collapsed,
                    ..
                } if name == "Provider stderr" => {
                    Some((call_id.as_str(), output.as_str(), *ok, *collapsed))
                }
                _ => None,
            })
            .collect();

        assert_eq!(cards.len(), 4);
        assert_eq!(
            cards[0],
            (
                "stderr-Commander",
                "legacy prior session stderr",
                Some(true),
                true
            )
        );
        assert_eq!(
            cards[1],
            (
                "stderr-Commander-turn-1-done",
                "18:06 claude warning",
                Some(true),
                true
            )
        );
        assert_eq!(
            cards[2],
            (
                "stderr-Commander-turn-2-done",
                "19:15 gemini warning\n19:15 gemini fatal",
                Some(false),
                false
            )
        );
        assert_eq!(
            cards[3],
            (
                "stderr-Commander-turn-3-done",
                "third turn without user prompt",
                Some(true),
                true
            )
        );
    }

    #[test]
    fn failed_commander_turn_emits_single_error_notice_across_turn_finished_and_event_finished() {
        let workspace = std::env::temp_dir().join(format!(
            "agent-command-stderr-dedup-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();

        let (mut mission, event_tx) = crate::mission::Mission::held();
        let (norm_tx, norm_rx) = mpsc::channel();
        mission.normalized = norm_rx;
        let mut live = crate::Live::new(workspace.clone(), Vec::new(), None);
        live.runs
            .push(crate::Run::new(specialists::COMMANDER, mission, false));

        let mut app = App {
            live: Some(live),
            ..App::default()
        };
        app.transcript.push_user("run task", None);

        let detail = "Provider exited with exit code: 1: client unsupported";
        norm_tx
            .send(NormalizedEvent::StderrLine("client unsupported".into()))
            .unwrap();
        norm_tx
            .send(NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: true,
                error_code: Some(HealthCode::AgentFailed),
                final_text: detail.into(),
            })
            .unwrap();
        event_tx
            .send(crate::mission::Event::Finished(Err(std::io::Error::other(
                detail,
            ))))
            .unwrap();

        app.poll_mission();

        let error_notices: Vec<&str> = app
            .transcript
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::SystemNotice {
                    level: NoticeLevel::Error,
                    text,
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(error_notices, vec![format!("Commander: {detail}")]);

        let stderr_cards: Vec<(&str, Option<bool>, bool)> = app
            .transcript
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::ToolCallCard {
                    call_id,
                    name,
                    ok,
                    collapsed,
                    ..
                } if name == "Provider stderr" => Some((call_id.as_str(), *ok, *collapsed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            stderr_cards,
            vec![("stderr-Commander-turn-1-done", Some(false), false)]
        );

        let (mut ok_mission, ok_event_tx) = crate::mission::Mission::held();
        let (ok_norm_tx, ok_norm_rx) = mpsc::channel();
        ok_mission.normalized = ok_norm_rx;
        app.live.as_mut().unwrap().runs.push(crate::Run::new(
            specialists::COMMANDER,
            ok_mission,
            false,
        ));
        app.transcript.push_user("exit zero error", None);
        ok_norm_tx
            .send(NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: true,
                error_code: Some(HealthCode::AgentFailed),
                final_text: "API error on exit 0".into(),
            })
            .unwrap();
        ok_event_tx
            .send(crate::mission::Event::Finished(Ok(())))
            .unwrap();
        app.poll_mission();

        let error_notices_after_ok: Vec<&str> = app
            .transcript
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::SystemNotice {
                    level: NoticeLevel::Error,
                    text,
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            error_notices_after_ok,
            vec![
                format!("Commander: {detail}"),
                "API error on exit 0".to_owned()
            ]
        );

        let (mut auth_mission, auth_event_tx) = crate::mission::Mission::held();
        let (auth_norm_tx, auth_norm_rx) = mpsc::channel();
        auth_mission.normalized = auth_norm_rx;
        app.live.as_mut().unwrap().runs.push(crate::Run::new(
            specialists::COMMANDER,
            auth_mission,
            false,
        ));
        app.transcript.push_user("auth failure turn", None);
        auth_norm_tx
            .send(NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: true,
                error_code: Some(HealthCode::AgentAuth),
                final_text: "reauth required".into(),
            })
            .unwrap();
        auth_event_tx
            .send(crate::mission::Event::Finished(Err(std::io::Error::other(
                "reauth required",
            ))))
            .unwrap();
        app.poll_mission();

        let connect_cards = app
            .transcript
            .entries
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::InlineConnectCard { .. }))
            .count();
        assert_eq!(connect_cards, 1);

        app.handle_normalized_event(
            "builder",
            NormalizedEvent::TurnFinished {
                interrupted: false,
                is_error: true,
                error_code: Some(HealthCode::AgentFailed),
                final_text: "specialist error".into(),
            },
        );
        let specialist_errors: Vec<&str> = app
            .transcript_for_mut("builder")
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::SystemNotice {
                    level: NoticeLevel::Error,
                    text,
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(specialist_errors, vec!["specialist error"]);

        let _ = std::fs::remove_dir_all(&workspace);
    }
}
