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
const MAX_SHELL_CONTEXT_SNIPPETS: usize = 4;
const MAX_SHELL_SNIPPET_CHARS: usize = 2000;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
    if unscored {
        if let Some(first) = parts.first_mut() {
            if let Some(stripped) = first.strip_suffix(" provider default") {
                *first = stripped.to_owned();
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
        if let Some((five, week)) = live.live_quota {
            let mut parts = Vec::new();
            if let Some(f) = five {
                parts.push(format!("5h {:.0}%", (100.0 - f).clamp(0.0, 100.0)));
            }
            if let Some(w) = week {
                parts.push(format!("7d {:.0}%", (100.0 - w).clamp(0.0, 100.0)));
            }
            if !parts.is_empty() {
                return Some(parts.join(" "));
            }
        }
        let idx = live.displayed_profile();
        let allowance = live.allowances.get(idx)?;
        let mut parts = Vec::new();
        if let Some(five) = allowance.five {
            parts.push(format!("5h {:.0}%", five.clamp(0.0, 100.0)));
        }
        if let Some(week) = allowance.week {
            parts.push(format!("7d {:.0}%", week.clamp(0.0, 100.0)));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    pub fn footer_mode_label(&self) -> String {
        let mode = self.settings.commander_permission.flag();
        if self.settings.prod_gate {
            format!("{mode} · ProdGate on")
        } else {
            format!("{mode} · ProdGate off")
        }
    }

    pub fn update_pill_text(&self, bar_width: u16) -> String {
        if let UpdateState::Downloading { pct } = &self.update_state {
            return format!("⬇ Updating {pct}%");
        }
        if let Some(latest) = self.update_available.as_deref() {
            return if bar_width >= 100 {
                format!("⬆ Update v{latest} (F6)")
            } else {
                format!("⬆ Update v{latest}")
            };
        }
        match &self.update_state {
            UpdateState::Available { tag, .. } => {
                let ver = tag.trim_start_matches('v');
                if bar_width >= 100 {
                    format!("⬆ Update v{ver} (F6)")
                } else {
                    format!("⬆ Update v{ver}")
                }
            }
            UpdateState::Idle | UpdateState::UpToDate { .. } if bar_width < 75 => {
                "↻ Update".to_owned()
            }
            other => other.pill_label(),
        }
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
                let det = self
                    .auth_detections
                    .iter()
                    .find(|d| d.provider == provider && d.logged_in);
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
        let check_updates = self.launch_update_check();
        if check_updates {
            self.update_state = UpdateState::Checking;
        }
        events::spawn_startup_probes(workspace, profile_root, check_updates, tx);
    }

    pub(crate) fn trigger_live_update_check(&mut self) {
        self.manual_update_check = true;
        self.update_state = UpdateState::Checking;
        self.notice = "Checking GitHub releases…".into();
        if let Some(tx) = &self.bg_tx {
            events::spawn_update_check(tx.clone());
        } else {
            let (tx, rx) = mpsc::channel();
            self.bg_tx = Some(tx.clone());
            self.bg_rx = Some(rx);
            events::spawn_update_check(tx);
        }
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
    }

    pub(crate) fn apply_background_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::AuthDetected {
                detections,
                profiles,
            } => {
                self.auth_detections = detections;
                if let Some(live) = &mut self.live {
                    if !profiles.is_empty() {
                        live.reset_profiles(profiles);
                    }
                    live.signed_in = live
                        .profiles
                        .iter()
                        .map(|profile| {
                            if let Some(host) = &self.host {
                                host.signed_in(profile)
                            } else {
                                profile.detect_passive().logged_in
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
                if let Some(ver) = state.available_version() {
                    self.update_available = Some(ver.to_owned());
                } else {
                    self.update_available = None;
                }
                if std::mem::take(&mut self.manual_update_check) {
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
            let config = bridge::BridgeSessionConfig {
                workspace_root: live.workspace.clone(),
                profile_root: root,
                soul,
                catalog: bridge::ExternalToolCatalog::new(),
                ask_user: bridge::AskUserBroker::new(false),
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
            NormalizedEvent::SessionStarted { session_id, model } => {
                if is_commander {
                    if let Some(live) = &mut self.live {
                        live.commander_session_id = Some(session_id);
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
            NormalizedEvent::RateLimit {
                five_hour_pct,
                seven_day_pct,
                ..
            } => {
                if let Some(live) = &mut self.live {
                    live.live_quota = Some((five_hour_pct, seven_day_pct));
                }
            }
            NormalizedEvent::UsageDelta { .. } => {}
            NormalizedEvent::StderrLine(line) => {
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
                    self.transcript_for_mut(name)
                        .push(TranscriptEntry::SystemNotice {
                            level: NoticeLevel::Warning,
                            text: "Turn interrupted.".into(),
                        });
                } else if is_error {
                    self.mark_provider_stderr_failed(name);
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
                        self.transcript_for_mut(name)
                            .push(TranscriptEntry::SystemNotice {
                                level: NoticeLevel::Error,
                                text: final_text,
                            });
                    }
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
            self.rail_open,
            self.update_available.as_deref(),
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
        let mut had_runs = false;
        if let Some(live) = &mut self.live {
            let cancelled = std::mem::take(&mut live.runs);
            had_runs = !cancelled.is_empty();
            for run in cancelled {
                run.mission.interrupt();
                drop(run);
            }
            live.running_profile = None;
            live.pending_prod_gates.clear();
            live.queue.clear();
        }
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
        let call_id = format!("stderr-{name}");
        let transcript = self.transcript_for_mut(name);
        if let Some(TranscriptEntry::ToolCallCard {
            summary, output, ..
        }) = transcript.entries.iter_mut().rev().find(|entry| {
            matches!(
                entry,
                TranscriptEntry::ToolCallCard { call_id: id, .. } if id == &call_id
            )
        }) {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(trimmed);
            let count = output.lines().count();
            let noun = if count == 1 { "line" } else { "lines" };
            *summary = format!("{count} {noun} · Ctrl+E to expand");
            return;
        }
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

    pub(crate) fn mark_provider_stderr_failed(&mut self, name: &str) {
        let call_id = format!("stderr-{name}");
        let transcript = self.transcript_for_mut(name);
        if let Some(TranscriptEntry::ToolCallCard { ok, collapsed, .. }) =
            transcript.entries.iter_mut().rev().find(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::ToolCallCard { call_id: id, .. } if id == &call_id
                )
            })
        {
            *ok = Some(false);
            *collapsed = false;
        }
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
                RailHit::OpenConnect(_) => self.open_settings(),
            }
            return;
        }
        if self.input_area.contains(point) {
            self.rail_focused = false;
            self.editing = true;
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
                let run_name = self
                    .live
                    .as_mut()
                    .and_then(|live| live.pending_prod_gates.remove(&req_id))
                    .unwrap_or_else(|| thread_name.to_owned());
                if let Some(live) = &self.live {
                    if let Some(pos) = live.run_position(&run_name) {
                        let _ = live.runs[pos]
                            .mission
                            .respond_permission(&req_id, true, None);
                    }
                }
                self.transcript_for_mut(&run_name)
                    .resolve_prod_gate(&req_id);
                self.notice = "Allowed ProdGate action.".into();
            }
            TranscriptHitAction::DenyProdGate { req_id } => {
                let run_name = self
                    .live
                    .as_mut()
                    .and_then(|live| live.pending_prod_gates.remove(&req_id))
                    .unwrap_or_else(|| thread_name.to_owned());
                if let Some(live) = &self.live {
                    if let Some(pos) = live.run_position(&run_name) {
                        let _ = live.runs[pos].mission.respond_permission(
                            &req_id,
                            false,
                            Some("User denied destructive action via ProdGate."),
                        );
                    }
                }
                self.transcript_for_mut(&run_name)
                    .resolve_prod_gate(&req_id);
                self.notice = "Denied ProdGate action.".into();
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
        let switched_provider = live.commander_provider.is_some()
            && live
                .profiles
                .get(live.displayed_profile())
                .is_some_and(|p| Some(p.provider) != live.commander_provider);
        if switched_provider && !live.handoff_turns.is_empty() {
            let handoff = crate::soul::build_portable_handoff(
                &live.handoff_turns,
                crate::soul::DEFAULT_HANDOFF_TOKENS,
            );
            sections.push(handoff);
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
