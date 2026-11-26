#[cfg(unix)]
#[path = "mission/unix.rs"]
mod process_tree;
#[cfg(windows)]
#[path = "mission/windows.rs"]
mod process_tree;

use crate::{
    auth::{Profile, Provider},
    catalog::Catalog,
    checkpoint::{Checkpoint, Claim, Handoff, Progress},
    native::Pins,
    permissions::{self, PermissionEffect, PermissionMode, PermissionPolicy},
    routing::{self, Candidate, Configuration, Request},
    targeting::Target,
    usage::{self, Usage},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc, Mutex, PoisonError,
    },
    thread,
    time::Duration,
};

const SNOOZE_POLL: Duration = Duration::from_millis(200);
const CANCEL_POLL: Duration = Duration::from_millis(15);
const STDERR_TAIL_LIMIT: usize = 20;
const OUTPUT_PREVIEW_CHARS: usize = 400;
const MAX_PARTIAL_TOOL_JSON_BYTES: usize = 256 * 1024;
const NO_ALTERNATE: &str =
    "Usage limit reached; mission stopped without replay. No eligible signed-in alternate.";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Shell,
    Read,
    Edit,
    Write,
    Search,
    Web,
    Mcp,
    Subagent,
    Todo,
    Generic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchKind {
    Add,
    Update,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthCode {
    AgentMissing,
    AgentBlocked,
    AgentAuth,
    AgentFailed,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NormalizedEvent {
    SessionStarted {
        session_id: String,
        model: Option<String>,
        profile: Option<String>,
    },
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart {
        call_id: String,
        kind: ToolKind,
        name: String,
        summary: String,
        raw_input: Value,
    },
    ToolEnd {
        call_id: String,
        ok: bool,
        output_preview: String,
        exit_code: Option<i32>,
    },
    FileChanged {
        path: String,
        kind: PatchKind,
    },
    PermissionAsked {
        req_id: String,
        tool: String,
        input: Value,
        prod_gate_reason: Option<String>,
    },
    UsageDelta {
        input: u64,
        cached_read: u64,
        output: u64,
        cost_usd: Option<f64>,
        context_used: Option<u64>,
    },
    RateLimit {
        five_hour_pct: Option<f32>,
        seven_day_pct: Option<f32>,
        resets_at: Option<String>,
    },
    StderrLine(String),
    TurnFinished {
        interrupted: bool,
        is_error: bool,
        error_code: Option<HealthCode>,
        final_text: String,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaunchOptions {
    pub resume_session: Option<String>,
    pub resume_session_profile: Option<String>,
    pub portable_handoff: Option<String>,
    pub mcp_config: Option<PathBuf>,
    pub extra_args: Vec<String>,
    pub policy: PermissionPolicy,
    pub persistent_stdio: bool,
}

pub(crate) enum Event {
    Running {
        profile: usize,
        reason: Option<String>,
    },
    Output(String),
    Continuing {
        from: usize,
        to: usize,
    },
    Snoozed {
        profile: usize,
        until: i64,
    },
    Finished(io::Result<()>),
}

enum ControlMessage {
    Interrupt,
    PermissionResponse {
        req_id: String,
        allow: bool,
        steer_msg: Option<String>,
    },
}

pub struct Mission {
    pub(crate) events: Receiver<Event>,
    pub normalized: Receiver<NormalizedEvent>,
    cancel: Arc<AtomicBool>,
    control: Arc<Mutex<Option<Sender<ControlMessage>>>>,
    active_pid: Arc<AtomicU32>,
}

impl Drop for Mission {
    fn drop(&mut self) {
        self.interrupt();
    }
}

impl Mission {
    pub fn interrupt(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let has_control = if let Some(tx) = self
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(ControlMessage::Interrupt);
            true
        } else {
            false
        };
        if !has_control {
            let pid = self.active_pid.swap(0, Ordering::AcqRel);
            if pid != 0 {
                thread::spawn(move || process_tree::kill_tree(pid));
            }
        }
    }

    pub fn respond_permission(
        &self,
        req_id: &str,
        allow: bool,
        steer_msg: Option<&str>,
    ) -> io::Result<()> {
        let guard = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(tx) = guard.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "No interactive stdio permission channel is active for this mission.",
            ));
        };
        tx.send(ControlMessage::PermissionResponse {
            req_id: req_id.to_owned(),
            allow,
            steer_msg: steer_msg.map(str::to_owned),
        })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Permission channel closed"))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LimitPolicy {
    pub(crate) auto_resume_on_reset: bool,
    pub(crate) fallback_profile: Option<String>,
}

pub(crate) struct Route {
    pub(crate) request: Request,
    pub(crate) preferred: Option<String>,
    pub(crate) catalog: Catalog,
    pub(crate) pins: Pins,
    pub(crate) limits: LimitPolicy,
    pub(crate) target: Option<Target>,
    pub(crate) mode: PermissionMode,
}

impl Route {
    #[cfg(test)]
    pub(crate) fn single(prompt: &str, preferred: Option<String>) -> Self {
        Self {
            request: Request {
                units: vec![prompt.into()],
                ..Request::default()
            },
            preferred,
            catalog: Catalog::default(),
            pins: Pins::default(),
            limits: LimitPolicy::default(),
            target: None,
            mode: PermissionMode::Supervised,
        }
    }
}

impl Mission {
    #[cfg(test)]
    pub(crate) fn held() -> (Self, std::sync::mpsc::Sender<Event>) {
        let (sender, events) = std::sync::mpsc::channel();
        let (_norm_tx, normalized) = mpsc::channel();
        let mission = Self {
            events,
            normalized,
            cancel: Arc::default(),
            control: Arc::new(Mutex::new(None)),
            active_pid: Arc::new(AtomicU32::new(0)),
        };
        (mission, sender)
    }

    pub(crate) fn launch(profiles: Vec<Profile>, route: Route, workspace: PathBuf) -> Self {
        let options = LaunchOptions {
            policy: PermissionPolicy::for_mode(route.mode),
            ..LaunchOptions::default()
        };
        Self::launch_with_options(profiles, route, workspace, options)
    }

    pub(crate) fn launch_with_options(
        profiles: Vec<Profile>,
        route: Route,
        workspace: PathBuf,
        options: LaunchOptions,
    ) -> Self {
        Self::launch_with_clock_and_options(profiles, route, workspace, usage::now, options)
    }

    #[cfg(all(test, unix))]
    fn launch_with_clock(
        profiles: Vec<Profile>,
        route: Route,
        workspace: PathBuf,
        clock: fn() -> usage::Tick,
    ) -> Self {
        let options = LaunchOptions {
            policy: PermissionPolicy::for_mode(route.mode),
            ..LaunchOptions::default()
        };
        Self::launch_with_clock_and_options(profiles, route, workspace, clock, options)
    }

    fn launch_with_clock_and_options(
        profiles: Vec<Profile>,
        route: Route,
        workspace: PathBuf,
        clock: fn() -> usage::Tick,
        options: LaunchOptions,
    ) -> Self {
        Self::start(
            Execution {
                profiles,
                workspace,
                clock,
                limits: route.limits.clone(),
                mode: route.mode,
                options,
                active_pid: Arc::new(AtomicU32::new(0)),
            },
            Work::Routed(route),
        )
    }

    pub(crate) fn resume(
        profiles: Vec<Profile>,
        interrupted: usize,
        workspace: PathBuf,
        prompt: String,
        claim: Claim,
    ) -> Self {
        Self::start(
            Execution {
                profiles,
                workspace,
                clock: usage::now,
                limits: LimitPolicy::default(),
                mode: PermissionMode::Supervised,
                options: LaunchOptions {
                    policy: PermissionPolicy::for_mode(PermissionMode::Supervised),
                    ..LaunchOptions::default()
                },
                active_pid: Arc::new(AtomicU32::new(0)),
            },
            Work::Resume {
                interrupted,
                prompt,
                claim,
            },
        )
    }

    fn start(execution: Execution, work: Work) -> Self {
        let (sender, events) = mpsc::sync_channel(64);
        let (norm_sender, normalized) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let control = Arc::new(Mutex::new(None));
        let active_pid = Arc::clone(&execution.active_pid);
        let worker_cancel = Arc::clone(&cancel);
        let worker_control = Arc::clone(&control);
        thread::spawn(move || {
            let result = match work {
                Work::Routed(route) => execution.route(
                    &route,
                    &sender,
                    &norm_sender,
                    &worker_cancel,
                    &worker_control,
                ),
                Work::Resume {
                    interrupted,
                    prompt,
                    claim,
                } => execution.resume(
                    interrupted,
                    &prompt,
                    claim,
                    &sender,
                    &norm_sender,
                    &worker_cancel,
                    &worker_control,
                ),
            };
            let _ = sender.send(Event::Finished(result));
        });
        Self {
            events,
            normalized,
            cancel,
            control,
            active_pid,
        }
    }
}

pub struct WarmSession {
    provider: Provider,
    pub events: Receiver<NormalizedEvent>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Option<Child>>>,
    interrupted: Arc<AtomicBool>,
}

impl Drop for WarmSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl WarmSession {
    pub fn spawn_claude(
        profile: &Profile,
        workspace: &Path,
        configuration: &Configuration,
        mode: PermissionMode,
        options: &LaunchOptions,
    ) -> io::Result<Self> {
        if profile.provider != Provider::Claude {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Warm persistent stdio sessions are supported for Claude.",
            ));
        }
        let mut command = profile.authenticated_command()?;
        command.args(configuration.arguments());
        command.args(claude_persistent_arguments(mode, options));
        permissions::refuse_bypass(command.get_args()).map_err(io::Error::other)?;
        command
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process_tree::own_group(&mut command);

        let mut child = command.spawn()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().expect("warm session stdout is piped");
        let stderr = child.stderr.take().expect("warm session stderr is piped");
        let child_handle = Arc::new(Mutex::new(Some(child)));
        let interrupted = Arc::new(AtomicBool::new(false));
        let (tx, events) = mpsc::channel();

        let stderr_tx = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    let _ = stderr_tx.send(NormalizedEvent::StderrLine(trimmed.to_owned()));
                }
            }
        });

        let stdout_tx = tx;
        let stdout_stdin = Arc::clone(&stdin);
        let stdout_interrupted = Arc::clone(&interrupted);
        let policy = options.policy.clone();
        let profile_clone = profile.clone();
        thread::spawn(move || {
            let mut parser = EventParser::with_profile(Provider::Claude, profile_clone.to_string());
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let _ = Usage::observe_claude(&profile_clone, &value, usage::now());
                let is_interrupted = stdout_interrupted.load(Ordering::Acquire);
                for event in parser.parse_value(&value, is_interrupted, Some(&policy)) {
                    if let NormalizedEvent::PermissionAsked {
                        ref req_id,
                        ref tool,
                        ref input,
                        ..
                    } = event
                    {
                        let resource = tool_resource(tool, input);
                        let resolved = policy.evaluate(tool, &resource, input);
                        match resolved.effect {
                            PermissionEffect::Allow => {
                                let _ = write_claude_permission_response(
                                    &stdout_stdin,
                                    req_id,
                                    true,
                                    None,
                                );
                                continue;
                            }
                            PermissionEffect::Deny => {
                                let _ = write_claude_permission_response(
                                    &stdout_stdin,
                                    req_id,
                                    false,
                                    resolved.reason.as_deref(),
                                );
                                continue;
                            }
                            PermissionEffect::Ask => {}
                        }
                    }
                    if matches!(event, NormalizedEvent::TurnFinished { .. }) {
                        stdout_interrupted.store(false, Ordering::Release);
                    }
                    if stdout_tx.send(event).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self {
            provider: Provider::Claude,
            events,
            stdin,
            child: child_handle,
            interrupted,
        })
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn send_turn(&self, prompt: &str) -> io::Result<()> {
        self.interrupted.store(false, Ordering::Release);
        let payload = claude_user_turn_json(prompt);
        write_stdin_line(&self.stdin, &payload)
    }

    pub fn interrupt(&self) -> io::Result<()> {
        self.interrupted.store(true, Ordering::Release);
        let payload = claude_interrupt_request_json(&uuid::Uuid::new_v4().to_string());
        if write_stdin_line(&self.stdin, &payload).is_err() {
            self.force_kill();
        }
        Ok(())
    }

    pub fn respond_permission(
        &self,
        req_id: &str,
        allow: bool,
        steer_msg: Option<&str>,
    ) -> io::Result<()> {
        write_claude_permission_response(&self.stdin, req_id, allow, steer_msg)
    }

    pub fn force_kill(&self) {
        if let Some(mut child) = self
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            process_tree::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn shutdown(&self) {
        self.stdin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.force_kill();
    }
}

pub fn claude_user_turn_json(prompt: &str) -> String {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": prompt }]
        }
    })
    .to_string()
}

pub fn claude_interrupt_request_json(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" }
    })
    .to_string()
}

pub fn claude_permission_response_json(
    request_id: &str,
    allow: bool,
    steer_msg: Option<&str>,
) -> String {
    let inner = if allow {
        json!({ "behavior": "allow" })
    } else {
        json!({
            "behavior": "deny",
            "message": steer_msg.unwrap_or("Denied by operator.")
        })
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": inner
        }
    })
    .to_string()
}

fn write_claude_permission_response(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    req_id: &str,
    allow: bool,
    steer_msg: Option<&str>,
) -> io::Result<()> {
    let payload = claude_permission_response_json(req_id, allow, steer_msg);
    write_stdin_line(stdin, &payload)
}

fn write_stdin_line(stdin: &Arc<Mutex<Option<ChildStdin>>>, line: &str) -> io::Result<()> {
    let mut guard = stdin.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(writer) = guard.as_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "Provider stdin is closed.",
        ));
    };
    writeln!(writer, "{line}")?;
    writer.flush()
}

pub fn claude_persistent_arguments(mode: PermissionMode, options: &LaunchOptions) -> Vec<String> {
    let mut args = vec![
        "-p".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--include-partial-messages".to_owned(),
        "--permission-prompt-tool".to_owned(),
        "stdio".to_owned(),
        "--setting-sources".to_owned(),
        "user".to_owned(),
    ];
    if let Some(session_id) = &options.resume_session {
        args.extend(["--resume".to_owned(), session_id.clone()]);
    }
    if let Some(mcp_config) = &options.mcp_config {
        args.extend(["--mcp-config".to_owned(), mcp_config.display().to_string()]);
    }
    args.extend(mode.claude_arguments().into_iter().map(str::to_owned));
    args.extend(options.extra_args.iter().cloned());
    args
}

pub fn build_provider_arguments(
    provider: Provider,
    mode: PermissionMode,
    options: &LaunchOptions,
) -> Vec<String> {
    match provider {
        Provider::Claude if options.persistent_stdio => claude_persistent_arguments(mode, options),
        Provider::Claude => {
            let mut args: Vec<String> = permissions::dispatch_arguments(provider, mode)
                .into_iter()
                .map(str::to_owned)
                .collect();
            let has_trailing_dash = args.last().is_some_and(|a| a == "--");
            if has_trailing_dash {
                args.pop();
            }
            if let Some(session_id) = &options.resume_session {
                args.extend(["--resume".to_owned(), session_id.clone()]);
            }
            if let Some(mcp_config) = &options.mcp_config {
                args.extend(["--mcp-config".to_owned(), mcp_config.display().to_string()]);
            }
            args.extend(options.extra_args.iter().cloned());
            if has_trailing_dash {
                args.push("--".to_owned());
            }
            args
        }
        Provider::Codex => {
            if let Some(thread_id) = &options.resume_session {
                let mut args = vec![
                    "exec".to_owned(),
                    "resume".to_owned(),
                    "--json".to_owned(),
                    "--skip-git-repo-check".to_owned(),
                ];
                args.extend(options.extra_args.iter().cloned());
                args.push(thread_id.clone());
                args.push("-".to_owned());
                args
            } else {
                let mut args: Vec<String> = permissions::dispatch_arguments(provider, mode)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let has_stdin_dash = args.last().is_some_and(|a| a == "-");
                if has_stdin_dash {
                    args.pop();
                }
                args.extend(options.extra_args.iter().cloned());
                if has_stdin_dash {
                    args.push("-".to_owned());
                }
                args
            }
        }
        Provider::Gemini => {
            let mut args: Vec<String> = permissions::dispatch_arguments(provider, mode)
                .into_iter()
                .map(str::to_owned)
                .collect();
            if let Some(session_id) = &options.resume_session {
                args.extend(["--resume".to_owned(), session_id.clone()]);
            }
            args.extend(options.extra_args.iter().cloned());
            args.extend(["-p".to_owned(), String::new()]);
            args
        }
    }
}

const BENIGN_STDERR_WARNINGS: [&str; 11] = [
    "warning: 256-color support not detected",
    "warning: true color (24-bit) support not detected",
    "warning: basic terminal detected",
    "warning: limited color support detected",
    "warning: windows 10 detected",
    "warning: gnu screen detected",
    "warning: jetbrains terminal detected",
    "ripgrep is not available",
    "experimentalwarning:",
    "deprecationwarning:",
    "(use `node --trace-",
];

fn is_benign_stderr_warning(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    BENIGN_STDERR_WARNINGS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

fn is_critical_stderr_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    const CRITICAL_SIGNALS: [&str; 20] = [
        "an unexpected critical error occurred",
        "critical error",
        "fatal:",
        "error:",
        "authentication failed",
        "failed to authenticate",
        "authentication required",
        "not logged in",
        "unauthorized",
        "invalid api key",
        "invalid_grant",
        "sign in first",
        "no longer supported",
        "migrate to",
        "no conversation found",
        "session not found",
        "no such session",
        "thread not found",
        "no such thread",
        "panicked at",
    ];
    CRITICAL_SIGNALS.iter().any(|signal| lower.contains(signal))
}

fn non_benign_stderr_lines(stderr_lines: &[String]) -> Vec<&str> {
    stderr_lines
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_benign_stderr_warning(line))
        .collect()
}

fn push_stderr_tail(lines: &mut Vec<String>, line: String) {
    if lines.len() < STDERR_TAIL_LIMIT {
        lines.push(line);
        return;
    }
    if is_benign_stderr_warning(&line) {
        if let Some(pos) = lines
            .iter()
            .position(|entry| is_benign_stderr_warning(entry))
        {
            lines.remove(pos);
            lines.push(line);
        }
        return;
    }
    if let Some(pos) = lines
        .iter()
        .position(|entry| is_benign_stderr_warning(entry))
    {
        lines.remove(pos);
    } else {
        lines.remove(0);
    }
    lines.push(line);
}

fn summarize_stderr_for_error(captured: &[String]) -> String {
    let relevant = non_benign_stderr_lines(captured);
    if !relevant.is_empty() {
        let critical: Vec<&str> = relevant
            .iter()
            .copied()
            .filter(|line| is_critical_stderr_line(line))
            .collect();
        if !critical.is_empty() {
            return critical.join(" | ");
        }
        return relevant.join(" | ");
    }
    captured
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn format_exit_detail(status_label: &str, captured: &[String]) -> String {
    let summary = summarize_stderr_for_error(captured);
    if summary.is_empty() {
        format!("Provider exited with {status_label}")
    } else {
        format!("Provider exited with {status_label}: {summary}")
    }
}

pub fn classify_health(stderr_lines: &[String], io_error: Option<&io::Error>) -> HealthCode {
    if let Some(err) = io_error {
        match err.kind() {
            io::ErrorKind::NotFound => return HealthCode::AgentMissing,
            io::ErrorKind::PermissionDenied => return HealthCode::AgentBlocked,
            _ => {}
        }
    }
    let joined = non_benign_stderr_lines(stderr_lines)
        .join("\n")
        .to_ascii_lowercase();
    const AUTH_SIGNALS: [&str; 12] = [
        "not logged in",
        "authentication failed",
        "failed to authenticate",
        "authentication required",
        "unauthorized",
        "invalid api key",
        "invalid_grant",
        "oauth",
        "credentials",
        "sign in first",
        "no longer supported",
        "migrate to",
    ];
    if AUTH_SIGNALS.iter().any(|signal| joined.contains(signal)) {
        return HealthCode::AgentAuth;
    }
    HealthCode::AgentFailed
}

pub fn classify_tool_kind(tool_name: &str) -> ToolKind {
    match tool_name.trim().to_ascii_lowercase().as_str() {
        "bash" | "shell" | "execute" | "command_execution" | "run_command"
        | "run_shell_command" => ToolKind::Shell,
        "read" | "read_file" | "view_file" | "cat" => ToolKind::Read,
        "edit" | "edit_file" | "multiedit" | "patch" | "replace_file_content" => ToolKind::Edit,
        "write" | "write_file" | "notebookedit" | "create_file" => ToolKind::Write,
        "glob" | "grep" | "ls" | "find" | "search" | "ripgrep" | "list_dir" => ToolKind::Search,
        "webfetch" | "websearch" | "web_search" | "fetch" => ToolKind::Web,
        "task" | "agent" | "subagent" | "spawn_worker" | "collab_tool_call" => ToolKind::Subagent,
        "todowrite" | "todo_list" => ToolKind::Todo,
        name if name.starts_with("mcp__") || name.contains("mcp") => ToolKind::Mcp,
        _ => ToolKind::Generic,
    }
}

fn tool_resource(tool_name: &str, input: &Value) -> String {
    for key in ["command", "file_path", "path", "url", "pattern", "query"] {
        if let Some(val) = input.get(key).and_then(Value::as_str) {
            return val.to_owned();
        }
    }
    tool_name.to_owned()
}

fn summarize_tool(name: &str, input: &Value) -> String {
    for key in [
        "command",
        "cmd",
        "file_path",
        "path",
        "notebook_path",
        "pattern",
        "query",
        "url",
        "description",
    ] {
        if let Some(val) = input.get(key).and_then(Value::as_str) {
            let single_line = val.lines().next().unwrap_or(val).trim();
            if !single_line.is_empty() {
                let clipped: String = single_line.chars().take(120).collect();
                return format!("{name}: {clipped}");
            }
        }
    }
    name.to_owned()
}

fn summarize_partial_tool_json(name: &str, partial_json: &str) -> Option<String> {
    for key in [
        "command",
        "cmd",
        "file_path",
        "path",
        "notebook_path",
        "pattern",
        "query",
        "url",
        "description",
    ] {
        if let Some(val) = extract_partial_json_string_field(partial_json, key) {
            let single_line = val.lines().next().unwrap_or(&val).trim();
            if !single_line.is_empty() {
                let clipped: String = single_line.chars().take(120).collect();
                return Some(format!("{name}: {clipped}"));
            }
        }
    }
    None
}

fn extract_partial_json_string_field(partial: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut offset = 0usize;
    while let Some(found) = partial[offset..].find(&needle) {
        let after_key = offset + found + needle.len();
        let rest = partial[after_key..].trim_start();
        if let Some(after_colon) = rest.strip_prefix(':') {
            let value_start = after_colon.trim_start();
            if let Some(quoted) = value_start.strip_prefix('"') {
                return parse_complete_json_string_literal(quoted);
            }
        }
        offset = after_key;
    }
    None
}

fn parse_complete_json_string_literal(input: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000C}'),
                'u' => {
                    let mut hex = String::with_capacity(4);
                    for _ in 0..4 {
                        hex.push(chars.next()?);
                    }
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    if let Some(decoded) = char::from_u32(code) {
                        out.push(decoded);
                    }
                }
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

fn preview_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= OUTPUT_PREVIEW_CHARS {
        trimmed.to_owned()
    } else {
        let mut out: String = trimmed.chars().take(OUTPUT_PREVIEW_CHARS).collect();
        out.push('…');
        out
    }
}

enum ActiveStreamBlock {
    Thinking,
    Text,
    ToolUse {
        call_id: String,
        name: String,
        partial_json: String,
        emitted_summary: String,
    },
}

pub struct EventParser {
    provider: Provider,
    profile: Option<String>,
    last_cumulative_cost_usd: f64,
    active_stream_message_id: Option<String>,
    streamed_text_in_message: bool,
    streamed_thinking_in_message: bool,
    gemini_streamed_delta: bool,
    final_text: String,
    tool_names_by_id: HashMap<String, (String, Value)>,
    emitted_tool_summaries: HashMap<String, String>,
    active_blocks: HashMap<usize, ActiveStreamBlock>,
    codex_item_text: HashMap<String, String>,
    codex_started_tools: HashSet<String>,
}

impl EventParser {
    pub fn new(provider: Provider) -> Self {
        Self::create(provider, None)
    }

    pub fn with_profile(provider: Provider, profile: String) -> Self {
        Self::create(provider, Some(profile))
    }

    fn create(provider: Provider, profile: Option<String>) -> Self {
        Self {
            provider,
            profile,
            last_cumulative_cost_usd: 0.0,
            active_stream_message_id: None,
            streamed_text_in_message: false,
            streamed_thinking_in_message: false,
            gemini_streamed_delta: false,
            final_text: String::new(),
            tool_names_by_id: HashMap::new(),
            emitted_tool_summaries: HashMap::new(),
            active_blocks: HashMap::new(),
            codex_item_text: HashMap::new(),
            codex_started_tools: HashSet::new(),
        }
    }

    pub fn parse_value(
        &mut self,
        event: &Value,
        interrupted: bool,
        policy: Option<&PermissionPolicy>,
    ) -> Vec<NormalizedEvent> {
        match self.provider {
            Provider::Claude => self.parse_claude(event, interrupted, policy),
            Provider::Codex => self.parse_codex(event, interrupted),
            Provider::Gemini => self.parse_gemini(event, interrupted),
        }
    }

    fn emit_claude_tool_start(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        raw_input: Value,
        out: &mut Vec<NormalizedEvent>,
    ) {
        if raw_input.is_object() && !raw_input.as_object().is_some_and(serde_json::Map::is_empty)
            || !self.tool_names_by_id.contains_key(&call_id)
        {
            self.tool_names_by_id
                .insert(call_id.clone(), (name.clone(), raw_input.clone()));
        }
        let should_emit = match self.emitted_tool_summaries.get(&call_id) {
            None => true,
            Some(prev) => prev != &summary && summary.len() > prev.len(),
        };
        if should_emit {
            self.emitted_tool_summaries
                .insert(call_id.clone(), summary.clone());
            out.push(NormalizedEvent::ToolStart {
                call_id,
                kind: classify_tool_kind(&name),
                name,
                summary,
                raw_input,
            });
        }
    }

    fn parse_claude(
        &mut self,
        event: &Value,
        interrupted: bool,
        policy: Option<&PermissionPolicy>,
    ) -> Vec<NormalizedEvent> {
        let mut out = Vec::new();
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "init" => {
                if let Some(session_id) = event["session_id"].as_str() {
                    out.push(NormalizedEvent::SessionStarted {
                        session_id: session_id.to_owned(),
                        model: event["model"].as_str().map(str::to_owned),
                        profile: self.profile.clone(),
                    });
                }
            }
            Some("stream_event") => {
                let inner = &event["event"];
                match inner["type"].as_str() {
                    Some("message_start") => {
                        self.active_stream_message_id =
                            inner["message"]["id"].as_str().map(str::to_owned);
                        self.streamed_text_in_message = false;
                        self.streamed_thinking_in_message = false;
                        self.active_blocks.clear();
                    }
                    Some("content_block_start") => {
                        let index = inner["index"].as_u64().unwrap_or(0) as usize;
                        let block = &inner["content_block"];
                        match block["type"].as_str() {
                            Some("thinking" | "redacted_thinking") => {
                                self.active_blocks
                                    .insert(index, ActiveStreamBlock::Thinking);
                                let initial = block["thinking"].as_str().unwrap_or_default();
                                if !initial.is_empty() {
                                    self.streamed_thinking_in_message = true;
                                }
                                out.push(NormalizedEvent::ThinkingDelta(initial.to_owned()));
                            }
                            Some("text") => {
                                self.active_blocks.insert(index, ActiveStreamBlock::Text);
                                if let Some(initial) =
                                    block["text"].as_str().filter(|t| !t.is_empty())
                                {
                                    self.streamed_text_in_message = true;
                                    self.final_text.push_str(initial);
                                    out.push(NormalizedEvent::TextDelta(initial.to_owned()));
                                }
                            }
                            Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {
                                let call_id = block["id"].as_str().unwrap_or_default().to_owned();
                                let name = block["name"].as_str().unwrap_or("tool").to_owned();
                                let initial_input = block["input"].clone();
                                let summary = summarize_tool(&name, &initial_input);
                                self.active_blocks.insert(
                                    index,
                                    ActiveStreamBlock::ToolUse {
                                        call_id: call_id.clone(),
                                        name: name.clone(),
                                        partial_json: String::new(),
                                        emitted_summary: summary.clone(),
                                    },
                                );
                                self.emit_claude_tool_start(
                                    call_id,
                                    name,
                                    summary,
                                    initial_input,
                                    &mut out,
                                );
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_delta") => {
                        let index = inner["index"].as_u64().unwrap_or(0) as usize;
                        let delta = &inner["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                if let Some(text) = delta["text"].as_str().filter(|t| !t.is_empty())
                                {
                                    self.streamed_text_in_message = true;
                                    self.final_text.push_str(text);
                                    out.push(NormalizedEvent::TextDelta(text.to_owned()));
                                }
                            }
                            Some("thinking_delta") => {
                                if let Some(thinking) =
                                    delta["thinking"].as_str().filter(|t| !t.is_empty())
                                {
                                    self.streamed_thinking_in_message = true;
                                    out.push(NormalizedEvent::ThinkingDelta(thinking.to_owned()));
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(chunk) = delta["partial_json"].as_str() {
                                    let enriched = if let Some(ActiveStreamBlock::ToolUse {
                                        call_id,
                                        name,
                                        partial_json,
                                        emitted_summary,
                                    }) = self.active_blocks.get_mut(&index)
                                    {
                                        if partial_json.len() < MAX_PARTIAL_TOOL_JSON_BYTES {
                                            partial_json.push_str(chunk);
                                        }
                                        if *emitted_summary == *name {
                                            if let Some(rich) =
                                                summarize_partial_tool_json(name, partial_json)
                                            {
                                                *emitted_summary = rich.clone();
                                                Some((call_id.clone(), name.clone(), rich))
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    };
                                    if let Some((call_id, name, summary)) = enriched {
                                        self.emit_claude_tool_start(
                                            call_id,
                                            name,
                                            summary,
                                            Value::Object(serde_json::Map::new()),
                                            &mut out,
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_stop") => {
                        let index = inner["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(ActiveStreamBlock::ToolUse {
                            call_id,
                            name,
                            partial_json,
                            ..
                        }) = self.active_blocks.remove(&index)
                        {
                            if let Ok(parsed) = serde_json::from_str::<Value>(&partial_json) {
                                let summary = summarize_tool(&name, &parsed);
                                self.emit_claude_tool_start(
                                    call_id, name, summary, parsed, &mut out,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                let msg_id = event["message"]["id"].as_str();
                let matches_streamed = match (msg_id, self.active_stream_message_id.as_deref()) {
                    (Some(a), Some(b)) => a == b,
                    (None, _) | (_, None) => true,
                };
                if let Some(items) = event["message"]["content"].as_array() {
                    for item in items {
                        match item["type"].as_str() {
                            Some("text") => {
                                if !(matches_streamed && self.streamed_text_in_message) {
                                    if let Some(text) =
                                        item["text"].as_str().filter(|t| !t.is_empty())
                                    {
                                        self.final_text.push_str(text);
                                        out.push(NormalizedEvent::TextDelta(text.to_owned()));
                                    }
                                }
                            }
                            Some("thinking") => {
                                if !(matches_streamed && self.streamed_thinking_in_message) {
                                    if let Some(thinking) =
                                        item["thinking"].as_str().filter(|t| !t.is_empty())
                                    {
                                        out.push(NormalizedEvent::ThinkingDelta(
                                            thinking.to_owned(),
                                        ));
                                    }
                                }
                            }
                            Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {
                                let call_id = item["id"].as_str().unwrap_or_default().to_owned();
                                let name = item["name"].as_str().unwrap_or("tool").to_owned();
                                let input = item["input"].clone();
                                let summary = summarize_tool(&name, &input);
                                self.emit_claude_tool_start(
                                    call_id, name, summary, input, &mut out,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                let usage_val = &event["message"]["usage"];
                if usage_val.is_object() {
                    let input = usage_val["input_tokens"].as_u64().unwrap_or(0);
                    let cached_read = usage_val["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    let cache_create = usage_val["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    let output = usage_val["output_tokens"].as_u64().unwrap_or(0);
                    let context_used = input
                        .saturating_add(cached_read)
                        .saturating_add(cache_create);
                    if context_used > 0 || output > 0 {
                        out.push(NormalizedEvent::UsageDelta {
                            input,
                            cached_read,
                            output,
                            cost_usd: None,
                            context_used: Some(context_used),
                        });
                    }
                }
            }
            Some("user") => {
                self.active_stream_message_id = None;
                self.streamed_text_in_message = false;
                self.streamed_thinking_in_message = false;
                self.active_blocks.clear();
                if let Some(items) = event["message"]["content"].as_array() {
                    for item in items {
                        if item["type"] == "tool_result" {
                            let call_id =
                                item["tool_use_id"].as_str().unwrap_or_default().to_owned();
                            let ok = item["is_error"] != true;
                            let mut content_str = match &item["content"] {
                                Value::Null => String::new(),
                                Value::String(text) => text.clone(),
                                Value::Array(parts) => parts
                                    .iter()
                                    .filter_map(|p| p["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                other => other.to_string(),
                            };
                            if content_str.trim().is_empty() {
                                let result_meta = event
                                    .get("tool_use_result")
                                    .or_else(|| event["message"].get("tool_use_result"));
                                if let Some(meta) = result_meta {
                                    if let Some(text) =
                                        meta.as_str().filter(|s| !s.trim().is_empty())
                                    {
                                        content_str = text.to_owned();
                                    } else if let Some(stdout) =
                                        meta["stdout"].as_str().filter(|s| !s.trim().is_empty())
                                    {
                                        content_str = stdout.to_owned();
                                    } else if let Some(stderr) =
                                        meta["stderr"].as_str().filter(|s| !s.trim().is_empty())
                                    {
                                        content_str = stderr.to_owned();
                                    }
                                }
                            }
                            out.push(NormalizedEvent::ToolEnd {
                                call_id: call_id.clone(),
                                ok,
                                output_preview: preview_text(&content_str),
                                exit_code: None,
                            });
                            if ok {
                                if let Some((name, input)) = self.tool_names_by_id.get(&call_id) {
                                    if let Some(path) = input
                                        .get("file_path")
                                        .or_else(|| input.get("path"))
                                        .or_else(|| input.get("notebook_path"))
                                        .and_then(Value::as_str)
                                    {
                                        let kind = match classify_tool_kind(name) {
                                            ToolKind::Write => Some(PatchKind::Add),
                                            ToolKind::Edit => Some(PatchKind::Update),
                                            _ => None,
                                        };
                                        if let Some(kind) = kind {
                                            out.push(NormalizedEvent::FileChanged {
                                                path: path.to_owned(),
                                                kind,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Some("control_request") => {
                let req = &event["request"];
                if req["subtype"] == "can_use_tool" {
                    let req_id = event["request_id"].as_str().unwrap_or_default().to_owned();
                    let tool = req["tool_name"].as_str().unwrap_or("tool").to_owned();
                    let input = req["input"].clone();
                    let prod_gate_reason = policy
                        .and_then(|p| {
                            p.evaluate(&tool, &tool_resource(&tool, &input), &input)
                                .prod_gate
                        })
                        .or_else(|| crate::prodgate::ProdGate::check(&tool, &input))
                        .map(|hit| hit.reason);
                    out.push(NormalizedEvent::PermissionAsked {
                        req_id,
                        tool,
                        input,
                        prod_gate_reason,
                    });
                }
            }
            Some("rate_limit_event") => {
                let info = &event["rate_limit_info"];
                let windows = &info["unifiedWindows"];
                let pct = |value: &Value| {
                    value
                        .as_f64()
                        .filter(|u| u.is_finite())
                        .map(|u| (u * 100.0).clamp(0.0, 100.0) as f32)
                };
                let mut five = pct(&windows["five_hour"]["utilization"]);
                let mut seven = [
                    "seven_day",
                    "seven_day_opus",
                    "seven_day_sonnet",
                    "seven_day_overage_included",
                    "overage",
                ]
                .into_iter()
                .filter_map(|key| pct(&windows[key]["utilization"]))
                .max_by(f32::total_cmp);
                let rejected = info["status"] == "rejected";
                let top_pct = pct(&info["utilization"]).or_else(|| rejected.then_some(100.0));
                let merge =
                    |existing: Option<f32>, candidate: Option<f32>| match (existing, candidate) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                match info["rateLimitType"].as_str() {
                    Some("five_hour") => five = merge(five, top_pct),
                    Some(
                        "seven_day"
                        | "seven_day_opus"
                        | "seven_day_sonnet"
                        | "seven_day_overage_included"
                        | "overage",
                    ) => seven = merge(seven, top_pct),
                    _ => {}
                }
                let resets_at = info["resetsAt"]
                    .as_i64()
                    .map(|ts| ts.to_string())
                    .or_else(|| info["resetsAt"].as_str().map(str::to_owned));
                out.push(NormalizedEvent::RateLimit {
                    five_hour_pct: five,
                    seven_day_pct: seven,
                    resets_at,
                });
            }
            Some("result") => {
                if let Some(total_cost) = event["total_cost_usd"].as_f64() {
                    let delta = (total_cost - self.last_cumulative_cost_usd).max(0.0);
                    self.last_cumulative_cost_usd = total_cost;
                    let usage_val = &event["usage"];
                    out.push(NormalizedEvent::UsageDelta {
                        input: usage_val["input_tokens"].as_u64().unwrap_or(0),
                        cached_read: usage_val["cache_read_input_tokens"].as_u64().unwrap_or(0),
                        output: usage_val["output_tokens"].as_u64().unwrap_or(0),
                        cost_usd: Some(delta),
                        context_used: None,
                    });
                }
                let raw_is_error = event["is_error"] == true;
                let was_interrupted = interrupted && event["subtype"] == "error_during_execution";
                let final_text = if !self.final_text.is_empty() {
                    std::mem::take(&mut self.final_text)
                } else {
                    event["result"].as_str().unwrap_or_default().to_owned()
                };
                self.active_stream_message_id = None;
                self.streamed_text_in_message = false;
                self.streamed_thinking_in_message = false;
                self.active_blocks.clear();
                self.emitted_tool_summaries.clear();
                out.push(NormalizedEvent::TurnFinished {
                    interrupted: was_interrupted,
                    is_error: raw_is_error && !was_interrupted,
                    error_code: (raw_is_error && !was_interrupted)
                        .then_some(HealthCode::AgentFailed),
                    final_text,
                });
            }
            _ => {}
        }
        out
    }

    fn codex_incremental_delta(&mut self, item_id: &str, full_text: &str, remove: bool) -> String {
        let previous = if remove {
            self.codex_item_text.remove(item_id).unwrap_or_default()
        } else {
            self.codex_item_text
                .get(item_id)
                .cloned()
                .unwrap_or_default()
        };
        let delta = if let Some(suffix) = full_text.strip_prefix(&previous) {
            suffix.to_owned()
        } else if full_text == previous {
            String::new()
        } else {
            full_text.to_owned()
        };
        if !remove {
            self.codex_item_text
                .insert(item_id.to_owned(), full_text.to_owned());
        }
        delta
    }

    fn emit_codex_tool_start(
        &mut self,
        call_id: &str,
        item: &Value,
        out: &mut Vec<NormalizedEvent>,
    ) {
        if !self.codex_started_tools.insert(call_id.to_owned()) {
            return;
        }
        match item["type"].as_str() {
            Some("command_execution") => {
                let cmd = item["command"].as_str().unwrap_or_default();
                out.push(NormalizedEvent::ToolStart {
                    call_id: call_id.to_owned(),
                    kind: ToolKind::Shell,
                    name: "command_execution".into(),
                    summary: format!("shell: {}", preview_text(cmd)),
                    raw_input: json!({ "command": cmd }),
                });
            }
            Some("file_change") => {
                let first_path = item["changes"]
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|c| c["path"].as_str())
                    .unwrap_or("files");
                out.push(NormalizedEvent::ToolStart {
                    call_id: call_id.to_owned(),
                    kind: ToolKind::Edit,
                    name: "file_change".into(),
                    summary: format!("Edit: {first_path}"),
                    raw_input: item.clone(),
                });
            }
            Some("mcp_tool_call") => {
                let tool = item["tool"].as_str().unwrap_or("mcp_tool").to_owned();
                out.push(NormalizedEvent::ToolStart {
                    call_id: call_id.to_owned(),
                    kind: ToolKind::Mcp,
                    summary: summarize_tool(&tool, &item["arguments"]),
                    name: tool,
                    raw_input: item["arguments"].clone(),
                });
            }
            Some("collab_tool_call") => {
                let tool = item["tool"].as_str().unwrap_or("collab").to_owned();
                out.push(NormalizedEvent::ToolStart {
                    call_id: call_id.to_owned(),
                    kind: ToolKind::Subagent,
                    summary: format!("subagent: {tool}"),
                    name: tool,
                    raw_input: item.clone(),
                });
            }
            Some("web_search") => {
                let query = item["query"].as_str().unwrap_or_default();
                out.push(NormalizedEvent::ToolStart {
                    call_id: call_id.to_owned(),
                    kind: ToolKind::Web,
                    name: "web_search".into(),
                    summary: format!("web_search: {}", preview_text(query)),
                    raw_input: json!({ "query": query }),
                });
            }
            _ => {}
        }
    }

    fn parse_codex(&mut self, event: &Value, interrupted: bool) -> Vec<NormalizedEvent> {
        let mut out = Vec::new();
        match event["type"].as_str() {
            Some("thread.started") => {
                if let Some(thread_id) = event["thread_id"].as_str() {
                    out.push(NormalizedEvent::SessionStarted {
                        session_id: thread_id.to_owned(),
                        model: None,
                        profile: self.profile.clone(),
                    });
                }
            }
            Some("item.started") => {
                let item = &event["item"];
                let call_id = item["id"].as_str().unwrap_or_default();
                match item["type"].as_str() {
                    Some("reasoning") => {
                        let initial = item["text"].as_str().unwrap_or_default();
                        let delta = self.codex_incremental_delta(call_id, initial, false);
                        out.push(NormalizedEvent::ThinkingDelta(delta));
                    }
                    Some("agent_message") => {
                        if let Some(text) = item["text"].as_str().filter(|t| !t.is_empty()) {
                            let delta = self.codex_incremental_delta(call_id, text, false);
                            if !delta.is_empty() {
                                self.final_text.push_str(&delta);
                                out.push(NormalizedEvent::TextDelta(delta));
                            }
                        }
                    }
                    Some(
                        "command_execution" | "file_change" | "mcp_tool_call" | "collab_tool_call"
                        | "web_search",
                    ) => {
                        self.emit_codex_tool_start(call_id, item, &mut out);
                    }
                    _ => {}
                }
            }
            Some("item.updated") => {
                let item = &event["item"];
                let call_id = item["id"].as_str().unwrap_or_default();
                match item["type"].as_str() {
                    Some("agent_message") => {
                        if let Some(text) = item["text"].as_str() {
                            let delta = self.codex_incremental_delta(call_id, text, false);
                            if !delta.is_empty() {
                                self.final_text.push_str(&delta);
                                out.push(NormalizedEvent::TextDelta(delta));
                            }
                        }
                    }
                    Some("reasoning") => {
                        if let Some(text) = item["text"].as_str() {
                            let delta = self.codex_incremental_delta(call_id, text, false);
                            if !delta.is_empty() {
                                out.push(NormalizedEvent::ThinkingDelta(delta));
                            }
                        }
                    }
                    Some(
                        "command_execution" | "file_change" | "mcp_tool_call" | "collab_tool_call"
                        | "web_search",
                    ) => {
                        self.emit_codex_tool_start(call_id, item, &mut out);
                    }
                    _ => {}
                }
            }
            Some("item.completed") => {
                let item = &event["item"];
                let call_id = item["id"].as_str().unwrap_or_default().to_owned();
                match item["type"].as_str() {
                    Some("agent_message") => {
                        if let Some(text) = item["text"].as_str() {
                            let delta = self.codex_incremental_delta(&call_id, text, true);
                            if !delta.is_empty() {
                                self.final_text.push_str(&delta);
                                out.push(NormalizedEvent::TextDelta(delta));
                            }
                        }
                    }
                    Some("reasoning") => {
                        if let Some(text) = item["text"].as_str() {
                            let delta = self.codex_incremental_delta(&call_id, text, true);
                            if !delta.is_empty() {
                                out.push(NormalizedEvent::ThinkingDelta(delta));
                            }
                        }
                    }
                    Some("command_execution") => {
                        self.emit_codex_tool_start(&call_id, item, &mut out);
                        self.codex_started_tools.remove(&call_id);
                        let status = item["status"].as_str().unwrap_or("completed");
                        let exit_code = item["exit_code"].as_i64().map(|c| c as i32);
                        let output = item["aggregated_output"].as_str().unwrap_or_default();
                        out.push(NormalizedEvent::ToolEnd {
                            call_id,
                            ok: status == "completed" && exit_code.unwrap_or(0) == 0,
                            output_preview: preview_text(output),
                            exit_code,
                        });
                    }
                    Some("file_change") => {
                        let had_tool_card = self.codex_started_tools.remove(&call_id);
                        let mut changed_paths = Vec::new();
                        if let Some(changes) = item["changes"].as_array() {
                            for change in changes {
                                if let Some(path) = change["path"].as_str() {
                                    changed_paths.push(path.to_owned());
                                    let kind = match change["kind"].as_str() {
                                        Some("add") => PatchKind::Add,
                                        Some("delete") => PatchKind::Delete,
                                        _ => PatchKind::Update,
                                    };
                                    out.push(NormalizedEvent::FileChanged {
                                        path: path.to_owned(),
                                        kind,
                                    });
                                }
                            }
                        }
                        if had_tool_card {
                            let ok = item["status"].as_str().unwrap_or("completed") != "failed";
                            out.push(NormalizedEvent::ToolEnd {
                                call_id,
                                ok,
                                output_preview: changed_paths.join(", "),
                                exit_code: None,
                            });
                        }
                    }
                    Some("mcp_tool_call" | "collab_tool_call" | "web_search") => {
                        self.emit_codex_tool_start(&call_id, item, &mut out);
                        self.codex_started_tools.remove(&call_id);
                        let ok = item["error"].is_null() && item["status"] != "failed";
                        let preview = item["error"]
                            .as_str()
                            .or_else(|| item["result"].as_str())
                            .map(preview_text)
                            .unwrap_or_else(|| {
                                if item["result"].is_null() {
                                    String::new()
                                } else {
                                    preview_text(&item["result"].to_string())
                                }
                            });
                        out.push(NormalizedEvent::ToolEnd {
                            call_id,
                            ok,
                            output_preview: preview,
                            exit_code: None,
                        });
                    }
                    Some("error") => {
                        if let Some(msg) = item["message"].as_str() {
                            out.push(NormalizedEvent::StderrLine(msg.to_owned()));
                        }
                    }
                    _ => {}
                }
            }
            Some("turn.completed") => {
                self.codex_item_text.clear();
                self.codex_started_tools.clear();
                let usage_val = &event["usage"];
                if usage_val.is_object() {
                    out.push(NormalizedEvent::UsageDelta {
                        input: usage_val["input_tokens"].as_u64().unwrap_or(0),
                        cached_read: usage_val["cached_input_tokens"].as_u64().unwrap_or(0),
                        output: usage_val["output_tokens"].as_u64().unwrap_or(0),
                        cost_usd: None,
                        context_used: None,
                    });
                }
                out.push(NormalizedEvent::TurnFinished {
                    interrupted,
                    is_error: false,
                    error_code: None,
                    final_text: std::mem::take(&mut self.final_text),
                });
            }
            Some("turn.failed" | "error") => {
                let msg = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("Codex turn failed");
                out.push(NormalizedEvent::StderrLine(msg.to_owned()));
                if event["type"] == "turn.failed" {
                    self.codex_item_text.clear();
                    self.codex_started_tools.clear();
                    out.push(NormalizedEvent::TurnFinished {
                        interrupted,
                        is_error: !interrupted,
                        error_code: (!interrupted).then_some(HealthCode::AgentFailed),
                        final_text: std::mem::take(&mut self.final_text),
                    });
                }
            }
            _ => {}
        }
        out
    }

    fn parse_gemini(&mut self, event: &Value, interrupted: bool) -> Vec<NormalizedEvent> {
        let mut out = Vec::new();
        match event["type"].as_str() {
            Some("init") => {
                self.gemini_streamed_delta = false;
                if let Some(session_id) = event["session_id"].as_str() {
                    out.push(NormalizedEvent::SessionStarted {
                        session_id: session_id.to_owned(),
                        model: event["model"].as_str().map(str::to_owned),
                        profile: self.profile.clone(),
                    });
                }
            }
            Some("thought" | "thinking") => {
                if let Some(thought) = event["content"]
                    .as_str()
                    .or_else(|| event["text"].as_str())
                    .filter(|t| !t.is_empty())
                {
                    out.push(NormalizedEvent::ThinkingDelta(thought.to_owned()));
                }
            }
            Some("message") if event["role"] == "assistant" => {
                if let Some(thought) = event["thought"].as_str().filter(|t| !t.is_empty()) {
                    out.push(NormalizedEvent::ThinkingDelta(thought.to_owned()));
                }
                if let Some(text) = event["content"].as_str().filter(|t| !t.is_empty()) {
                    let is_delta = event["delta"] == true;
                    if is_delta {
                        self.gemini_streamed_delta = true;
                        self.final_text.push_str(text);
                        out.push(NormalizedEvent::TextDelta(text.to_owned()));
                    } else if !self.gemini_streamed_delta || !self.final_text.ends_with(text) {
                        self.final_text.push_str(text);
                        out.push(NormalizedEvent::TextDelta(text.to_owned()));
                    }
                }
            }
            Some("tool_use") => {
                let call_id = event["tool_id"].as_str().unwrap_or_default().to_owned();
                let name = event["tool_name"].as_str().unwrap_or("tool").to_owned();
                let input = event["parameters"].clone();
                self.tool_names_by_id
                    .insert(call_id.clone(), (name.clone(), input.clone()));
                out.push(NormalizedEvent::ToolStart {
                    call_id,
                    kind: classify_tool_kind(&name),
                    summary: summarize_tool(&name, &input),
                    name,
                    raw_input: input,
                });
            }
            Some("tool_result") => {
                let call_id = event["tool_id"].as_str().unwrap_or_default().to_owned();
                let ok = event["status"] == "success";
                let output = event["output"]
                    .as_str()
                    .or_else(|| event["error"]["message"].as_str())
                    .unwrap_or_default();
                out.push(NormalizedEvent::ToolEnd {
                    call_id: call_id.clone(),
                    ok,
                    output_preview: preview_text(output),
                    exit_code: None,
                });
                if ok {
                    if let Some((name, input)) = self.tool_names_by_id.get(&call_id) {
                        if let Some(path) = input
                            .get("file_path")
                            .or_else(|| input.get("path"))
                            .and_then(Value::as_str)
                        {
                            let kind = match classify_tool_kind(name) {
                                ToolKind::Write => Some(PatchKind::Add),
                                ToolKind::Edit => Some(PatchKind::Update),
                                _ => None,
                            };
                            if let Some(kind) = kind {
                                out.push(NormalizedEvent::FileChanged {
                                    path: path.to_owned(),
                                    kind,
                                });
                            }
                        }
                    }
                }
            }
            Some("error") => {
                if let Some(msg) = event["message"].as_str() {
                    out.push(NormalizedEvent::StderrLine(msg.to_owned()));
                }
            }
            Some("result") => {
                self.gemini_streamed_delta = false;
                let stats = &event["stats"];
                if stats.is_object() {
                    out.push(NormalizedEvent::UsageDelta {
                        input: stats["input_tokens"].as_u64().unwrap_or(0),
                        cached_read: stats["cached"].as_u64().unwrap_or(0),
                        output: stats["output_tokens"].as_u64().unwrap_or(0),
                        cost_usd: None,
                        context_used: None,
                    });
                }
                let is_error = event["status"] != "success" && !interrupted;
                out.push(NormalizedEvent::TurnFinished {
                    interrupted,
                    is_error,
                    error_code: is_error.then_some(HealthCode::AgentFailed),
                    final_text: std::mem::take(&mut self.final_text),
                });
            }
            _ => {}
        }
        out
    }
}

enum Work {
    Routed(Route),
    Resume {
        interrupted: usize,
        prompt: String,
        claim: Claim,
    },
}

struct Execution {
    profiles: Vec<Profile>,
    workspace: PathBuf,
    clock: fn() -> usage::Tick,
    limits: LimitPolicy,
    mode: PermissionMode,
    options: LaunchOptions,
    active_pid: Arc<AtomicU32>,
}

impl Execution {
    fn route(
        &self,
        route: &Route,
        sender: &SyncSender<Event>,
        norm_sender: &Sender<NormalizedEvent>,
        cancel: &AtomicBool,
        control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    ) -> io::Result<()> {
        let now = (self.clock)();
        if route.request.split {
            Usage::reconcile_peer_accounts(&self.profiles, now);
            let (candidates, exclusions) =
                routing::candidates(&self.profiles, &route.catalog, &route.pins, now);
            let ranked = restrict(
                routing::rank(candidates, route.request.policy, route.request.difficulty),
                route.target,
            );
            return self.run_split(
                &route.request.units,
                ranked,
                &exclusions,
                sender,
                norm_sender,
                cancel,
                control,
            );
        }
        let choice = self.select_route(route, now)?;
        self.dispatch(
            &choice.candidate,
            &route.request.units[0],
            Some(choice.reason),
            true,
            sender,
            norm_sender,
            cancel,
            control,
        )
    }

    fn select_route(&self, route: &Route, now: usage::Tick) -> io::Result<routing::Choice> {
        Usage::reconcile_peer_accounts(&self.profiles, now);
        let (candidates, exclusions) =
            routing::candidates(&self.profiles, &route.catalog, &route.pins, now);
        let ranked = restrict(
            routing::rank(candidates, route.request.policy, route.request.difficulty),
            route.target,
        );
        let preferred = route
            .preferred
            .as_deref()
            .filter(|_| route.target.is_none());
        let rotation = self.rotation(preferred, now);
        let mut choice =
            routing::choose(ranked, &exclusions, preferred, &rotation).map_err(io::Error::other)?;
        if let Some(target) = route.target {
            choice.reason = format!(
                "{} → {}",
                target.token(),
                routing::describe(&choice.candidate)
            );
        }
        Ok(choice)
    }

    fn rotation(&self, preferred: Option<&str>, now: usage::Tick) -> Vec<String> {
        let Some(index) = preferred.and_then(|label| {
            self.profiles
                .iter()
                .position(|profile| profile.to_string() == label)
        }) else {
            return Vec::new();
        };
        usage::alternates(&self.profiles, index, now)
            .into_iter()
            .map(|index| self.profiles[index].to_string())
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn run_split(
        &self,
        units: &[String],
        ranked: Result<Vec<Candidate>, String>,
        exclusions: &[routing::Exclusion],
        sender: &SyncSender<Event>,
        norm_sender: &Sender<NormalizedEvent>,
        cancel: &AtomicBool,
        control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    ) -> io::Result<()> {
        let ranked = routing::require_ranked(ranked, exclusions).map_err(io::Error::other)?;
        let plan = routing::allocate(units.len(), &ranked);
        if plan.len() != units.len() {
            return Err(io::Error::other(
                "No eligible profile has headroom for every split unit.",
            ));
        }
        for (position, (unit, candidate)) in units.iter().zip(&plan).enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let usage = Usage::read(&self.profiles[candidate.profile]);
            if let Some(reason) = routing::allowance_refusal(&usage, (self.clock)()) {
                return Err(io::Error::other(format!(
                    "Split unit {} of {} stopped: {} {reason}.",
                    position + 1,
                    units.len(),
                    candidate.label
                )));
            }
            let reason = format!(
                "Unit {}/{} → {}",
                position + 1,
                units.len(),
                routing::describe(candidate)
            );
            self.dispatch(
                candidate,
                unit,
                Some(reason),
                false,
                sender,
                norm_sender,
                cancel,
                control,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &self,
        candidate: &Candidate,
        prompt: &str,
        reason: Option<String>,
        allow_resume: bool,
        sender: &SyncSender<Event>,
        norm_sender: &Sender<NormalizedEvent>,
        cancel: &AtomicBool,
        control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    ) -> io::Result<()> {
        let index = candidate.profile;
        let profile = &self.profiles[index];
        let command = profile.authenticated_command().map_err(|error| {
            io::Error::other(format!("{profile} sign-in refused at dispatch: {error}"))
        })?;
        let (options, effective_prompt) =
            prepare_launch_options(&self.options, profile, prompt, allow_resume);
        let allow_stale_retry = options.resume_session.is_some();
        let outcome = run(
            Launch {
                command,
                profile: profile.clone(),
                index,
                reason,
                configuration: candidate.configuration.clone(),
                mode: self.mode,
                options,
            },
            &self.workspace,
            effective_prompt,
            &self.profiles,
            sender,
            norm_sender,
            cancel,
            control,
            &self.active_pid,
            self.clock,
            None,
            allow_stale_retry,
        )?;
        let outcome = match outcome {
            RunOutcome::StaleSession => {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let retry_command = profile.authenticated_command().map_err(|error| {
                    io::Error::other(format!("{profile} sign-in refused at dispatch: {error}"))
                })?;
                let (retry_options, retry_prompt) =
                    prepare_launch_options(&self.options, profile, prompt, false);
                run(
                    Launch {
                        command: retry_command,
                        profile: profile.clone(),
                        index,
                        reason: None,
                        configuration: candidate.configuration.clone(),
                        mode: self.mode,
                        options: retry_options,
                    },
                    &self.workspace,
                    retry_prompt,
                    &self.profiles,
                    sender,
                    norm_sender,
                    cancel,
                    control,
                    &self.active_pid,
                    self.clock,
                    None,
                    false,
                )?
            }
            other => other,
        };
        let progress = match outcome {
            RunOutcome::Completed => return Ok(()),
            RunOutcome::StaleSession => {
                return Err(io::Error::other("Provider session could not be resumed"))
            }
            RunOutcome::RateLimited(progress) => progress,
        };
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        Usage::sync_peer_accounts(&self.profiles, profile, (self.clock)());
        if !crate::checkpoint::is_git_workspace(&self.workspace) {
            if progress.completed_tool_events > 0 {
                return Err(io::Error::other(
                    "Usage limit reached after tool activity in a non-Git workspace; mission stopped without replay.",
                ));
            }
            let Some((to, next, handoff)) = self.handoff_target(index) else {
                return Err(io::Error::other(NO_ALTERNATE));
            };
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let configuration = match handoff {
                Handoff::CrossProvider => {
                    Configuration::provider_default(self.profiles[to].provider)
                }
                Handoff::SameProvider | Handoff::AfterReset => candidate.configuration.clone(),
            };
            return self.continue_without_checkpoint(
                (index, to, next),
                &configuration,
                prompt,
                sender,
                norm_sender,
                cancel,
                control,
            );
        }
        let checkpoint = Checkpoint::save(profile, uuid::Uuid::new_v4(), &self.workspace, progress)
            .map_err(|error| {
                io::Error::other(format!(
                    "Usage limit reached; mission stopped without replay. {error}"
                ))
            })?;
        let Some((to, next, handoff)) = self.handoff_target(index) else {
            return self.snooze_until_reset(
                (index, &checkpoint),
                &candidate.configuration,
                prompt,
                sender,
                norm_sender,
                cancel,
                control,
            );
        };
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let claim =
            checkpoint.reserve_handoff(profile, &self.workspace, &self.profiles[to], handoff)?;
        let configuration = match handoff {
            Handoff::CrossProvider => Configuration::provider_default(self.profiles[to].provider),
            Handoff::SameProvider | Handoff::AfterReset => candidate.configuration.clone(),
        };
        self.continue_with(
            (index, to, next),
            &configuration,
            prompt,
            &claim,
            sender,
            norm_sender,
            cancel,
            control,
        )
    }

    fn handoff_target(&self, index: usize) -> Option<(usize, Command, Handoff)> {
        if let Some((to, command)) = self.alternate(index) {
            return Some((to, command, Handoff::SameProvider));
        }
        let to = usage::cross_provider_fallback(
            &self.profiles,
            index,
            self.limits.fallback_profile.as_deref(),
            (self.clock)(),
        )?;
        let command = self.profiles[to].authenticated_command().ok()?;
        Some((to, command, Handoff::CrossProvider))
    }

    #[allow(clippy::too_many_arguments)]
    fn snooze_until_reset(
        &self,
        interrupted: (usize, &Checkpoint),
        configuration: &Configuration,
        prompt: &str,
        sender: &SyncSender<Event>,
        norm_sender: &Sender<NormalizedEvent>,
        cancel: &AtomicBool,
        control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    ) -> io::Result<()> {
        let (index, checkpoint) = interrupted;
        if !self.limits.auto_resume_on_reset {
            return Err(io::Error::other(NO_ALTERNATE));
        }
        let profile = &self.profiles[index];
        let until = Usage::read(profile)?
            .known_reset((self.clock)())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "{NO_ALTERNATE} Reset time is unknown, so auto-resume stopped safely."
                ))
            })?;
        let claim =
            checkpoint.reserve_handoff(profile, &self.workspace, profile, Handoff::AfterReset)?;
        let _ = sender.send(Event::Snoozed {
            profile: index,
            until,
        });
        if !self.wait_for_reset(profile, cancel)? {
            return Ok(());
        }
        let command = profile.authenticated_command()?;
        self.continue_with(
            (index, index, command),
            configuration,
            prompt,
            &claim,
            sender,
            norm_sender,
            cancel,
            control,
        )
    }

    fn wait_for_reset(&self, profile: &Profile, cancel: &AtomicBool) -> io::Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let now = (self.clock)();
            let usage = Usage::read(profile)?;
            if usage.eligible_alternate(now) {
                return Ok(true);
            }
            if usage.blocked(now) && usage.known_reset(now).is_none() {
                return Err(io::Error::other(
                    "Reset time became unknown while snoozed; auto-resume stopped safely.",
                ));
            }
            thread::sleep(SNOOZE_POLL);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resume(
        &self,
        index: usize,
        prompt: &str,
        claim: Claim,
        sender: &SyncSender<Event>,
        norm_sender: &Sender<NormalizedEvent>,
        cancel: &AtomicBool,
        control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    ) -> io::Result<()> {
        let (to, profile) = self
            .profiles
            .iter()
            .enumerate()
            .find(|(_, profile)| {
                profile.to_string() == claim.alternate
                    && claim.handoff.permits(&self.profiles[index], profile)
            })
            .ok_or_else(|| io::Error::other("Claimed alternate profile is unavailable"))?;
        if !Usage::read(profile)?.eligible_alternate((self.clock)()) {
            return Err(io::Error::other(
                "Claimed alternate is latched or has an exhausted usage window",
            ));
        }
        let next = profile.authenticated_command()?;
        self.continue_with(
            (index, to, next),
            &Configuration::provider_default(profile.provider),
            prompt,
            &claim,
            sender,
            norm_sender,
            cancel,
            control,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn continue_with(
        &self,
        selection: (usize, usize, Command),
        configuration: &Configuration,
        prompt: &str,
        claim: &Claim,
        sender: &SyncSender<Event>,
        norm_sender: &Sender<NormalizedEvent>,
        cancel: &AtomicBool,
        control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    ) -> io::Result<()> {
        let (index, to, command) = selection;
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (options, effective_prompt) =
            prepare_launch_options(&self.options, &self.profiles[to], prompt, false);
        if matches!(
            run(
                Launch {
                    command,
                    profile: self.profiles[to].clone(),
                    index: to,
                    reason: None,
                    configuration: configuration.clone(),
                    mode: self.mode,
                    options,
                },
                &self.workspace,
                effective_prompt,
                &self.profiles,
                sender,
                norm_sender,
                cancel,
                control,
                &self.active_pid,
                self.clock,
                Some(Continuation::Checkpointed {
                    claim,
                    from: index,
                    profile: &self.profiles[index],
                }),
                false,
            )?,
            RunOutcome::RateLimited(_)
        ) {
            Usage::sync_peer_accounts(&self.profiles, &self.profiles[to], (self.clock)());
            return Err(io::Error::other(
                "Continuation hit a usage limit; mission stopped without replay",
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn continue_without_checkpoint(
        &self,
        selection: (usize, usize, Command),
        configuration: &Configuration,
        prompt: &str,
        sender: &SyncSender<Event>,
        norm_sender: &Sender<NormalizedEvent>,
        cancel: &AtomicBool,
        control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    ) -> io::Result<()> {
        let (index, to, command) = selection;
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (options, effective_prompt) =
            prepare_launch_options(&self.options, &self.profiles[to], prompt, false);
        if matches!(
            run(
                Launch {
                    command,
                    profile: self.profiles[to].clone(),
                    index: to,
                    reason: None,
                    configuration: configuration.clone(),
                    mode: self.mode,
                    options,
                },
                &self.workspace,
                effective_prompt,
                &self.profiles,
                sender,
                norm_sender,
                cancel,
                control,
                &self.active_pid,
                self.clock,
                Some(Continuation::Uncheckpointed { from: index }),
                false,
            )?,
            RunOutcome::RateLimited(_)
        ) {
            Usage::sync_peer_accounts(&self.profiles, &self.profiles[to], (self.clock)());
            return Err(io::Error::other(
                "Continuation hit a usage limit; mission stopped without replay",
            ));
        }
        Ok(())
    }

    fn alternate(&self, index: usize) -> Option<(usize, Command)> {
        usage::alternates(&self.profiles, index, (self.clock)())
            .into_iter()
            .find_map(|candidate| {
                let profile = &self.profiles[candidate];
                let command = profile.authenticated_command().ok()?;
                Usage::read(profile)
                    .ok()?
                    .eligible_alternate((self.clock)())
                    .then_some((candidate, command))
            })
    }
}

fn prepare_launch_options(
    base: &LaunchOptions,
    profile: &Profile,
    prompt: &str,
    allow_resume: bool,
) -> (LaunchOptions, String) {
    let mut options = base.clone();
    let label = profile.to_string();
    let resume_matches = allow_resume
        && options.resume_session.is_some()
        && options.resume_session_profile.as_deref() == Some(label.as_str());
    if resume_matches {
        options.portable_handoff = None;
        (options, prompt.to_owned())
    } else {
        options.resume_session = None;
        options.resume_session_profile = None;
        let handoff = options.portable_handoff.take();
        (options, inject_portable_handoff(prompt, handoff.as_deref()))
    }
}

fn inject_portable_handoff(prompt: &str, handoff: Option<&str>) -> String {
    let Some(handoff) = handoff.map(str::trim).filter(|text| !text.is_empty()) else {
        return prompt.to_owned();
    };
    if prompt.contains(handoff) {
        return prompt.to_owned();
    }
    if let Some(pos) = prompt.rfind("\n\nOperator: ") {
        let (head, tail) = prompt.split_at(pos);
        format!("{head}\n\n{handoff}{tail}")
    } else {
        format!("{handoff}\n\n{prompt}")
    }
}

struct Launch {
    command: Command,
    profile: Profile,
    index: usize,
    reason: Option<String>,
    configuration: Configuration,
    mode: PermissionMode,
    options: LaunchOptions,
}

enum Continuation<'a> {
    Checkpointed {
        claim: &'a Claim,
        from: usize,
        profile: &'a Profile,
    },
    Uncheckpointed {
        from: usize,
    },
}

enum RunOutcome {
    Completed,
    RateLimited(Progress),
    StaleSession,
}

#[allow(clippy::too_many_arguments)]
fn run(
    launch: Launch,
    workspace: &Path,
    prompt: String,
    peers: &[Profile],
    sender: &SyncSender<Event>,
    norm_sender: &Sender<NormalizedEvent>,
    cancel: &AtomicBool,
    control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    active_pid: &AtomicU32,
    clock: fn() -> usage::Tick,
    continuation: Option<Continuation<'_>>,
    allow_stale_retry: bool,
) -> io::Result<RunOutcome> {
    let Launch {
        mut command,
        profile,
        index,
        reason,
        configuration,
        mode,
        options,
    } = launch;
    if cancel.load(Ordering::Acquire) {
        return Ok(RunOutcome::Completed);
    }
    let resuming = options.resume_session.is_some();
    command.args(configuration.arguments());
    command.args(build_provider_arguments(profile.provider, mode, &options));
    permissions::refuse_bypass(command.get_args()).map_err(io::Error::other)?;
    command
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let prompt = match &continuation {
        Some(Continuation::Checkpointed { claim, profile, .. }) => {
            claim.start(profile, workspace, &prompt)?
        }
        Some(Continuation::Uncheckpointed { .. }) | None => prompt,
    };
    process_tree::own_group(&mut command);
    let mut child = command.spawn().inspect_err(|error| {
        let code = classify_health(&[], Some(error));
        let _ = norm_sender.send(NormalizedEvent::TurnFinished {
            interrupted: false,
            is_error: true,
            error_code: Some(code),
            final_text: error.to_string(),
        });
    })?;
    let child_pid = child.id();
    active_pid.store(child_pid, Ordering::Release);
    if let Some(next) = continuation {
        let from = match next {
            Continuation::Checkpointed { from, .. } | Continuation::Uncheckpointed { from } => from,
        };
        let _ = sender.send(Event::Continuing { from, to: index });
    }
    let stdout = child.stdout.take().expect("mission stdout is piped");
    let stderr = child.stderr.take().expect("mission stderr is piped");
    let stdout_sender = sender.clone();
    let stdout_norm = norm_sender.clone();
    let stderr_norm = norm_sender.clone();
    let _ = sender.send(Event::Running {
        profile: index,
        reason,
    });
    let limited = Arc::new(AtomicBool::new(false));
    let output_limit = Arc::clone(&limited);
    let reader_cancel = Arc::new(AtomicBool::new(false));
    let output_cancel = Arc::clone(&reader_cancel);
    let stderr_cancel = Arc::clone(&reader_cancel);
    let policy = options.policy.clone();
    let persistent_claude = profile.provider == Provider::Claude && options.persistent_stdio;

    let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let stderr_collect = Arc::clone(&stderr_lines);
    let stderr_reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if stderr_cancel.load(Ordering::Acquire) {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = stderr_norm.send(NormalizedEvent::StderrLine(trimmed.to_owned()));
            let mut guard = stderr_collect
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            push_stderr_tail(&mut guard, trimmed.to_owned());
        }
    });

    let mut stdin = child.stdin.take().expect("mission stdin is piped");
    let (writer, control_handle) = if persistent_claude {
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ControlMessage>();
        *control.lock().unwrap_or_else(PoisonError::into_inner) = Some(ctrl_tx);
        let handle = thread::spawn(move || -> io::Result<()> {
            writeln!(stdin, "{}", claude_user_turn_json(&prompt))?;
            stdin.flush()?;
            for msg in ctrl_rx {
                match msg {
                    ControlMessage::Interrupt => {
                        let req_id = uuid::Uuid::new_v4().to_string();
                        let _ = writeln!(stdin, "{}", claude_interrupt_request_json(&req_id));
                        let _ = stdin.flush();
                    }
                    ControlMessage::PermissionResponse {
                        req_id,
                        allow,
                        steer_msg,
                    } => {
                        let _ = writeln!(
                            stdin,
                            "{}",
                            claude_permission_response_json(&req_id, allow, steer_msg.as_deref())
                        );
                        let _ = stdin.flush();
                    }
                }
            }
            Ok(())
        });
        (handle, true)
    } else {
        let handle = thread::spawn(move || stdin.write_all(prompt.as_bytes()));
        (handle, false)
    };

    let reader_control = Arc::clone(control);
    let reader_profile = profile.clone();
    let reader_peers = peers.to_vec();
    let reader = thread::spawn(move || {
        read_output(
            &reader_profile,
            &reader_peers,
            stdout,
            &stdout_sender,
            &stdout_norm,
            &output_limit,
            &output_cancel,
            &policy,
            persistent_claude.then_some(reader_control),
            clock,
        )
    });

    let status = loop {
        if cancel.load(Ordering::Acquire) {
            reader_cancel.store(true, Ordering::Release);
            if control_handle {
                if let Some(tx) = control
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    let _ = tx.send(ControlMessage::Interrupt);
                }
                for _ in 0..10 {
                    if child.try_wait()?.is_some() {
                        break;
                    }
                    thread::sleep(CANCEL_POLL);
                }
            }
            active_pid.store(0, Ordering::Release);
            process_tree::kill_tree(child_pid);
            let _ = child.kill();
            let _ = child.wait();
            control
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let _ = norm_sender.send(NormalizedEvent::TurnFinished {
                interrupted: true,
                is_error: false,
                error_code: None,
                final_text: String::new(),
            });
            return Ok(RunOutcome::Completed);
        }
        if limited.load(Ordering::Acquire) {
            active_pid.store(0, Ordering::Release);
            process_tree::kill_tree(child_pid);
            let _ = child.kill();
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            active_pid.store(0, Ordering::Release);
            break status;
        }
        thread::sleep(CANCEL_POLL);
    };
    control
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let (progress, completed) = reader
        .join()
        .map_err(|_| io::Error::other("Mission output reader stopped"))??;
    let _ = stderr_reader.join();
    if limited.load(Ordering::Acquire) {
        return Ok(if completed {
            RunOutcome::Completed
        } else {
            RunOutcome::RateLimited(progress)
        });
    }
    if !status.success() {
        let captured = stderr_lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if allow_stale_retry
            && resuming
            && progress.completed_tool_events == 0
            && is_stale_session_error(&captured)
        {
            return Ok(RunOutcome::StaleSession);
        }
        if profile.provider == Provider::Gemini && is_gemini_stderr_limit(&captured) {
            let observed = clock();
            Usage::record_gemini_limit(&profile, None, observed)?;
            Usage::sync_peer_accounts(peers, &profile, observed);
            return Ok(if completed {
                RunOutcome::Completed
            } else {
                RunOutcome::RateLimited(progress)
            });
        }
        let code = classify_health(&captured, None);
        let detail = format_exit_detail(&status.to_string(), &captured);
        let _ = norm_sender.send(NormalizedEvent::TurnFinished {
            interrupted: false,
            is_error: true,
            error_code: Some(code),
            final_text: detail.clone(),
        });
        return Err(io::Error::other(detail));
    }
    writer
        .join()
        .map_err(|_| io::Error::other("Mission input writer stopped"))??;
    Ok(RunOutcome::Completed)
}

pub(crate) fn restrict(
    ranked: Result<Vec<Candidate>, String>,
    target: Option<Target>,
) -> Result<Vec<Candidate>, String> {
    let Some(target) = target else { return ranked };
    let total = ranked.as_ref().map_or(0, Vec::len);
    let mut ranked = ranked?;
    ranked.retain(|candidate| target.matches(&candidate.configuration));
    if ranked.is_empty() && total > 0 {
        return Err(format!(
            "No eligible signed-in profile or pinned model matches {}. Sign in to {} or pin a matching model with agent-command model pin.",
            target.token(),
            target.provider().binary()
        ));
    }
    Ok(ranked)
}

pub(crate) fn output_text(provider: Provider, event: &Value) -> Vec<&str> {
    match (provider, event["type"].as_str()) {
        (Provider::Claude, Some("assistant")) => event["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "text")
            .filter_map(|item| item["text"].as_str())
            .collect(),
        (Provider::Codex, Some("item.completed")) if event["item"]["type"] == "agent_message" => {
            event["item"]["text"].as_str().into_iter().collect()
        }
        (Provider::Gemini, Some("message")) if event["role"] == "assistant" => {
            event["content"].as_str().into_iter().collect()
        }
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn read_output(
    profile: &Profile,
    peers: &[Profile],
    stdout: impl io::Read,
    sender: &SyncSender<Event>,
    norm_sender: &Sender<NormalizedEvent>,
    limited: &AtomicBool,
    cancel: &AtomicBool,
    policy: &PermissionPolicy,
    control: Option<Arc<Mutex<Option<Sender<ControlMessage>>>>>,
    clock: fn() -> usage::Tick,
) -> io::Result<(Progress, bool)> {
    let mut progress = Progress::default();
    let mut completed = false;
    let mut parser = EventParser::with_profile(profile.provider, profile.to_string());
    let mut fallback_claude_limit: Option<&'static str> = None;
    for line in BufReader::new(stdout).lines() {
        if cancel.load(Ordering::Acquire) {
            return Ok((progress, completed));
        }
        let line = line?;
        if cancel.load(Ordering::Acquire) {
            return Ok((progress, completed));
        }
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let observed = clock();
        record_progress(profile.provider, &event, &mut progress);
        completed |= match profile.provider {
            Provider::Claude => {
                event["type"] == "result"
                    && event["subtype"] == "success"
                    && event["is_error"] == false
            }
            Provider::Codex => event["type"] == "turn.completed",
            Provider::Gemini => event["type"] == "result" && event["status"] == "success",
        };
        let mut suppress_limit_noise = false;
        let limit = match profile.provider {
            Provider::Claude => {
                if event["type"] == "rate_limit_event" {
                    let rejected = Usage::observe_claude(profile, &event, observed)?;
                    Usage::sync_peer_accounts(peers, profile, observed);
                    rejected
                } else if let Some(limit_type) = claude_limit_fallback(&event) {
                    suppress_limit_noise = true;
                    if fallback_claude_limit.is_none_or(str::is_empty) {
                        fallback_claude_limit = Some(limit_type);
                    }
                    if event["type"] == "result" && !limited.load(Ordering::Acquire) {
                        record_synthetic_claude_limit(
                            profile,
                            peers,
                            fallback_claude_limit.unwrap_or_default(),
                            observed,
                        )?;
                        true
                    } else {
                        false
                    }
                } else if event["type"] == "result"
                    && event["is_error"] == true
                    && fallback_claude_limit.is_some()
                {
                    suppress_limit_noise = true;
                    if !limited.load(Ordering::Acquire) {
                        record_synthetic_claude_limit(
                            profile,
                            peers,
                            fallback_claude_limit.unwrap_or_default(),
                            observed,
                        )?;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            Provider::Codex if codex_limit(&event) => {
                Usage::record_codex_limit(profile, observed)?;
                Usage::sync_peer_accounts(peers, profile, observed);
                suppress_limit_noise = true;
                true
            }
            Provider::Gemini => {
                if let Some(reset) = gemini_limit(&event) {
                    Usage::record_gemini_limit(profile, reset, observed)?;
                    Usage::sync_peer_accounts(peers, profile, observed);
                    suppress_limit_noise = true;
                    true
                } else {
                    false
                }
            }
            Provider::Codex => false,
        };
        if limit {
            limited.store(true, Ordering::Release);
        }
        let is_limited = suppress_limit_noise || limited.load(Ordering::Acquire);
        for normalized in parser.parse_value(&event, false, Some(policy)) {
            if is_limited && !matches!(normalized, NormalizedEvent::RateLimit { .. }) {
                continue;
            }
            if let (
                NormalizedEvent::PermissionAsked {
                    ref req_id,
                    ref tool,
                    ref input,
                    ..
                },
                Some(ctrl),
            ) = (&normalized, &control)
            {
                let resolved = policy.evaluate(tool, &tool_resource(tool, input), input);
                match resolved.effect {
                    PermissionEffect::Allow => {
                        if let Some(tx) =
                            ctrl.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
                        {
                            let _ = tx.send(ControlMessage::PermissionResponse {
                                req_id: req_id.clone(),
                                allow: true,
                                steer_msg: None,
                            });
                        }
                        continue;
                    }
                    PermissionEffect::Deny => {
                        if let Some(tx) =
                            ctrl.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
                        {
                            let _ = tx.send(ControlMessage::PermissionResponse {
                                req_id: req_id.clone(),
                                allow: false,
                                steer_msg: resolved.reason.clone(),
                            });
                        }
                        continue;
                    }
                    PermissionEffect::Ask => {}
                }
            }
            let _ = norm_sender.send(normalized);
        }
        if profile.provider == Provider::Claude && event["type"] == "result" {
            if let Some(ctrl) = &control {
                ctrl.lock().unwrap_or_else(PoisonError::into_inner).take();
            }
        }
        if is_limited {
            continue;
        }
        for text in output_text(profile.provider, &event) {
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let safe = line
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(500)
                    .collect();
                if sender.send(Event::Output(safe)).is_err() {
                    return Ok((progress, completed));
                }
            }
        }
    }
    if !completed && !limited.load(Ordering::Acquire) {
        if let Some(limit_type) = fallback_claude_limit {
            record_synthetic_claude_limit(profile, peers, limit_type, clock())?;
            limited.store(true, Ordering::Release);
        }
    }
    Ok((progress, completed))
}

fn record_synthetic_claude_limit(
    profile: &Profile,
    peers: &[Profile],
    limit_type: &str,
    observed: usage::Tick,
) -> io::Result<()> {
    let mut info = json!({ "status": "rejected" });
    if !limit_type.is_empty() {
        info["rateLimitType"] = json!(limit_type);
    }
    let synthetic = json!({
        "type": "rate_limit_event",
        "rate_limit_info": info,
    });
    let _ = Usage::observe_claude(profile, &synthetic, observed)?;
    Usage::sync_peer_accounts(peers, profile, observed);
    Ok(())
}

fn record_progress(provider: Provider, event: &Value, progress: &mut Progress) {
    match (provider, event["type"].as_str()) {
        (Provider::Claude, Some("assistant")) => {
            if claude_limit_fallback(event).is_none() {
                progress.assistant_messages += 1;
            }
        }
        (Provider::Claude, Some("user")) => {
            let tools = event["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "tool_result" && item["is_error"] != true);
            progress.completed_tool_events += tools.count() as u64;
        }
        (Provider::Codex, Some("item.completed")) => match event["item"]["type"].as_str() {
            Some("agent_message") => progress.assistant_messages += 1,
            Some("command_execution" | "file_change" | "mcp_tool_call")
                if event["item"]["status"] == "completed" =>
            {
                progress.completed_tool_events += 1
            }
            _ => {}
        },
        (Provider::Gemini, Some("message")) if event["role"] == "assistant" => {
            progress.assistant_messages += 1;
        }
        (Provider::Gemini, Some("tool_result")) if event["status"] == "success" => {
            progress.completed_tool_events += 1;
        }
        _ => {}
    }
}

fn classify_claude_limit_window(text: &str) -> &'static str {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("weekly") || lowered.contains("7-day") || lowered.contains("7d") {
        "seven_day"
    } else if lowered.contains("5-hour")
        || lowered.contains("5h")
        || lowered.contains("session limit")
    {
        "five_hour"
    } else {
        ""
    }
}

fn is_claude_limit_text(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("you've hit your")
        || lowered.contains("you\u{2019}ve hit your")
        || lowered.contains("weekly limit")
        || lowered.contains("usage limit reached")
        || lowered.contains("usage limit exceeded")
        || lowered.contains("rate limit reached")
        || lowered.contains("rate limit exceeded")
}

fn claude_limit_fallback(event: &Value) -> Option<&'static str> {
    match event["type"].as_str() {
        Some("assistant") => {
            let text = output_text(Provider::Claude, event).join("\n");
            if event["error"] == "rate_limit" || (!text.is_empty() && is_claude_limit_text(&text)) {
                Some(classify_claude_limit_window(&text))
            } else {
                None
            }
        }
        Some("result") if event["is_error"] == true => {
            let text = event["result"].as_str().unwrap_or_default();
            is_claude_limit_text(text).then(|| classify_claude_limit_window(text))
        }
        _ => None,
    }
}

fn gemini_limit(event: &Value) -> Option<Option<i64>> {
    let is_error = event["type"] == "error"
        || (event["type"] == "result" && event["status"].as_str().is_some_and(|s| s != "success"));
    if !is_error {
        return None;
    }
    let code_429 =
        event["code"].as_i64() == Some(429) || event["error"]["code"].as_i64() == Some(429);
    let combined = [
        event["message"].as_str(),
        event["status"].as_str(),
        event["error"]["message"].as_str(),
        event["error"]["status"].as_str(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();
    let matched = code_429
        || combined.contains("resource_exhausted")
        || combined.contains("quota exceeded")
        || combined.contains("rate limit")
        || combined.contains("too many requests")
        || combined.contains("usage limit")
        || combined.contains("429");
    if !matched {
        return None;
    }
    let reset = event["resetsAt"]
        .as_i64()
        .or_else(|| event["resetAt"].as_i64())
        .or_else(|| event["error"]["resetsAt"].as_i64())
        .or_else(|| event["error"]["resetAt"].as_i64());
    Some(reset)
}

fn is_gemini_stderr_limit(stderr_lines: &[String]) -> bool {
    let joined = stderr_lines.join("\n").to_ascii_lowercase();
    joined.contains("resource_exhausted")
        || joined.contains("quota exceeded")
        || joined.contains("rate limit")
        || joined.contains("too many requests")
        || joined.contains("status 429")
        || joined.contains("http 429")
}

fn is_stale_session_error(stderr_lines: &[String]) -> bool {
    let joined = stderr_lines.join("\n").to_ascii_lowercase();
    const SIGNALS: [&str; 9] = [
        "no conversation found with session id",
        "session not found",
        "no such session",
        "conversation not found",
        "thread not found",
        "no such thread",
        "unknown session",
        "invalid session",
        "cannot resume session",
    ];
    SIGNALS.iter().any(|signal| joined.contains(signal))
}

fn codex_limit(event: &Value) -> bool {
    if event["type"] == "error" || event["type"] == "turn.failed" {
        let error = if event["type"] == "error" {
            event
        } else {
            &event["error"]
        };
        return error["code"] == "usage_limit_reached"
            || error["message"].as_str().is_some_and(|message| {
                message.starts_with("You've hit your usage limit")
                    || message.starts_with("You’ve hit your usage limit")
            });
    }
    false
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn builds_resume_and_persistent_arguments_for_all_three_providers() {
        let options = LaunchOptions {
            resume_session: Some("sess-123".into()),
            mcp_config: Some(PathBuf::from("/tmp/mcp.json")),
            extra_args: vec![],
            policy: PermissionPolicy::for_mode(PermissionMode::WorkspaceAuto),
            persistent_stdio: true,
            ..LaunchOptions::default()
        };
        let claude =
            build_provider_arguments(Provider::Claude, PermissionMode::WorkspaceAuto, &options);
        assert!(claude.contains(&"--input-format".to_owned()));
        assert!(claude.contains(&"--permission-prompt-tool".to_owned()));
        assert!(claude.contains(&"stdio".to_owned()));
        assert!(claude
            .windows(2)
            .any(|pair| pair == ["--resume", "sess-123"]));
        assert!(claude
            .windows(2)
            .any(|pair| pair == ["--mcp-config", "/tmp/mcp.json"]));

        let codex =
            build_provider_arguments(Provider::Codex, PermissionMode::WorkspaceAuto, &options);
        assert_eq!(codex[0], "exec");
        assert_eq!(codex[1], "resume");
        assert!(codex.contains(&"sess-123".to_owned()));
        assert_eq!(codex.last().map(String::as_str), Some("-"));

        let gemini =
            build_provider_arguments(Provider::Gemini, PermissionMode::WorktreeAuto, &options);
        assert!(gemini.windows(2).any(|pair| pair == ["-o", "stream-json"]));
        assert!(gemini
            .windows(2)
            .any(|pair| pair == ["--approval-mode", "yolo"]));
        assert!(gemini
            .windows(2)
            .any(|pair| pair == ["--resume", "sess-123"]));
    }

    #[test]
    fn parses_claude_codex_and_gemini_normalized_events_and_diffs_cumulative_cost() {
        let mut claude = EventParser::new(Provider::Claude);
        let init = claude.parse_value(
            &json!({"type":"system","subtype":"init","session_id":"c-1","model":"claude-opus-4"}),
            false,
            None,
        );
        assert_eq!(
            init,
            vec![NormalizedEvent::SessionStarted {
                session_id: "c-1".into(),
                model: Some("claude-opus-4".into()),
                profile: None,
            }]
        );

        let perm = claude.parse_value(
            &json!({
                "type":"control_request",
                "request_id":"req-9",
                "request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"git push --force"}}
            }),
            false,
            None,
        );
        assert!(matches!(
            &perm[0],
            NormalizedEvent::PermissionAsked { req_id, prod_gate_reason: Some(_), .. } if req_id == "req-9"
        ));

        let turn1 = claude.parse_value(
            &json!({"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.10,"result":"done"}),
            false,
            None,
        );
        assert!(matches!(
            &turn1[0],
            NormalizedEvent::UsageDelta { cost_usd: Some(c), .. } if (*c - 0.10).abs() < 1e-6
        ));
        let turn2 = claude.parse_value(
            &json!({"type":"result","subtype":"error_during_execution","is_error":true,"total_cost_usd":0.15}),
            true,
            None,
        );
        assert!(matches!(
            &turn2[0],
            NormalizedEvent::UsageDelta { cost_usd: Some(c), .. } if (*c - 0.05).abs() < 1e-6
        ));
        assert!(matches!(
            &turn2[1],
            NormalizedEvent::TurnFinished {
                interrupted: true,
                is_error: false,
                ..
            }
        ));

        let mut codex = EventParser::new(Provider::Codex);
        let started = codex.parse_value(
            &json!({"type":"thread.started","thread_id":"t-42"}),
            false,
            None,
        );
        assert_eq!(
            started,
            vec![NormalizedEvent::SessionStarted {
                session_id: "t-42".into(),
                model: None,
                profile: None,
            }]
        );
        let changed = codex.parse_value(
            &json!({"type":"item.completed","item":{"id":"i1","type":"file_change","changes":[{"path":"src/lib.rs","kind":"update"}],"status":"completed"}}),
            false,
            None,
        );
        assert_eq!(
            changed,
            vec![NormalizedEvent::FileChanged {
                path: "src/lib.rs".into(),
                kind: PatchKind::Update,
            }]
        );

        let mut gemini = EventParser::new(Provider::Gemini);
        let g_init = gemini.parse_value(
            &json!({"type":"init","session_id":"g-7","model":"gemini-2.5-pro"}),
            false,
            None,
        );
        assert_eq!(
            g_init,
            vec![NormalizedEvent::SessionStarted {
                session_id: "g-7".into(),
                model: Some("gemini-2.5-pro".into()),
                profile: None,
            }]
        );
        let g_msg = gemini.parse_value(
            &json!({"type":"message","role":"assistant","content":"Hello from Gemini","delta":true}),
            false,
            None,
        );
        assert_eq!(
            g_msg,
            vec![NormalizedEvent::TextDelta("Hello from Gemini".into())]
        );
    }

    #[test]
    fn encodes_claude_stdio_interrupt_and_permission_control_messages() {
        let irq: Value = serde_json::from_str(&claude_interrupt_request_json("irq-1")).unwrap();
        assert_eq!(irq["type"], "control_request");
        assert_eq!(irq["request"]["subtype"], "interrupt");

        let allow: Value =
            serde_json::from_str(&claude_permission_response_json("r-1", true, None)).unwrap();
        assert_eq!(allow["response"]["response"]["behavior"], "allow");

        let deny: Value = serde_json::from_str(&claude_permission_response_json(
            "r-2",
            false,
            Some("Use --dry-run instead"),
        ))
        .unwrap();
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert_eq!(
            deny["response"]["response"]["message"],
            "Use --dry-run instead"
        );
    }

    #[test]
    fn classifies_health_codes_accurately() {
        assert_eq!(
            classify_health(
                &[],
                Some(&io::Error::new(io::ErrorKind::NotFound, "no bin"))
            ),
            HealthCode::AgentMissing
        );
        assert_eq!(
            classify_health(
                &[],
                Some(&io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            ),
            HealthCode::AgentBlocked
        );
        assert_eq!(
            classify_health(&["Error: invalid_grant OAuth token expired".into()], None),
            HealthCode::AgentAuth
        );
        assert_eq!(
            classify_health(
                &[
                    "Warning: 256-color support not detected. Using a terminal with at least 256-color support is recommended for a better visual experience.".into(),
                    "Ripgrep is not available. Falling back to GrepTool.".into(),
                    "An unexpected critical error occurred: This client is no longer supported for Gemini Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity suite of products: https://antigravity.google".into(),
                ],
                None
            ),
            HealthCode::AgentAuth
        );
        assert_eq!(
            classify_health(
                &["Please migrate to the Antigravity suite of products".into()],
                None
            ),
            HealthCode::AgentAuth
        );
        assert_eq!(
            classify_health(&["This client is no longer supported".into()], None),
            HealthCode::AgentAuth
        );
        assert_eq!(
            classify_health(
                &[
                    "(node:1234) DeprecationWarning: punycode is deprecated; please migrate to userland".into(),
                    "fatal: disk full".into(),
                ],
                None
            ),
            HealthCode::AgentFailed
        );
        assert_eq!(
            classify_health(&["fatal: disk full".into()], None),
            HealthCode::AgentFailed
        );
    }

    #[test]
    fn stderr_error_summary_filters_benign_warnings_prioritizes_critical_lines_and_falls_back() {
        let gemini_live = vec![
            "Warning: 256-color support not detected. Using a terminal with at least 256-color support is recommended for a better visual experience.".to_owned(),
            "Ripgrep is not available. Falling back to GrepTool.".to_owned(),
            "An unexpected critical error occurred: This client is no longer supported for Gemini Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity suite of products: https://antigravity.google".to_owned(),
        ];
        assert_eq!(
            format_exit_detail("exit code: 1", &gemini_live),
            "Provider exited with exit code: 1: An unexpected critical error occurred: This client is no longer supported for Gemini Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity suite of products: https://antigravity.google"
        );

        let mixed_chatter = vec![
            "Warning: 256-color support not detected.".to_owned(),
            "Initializing MCP servers...".to_owned(),
            "Error: request rejected by upstream".to_owned(),
            "    at main (cli.js:12:4)".to_owned(),
        ];
        assert_eq!(
            format_exit_detail("exit code: 1", &mixed_chatter),
            "Provider exited with exit code: 1: Error: request rejected by upstream"
        );

        let non_critical_only = vec![
            "Ripgrep is not available. Falling back to GrepTool.".to_owned(),
            "Connection reset by peer".to_owned(),
        ];
        assert_eq!(
            format_exit_detail("exit code: 2", &non_critical_only),
            "Provider exited with exit code: 2: Connection reset by peer"
        );

        let all_benign = vec![
            "Warning: 256-color support not detected.".to_owned(),
            "Ripgrep is not available. Falling back to GrepTool.".to_owned(),
        ];
        assert_eq!(
            format_exit_detail("exit code: 1", &all_benign),
            "Provider exited with exit code: 1: Warning: 256-color support not detected. | Ripgrep is not available. Falling back to GrepTool."
        );

        assert_eq!(
            format_exit_detail("exit code: 1", &[]),
            "Provider exited with exit code: 1"
        );
    }

    #[test]
    fn stderr_tail_preserves_fatal_lines_over_trailing_benign_warnings() {
        let mut tail = Vec::new();
        push_stderr_tail(&mut tail, "Error: fatal failure".to_owned());
        for i in 0..(STDERR_TAIL_LIMIT + 5) {
            push_stderr_tail(
                &mut tail,
                format!("Warning: 256-color support not detected. ({i})"),
            );
        }
        assert_eq!(tail.len(), STDERR_TAIL_LIMIT);
        assert!(tail.iter().any(|line| line == "Error: fatal failure"));
        assert_eq!(summarize_stderr_for_error(&tail), "Error: fatal failure");

        let mut all_benign = Vec::new();
        for i in 0..(STDERR_TAIL_LIMIT + 3) {
            push_stderr_tail(&mut all_benign, format!("Ripgrep is not available. ({i})"));
        }
        assert_eq!(all_benign.len(), STDERR_TAIL_LIMIT);
        assert!(all_benign
            .last()
            .is_some_and(|line| line.ends_with(&format!("({})", STDERR_TAIL_LIMIT + 2))));
    }

    #[test]
    fn process_tree_kill_terminates_shell_and_grandchild_cleanly() {
        let mut command = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "ping", "-n", "20", "127.0.0.1"]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 20"]);
            cmd
        };
        let mut child = process_tree::own_group(&mut command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        thread::sleep(Duration::from_millis(120));
        let started = std::time::Instant::now();
        process_tree::kill_tree(child.id());
        let _ = child.wait().unwrap();
        let remaining: Vec<String> = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .collect();
        let _ = remaining;
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn parses_realistic_claude_partial_stream_with_thinking_write_bash_and_text_without_duplicates()
    {
        let stream = [
            json!({"type":"system","subtype":"init","session_id":"sess-gridfall","model":"claude-opus-4-6"}),
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan the HTML5 canvas game."}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_stop","index":0}}),
            json!({"type":"assistant","message":{"id":"msg_1","content":[
                {"type":"thinking","thinking":"Plan the HTML5 canvas game."}
            ]}}),
            json!({"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_write","name":"Write","input":{}}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"C:\\\\Users\\\\j\\\\gridfall.html\","}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"content\":\"<!doctype html>\"}"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_stop","index":1}}),
            json!({"type":"assistant","message":{"id":"msg_1","content":[
                {"type":"thinking","thinking":"Plan the HTML5 canvas game."},
                {"type":"tool_use","id":"toolu_write","name":"Write","input":{"file_path":"C:\\Users\\j\\gridfall.html","content":"<!doctype html>"}}
            ]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_write","content":"File created successfully","is_error":false}]}}),
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_2"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_bash","name":"Bash","input":{}}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"start C:\\\\Users\\\\j\\\\gridfall.html\"}"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_stop","index":0}}),
            json!({"type":"assistant","message":{"id":"msg_2","content":[
                {"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"start C:\\Users\\j\\gridfall.html"}}
            ]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_bash","content":null,"is_error":false}]},"tool_use_result":{"stdout":"","stderr":"Launched browser"}}),
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_3"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Built "}}}),
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Gridfall!"}}}),
            json!({"type":"stream_event","event":{"type":"content_block_stop","index":0}}),
            json!({"type":"assistant","message":{"id":"msg_3","content":[{"type":"text","text":"Built Gridfall!"}]}}),
            json!({"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.04,"result":"Built Gridfall!"}),
        ];

        let mut parser = EventParser::new(Provider::Claude);
        let mut events = Vec::new();
        for item in &stream {
            events.extend(parser.parse_value(item, false, None));
        }

        let thinking_text: String = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking_text, "Plan the HTML5 canvas game.");

        let write_starts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ToolStart {
                    call_id, summary, ..
                } if call_id == "toolu_write" => Some(summary.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            write_starts,
            ["Write", "Write: C:\\Users\\j\\gridfall.html"]
        );

        let bash_starts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ToolStart {
                    call_id, summary, ..
                } if call_id == "toolu_bash" => Some(summary.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            bash_starts,
            ["Bash", "Bash: start C:\\Users\\j\\gridfall.html"]
        );

        assert!(events.iter().any(|e| matches!(
            e,
            NormalizedEvent::ToolEnd {
                call_id,
                ok: true,
                output_preview,
                ..
            } if call_id == "toolu_write" && output_preview == "File created successfully"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            NormalizedEvent::ToolEnd {
                call_id,
                ok: true,
                output_preview,
                ..
            } if call_id == "toolu_bash" && output_preview == "Launched browser"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            NormalizedEvent::FileChanged { path, kind: PatchKind::Add }
            if path == "C:\\Users\\j\\gridfall.html"
        )));

        let streamed_text: String = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed_text, "Built Gridfall!");
    }

    #[test]
    fn parses_codex_incremental_updates_and_gemini_deltas_without_duplication() {
        let mut codex = EventParser::new(Provider::Codex);
        let codex_stream = [
            json!({"type":"thread.started","thread_id":"th-9"}),
            json!({"type":"item.started","item":{"id":"r1","type":"reasoning","text":"Check"}}),
            json!({"type":"item.updated","item":{"id":"r1","type":"reasoning","text":"Check files"}}),
            json!({"type":"item.completed","item":{"id":"r1","type":"reasoning","text":"Check files."}}),
            json!({"type":"item.started","item":{"id":"c1","type":"command_execution","command":"cargo test"}}),
            json!({"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"cargo test","aggregated_output":"ok","exit_code":0,"status":"completed"}}),
            json!({"type":"item.started","item":{"id":"f1","type":"file_change","changes":[{"path":"src/app.rs","kind":"update"}]}}),
            json!({"type":"item.completed","item":{"id":"f1","type":"file_change","changes":[{"path":"src/app.rs","kind":"update"}],"status":"completed"}}),
            json!({"type":"item.updated","item":{"id":"m1","type":"agent_message","text":"All "}}),
            json!({"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"All green!"}}),
            json!({"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5}}),
        ];
        let mut codex_events = Vec::new();
        for item in &codex_stream {
            codex_events.extend(codex.parse_value(item, false, None));
        }
        let reasoning: String = codex_events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "Check files.");
        let msg: String = codex_events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(msg, "All green!");
        assert!(codex_events
            .iter()
            .any(|e| matches!(e, NormalizedEvent::ToolStart { call_id, .. } if call_id == "c1")));
        assert!(codex_events.iter().any(
            |e| matches!(e, NormalizedEvent::ToolEnd { call_id, ok: true, .. } if call_id == "c1")
        ));
        assert!(codex_events
            .iter()
            .any(|e| matches!(e, NormalizedEvent::ToolStart { call_id, .. } if call_id == "f1")));
        assert!(codex_events.iter().any(
            |e| matches!(e, NormalizedEvent::ToolEnd { call_id, ok: true, .. } if call_id == "f1")
        ));

        let mut gemini = EventParser::new(Provider::Gemini);
        let gemini_stream = [
            json!({"type":"init","session_id":"gem-1","model":"gemini-2.5-pro"}),
            json!({"type":"thought","content":"Inspect workspace"}),
            json!({"type":"tool_use","tool_id":"gt1","tool_name":"write_file","parameters":{"file_path":"game.html"}}),
            json!({"type":"tool_result","tool_id":"gt1","status":"success","output":"Wrote game.html"}),
            json!({"type":"message","role":"assistant","content":"Done ","delta":true}),
            json!({"type":"message","role":"assistant","content":"now.","delta":true}),
            json!({"type":"message","role":"assistant","content":"Done now.","delta":false}),
            json!({"type":"result","status":"success","stats":{"input_tokens":20,"cached":0,"output_tokens":8}}),
        ];
        let mut gem_events = Vec::new();
        for item in &gemini_stream {
            gem_events.extend(gemini.parse_value(item, false, None));
        }
        let gem_text: String = gem_events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(gem_text, "Done now.");
        assert!(gem_events
            .iter()
            .any(|e| matches!(e, NormalizedEvent::ThinkingDelta(t) if t == "Inspect workspace")));
        assert!(gem_events
            .iter()
            .any(|e| matches!(e, NormalizedEvent::ToolStart { call_id, summary, .. } if call_id == "gt1" && summary == "write_file: game.html")));
        assert!(gem_events.iter().any(|e| matches!(
            e,
            NormalizedEvent::FileChanged {
                path,
                kind: PatchKind::Add
            } if path == "game.html"
        )));
    }

    struct TempState {
        parent: PathBuf,
        root: PathBuf,
    }

    impl TempState {
        fn new(label: &str) -> Self {
            let parent = std::env::temp_dir().join(format!(
                "agent-command-mission-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&parent).unwrap();
            let root = parent.join("state");
            Self { parent, root }
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.parent);
        }
    }

    #[test]
    fn event_parser_with_profile_tags_session_started_for_all_providers() {
        let mut claude = EventParser::with_profile(Provider::Claude, "claude/main".into());
        assert_eq!(
            claude.parse_value(
                &json!({"type":"system","subtype":"init","session_id":"c-9","model":"claude-opus-4"}),
                false,
                None,
            ),
            vec![NormalizedEvent::SessionStarted {
                session_id: "c-9".into(),
                model: Some("claude-opus-4".into()),
                profile: Some("claude/main".into()),
            }]
        );

        let mut codex = EventParser::with_profile(Provider::Codex, "codex/work".into());
        assert_eq!(
            codex.parse_value(
                &json!({"type":"thread.started","thread_id":"t-9"}),
                false,
                None,
            ),
            vec![NormalizedEvent::SessionStarted {
                session_id: "t-9".into(),
                model: None,
                profile: Some("codex/work".into()),
            }]
        );

        let mut gemini = EventParser::with_profile(Provider::Gemini, "gemini/default".into());
        assert_eq!(
            gemini.parse_value(
                &json!({"type":"init","session_id":"g-9","model":"gemini-2.5-pro"}),
                false,
                None,
            ),
            vec![NormalizedEvent::SessionStarted {
                session_id: "g-9".into(),
                model: Some("gemini-2.5-pro".into()),
                profile: Some("gemini/default".into()),
            }]
        );
    }

    #[test]
    fn event_parser_claude_rate_limit_handles_top_level_rejected_and_unified_windows() {
        let mut parser = EventParser::with_profile(Provider::Claude, "claude/main".into());

        let rejected_top = parser.parse_value(
            &json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": "seven_day_opus",
                    "resetsAt": 1_795_683_600
                }
            }),
            false,
            None,
        );
        assert_eq!(
            rejected_top,
            vec![NormalizedEvent::RateLimit {
                five_hour_pct: None,
                seven_day_pct: Some(100.0),
                resets_at: Some("1795683600".into()),
            }]
        );

        let unified = parser.parse_value(
            &json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": "seven_day",
                    "resetsAt": 1_795_683_600,
                    "unifiedWindows": {
                        "five_hour": { "utilization": 0.42 },
                        "seven_day_overage_included": { "utilization": 0.99 }
                    }
                }
            }),
            false,
            None,
        );
        assert_eq!(
            unified,
            vec![NormalizedEvent::RateLimit {
                five_hour_pct: Some(42.0),
                seven_day_pct: Some(100.0),
                resets_at: Some("1795683600".into()),
            }]
        );
    }

    #[test]
    fn prepare_launch_options_guards_resume_by_profile_and_injects_portable_handoff() {
        let temp = TempState::new("prepare-launch");
        let claude_main = Profile::new(temp.root.clone(), Provider::Claude, "main").unwrap();
        let claude_default = Profile::system_default(temp.root.clone(), Provider::Claude);
        let codex_default = Profile::system_default(temp.root.clone(), Provider::Codex);
        let gemini_default = Profile::system_default(temp.root.clone(), Provider::Gemini);
        let prompt = "System context\n\nOperator: Populate my inbox";
        let base = LaunchOptions {
            resume_session: Some("f855ba63-1291-4822-bff2-07702cae2889".into()),
            resume_session_profile: Some("claude/main".into()),
            portable_handoff: Some("Recent conversation:\nUser: hi\nAssistant: hello".into()),
            ..LaunchOptions::default()
        };

        let (kept, kept_prompt) = prepare_launch_options(&base, &claude_main, prompt, true);
        assert_eq!(
            kept.resume_session.as_deref(),
            Some("f855ba63-1291-4822-bff2-07702cae2889")
        );
        assert_eq!(kept.portable_handoff, None);
        assert_eq!(kept_prompt, prompt);

        for other in [&claude_default, &codex_default, &gemini_default] {
            let (rotated, rotated_prompt) = prepare_launch_options(&base, other, prompt, true);
            assert_eq!(rotated.resume_session, None);
            assert_eq!(rotated.resume_session_profile, None);
            assert_eq!(
                rotated_prompt,
                "System context\n\nRecent conversation:\nUser: hi\nAssistant: hello\n\nOperator: Populate my inbox"
            );
        }

        let (disallowed, disallowed_prompt) =
            prepare_launch_options(&base, &claude_main, prompt, false);
        assert_eq!(disallowed.resume_session, None);
        assert!(disallowed_prompt.contains("Recent conversation:"));

        let untagged = LaunchOptions {
            resume_session: Some("legacy-id".into()),
            resume_session_profile: None,
            portable_handoff: Some("Summary".into()),
            ..LaunchOptions::default()
        };
        let (safe, safe_prompt) = prepare_launch_options(&untagged, &claude_main, "plain", true);
        assert_eq!(safe.resume_session, None);
        assert_eq!(safe_prompt, "Summary\n\nplain");
        assert_eq!(
            inject_portable_handoff(&safe_prompt, Some("Summary")),
            safe_prompt
        );
    }

    #[test]
    fn stale_session_and_limit_fallback_detectors_classify_provider_errors() {
        assert!(is_stale_session_error(&[
            "No conversation found with session ID: f855ba63-1291-4822-bff2-07702cae2889".into()
        ]));
        assert!(is_stale_session_error(&["Error: thread not found".into()]));
        assert!(is_stale_session_error(&["no such thread".into()]));
        assert!(is_stale_session_error(&["Session not found".into()]));
        assert!(is_stale_session_error(&["no such session".into()]));
        assert!(!is_stale_session_error(&["fatal: disk full".into()]));

        let rate_msg = json!({
            "type": "assistant",
            "error": "rate_limit",
            "message": {
                "content": [{"type": "text", "text": "You've hit your weekly limit · resets 3am (America/Chicago)"}]
            }
        });
        assert_eq!(claude_limit_fallback(&rate_msg), Some("seven_day"));
        let mut progress = Progress::default();
        record_progress(Provider::Claude, &rate_msg, &mut progress);
        assert_eq!(progress.assistant_messages, 0);

        let normal_msg = json!({
            "type": "assistant",
            "message": { "content": [{"type": "text", "text": "Done"}] }
        });
        assert_eq!(claude_limit_fallback(&normal_msg), None);
        record_progress(Provider::Claude, &normal_msg, &mut progress);
        assert_eq!(progress.assistant_messages, 1);

        let tool_ok = json!({
            "type": "user",
            "message": { "content": [{"type": "tool_result", "is_error": false}] }
        });
        record_progress(Provider::Claude, &tool_ok, &mut progress);
        assert_eq!(progress.completed_tool_events, 1);

        assert_eq!(
            gemini_limit(&json!({
                "type": "error",
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "resetsAt": 12_345
            })),
            Some(Some(12_345))
        );
        assert_eq!(
            gemini_limit(&json!({"type": "error", "message": "network timeout"})),
            None
        );
        assert!(is_gemini_stderr_limit(&[
            "API Error: 429 RESOURCE_EXHAUSTED quota exceeded".into()
        ]));
        assert!(!is_gemini_stderr_limit(&["invalid_grant".into()]));
    }

    #[test]
    fn read_output_suppresses_limit_noise_and_records_claude_fallback_and_gemini_limits() {
        let temp = TempState::new("read-output-limits");
        let claude = Profile::new(temp.root.clone(), Provider::Claude, "main").unwrap();
        let gemini = Profile::new(temp.root.clone(), Provider::Gemini, "main").unwrap();
        claude.prepare().unwrap();
        gemini.prepare().unwrap();
        let peers = [claude.clone(), gemini.clone()];
        let policy = PermissionPolicy::for_mode(PermissionMode::WorkspaceAuto);
        let fixed_clock = || usage::tick(5_000, 0);

        let claude_fallback_stream = [
            json!({
                "type": "assistant",
                "error": "rate_limit",
                "message": {"content": [{"type": "text", "text": "You've hit your weekly limit · resets 3am"}]}
            }),
            json!({
                "type": "result",
                "is_error": true,
                "result": "You've hit your weekly limit · resets 3am"
            }),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let (tx, rx) = mpsc::sync_channel(16);
        let (ntx, nrx) = mpsc::channel();
        let limited = AtomicBool::new(false);
        let cancel = AtomicBool::new(false);
        let (progress, completed) = read_output(
            &claude,
            &peers,
            claude_fallback_stream.as_bytes(),
            &tx,
            &ntx,
            &limited,
            &cancel,
            &policy,
            None,
            fixed_clock,
        )
        .unwrap();
        assert!(limited.load(Ordering::Acquire));
        assert!(!completed);
        assert_eq!(progress.assistant_messages, 0);
        assert!(rx.try_recv().is_err());
        assert!(nrx.try_recv().is_err());
        assert!(Usage::read(&claude).unwrap().blocked(fixed_clock()));

        let claude_real = Profile::new(temp.root.clone(), Provider::Claude, "work").unwrap();
        claude_real.prepare().unwrap();
        let claude_ordered_stream = [
            json!({
                "type": "assistant",
                "error": "rate_limit",
                "message": {"content": [{"type": "text", "text": "You've hit your weekly limit"}]}
            }),
            json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": "seven_day",
                    "resetsAt": 9_000
                }
            }),
            json!({
                "type": "result",
                "is_error": true,
                "result": "You've hit your weekly limit"
            }),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let (tx2, rx2) = mpsc::sync_channel(16);
        let (ntx2, nrx2) = mpsc::channel();
        let limited2 = AtomicBool::new(false);
        read_output(
            &claude_real,
            std::slice::from_ref(&claude_real),
            claude_ordered_stream.as_bytes(),
            &tx2,
            &ntx2,
            &limited2,
            &cancel,
            &policy,
            None,
            fixed_clock,
        )
        .unwrap();
        assert!(limited2.load(Ordering::Acquire));
        assert!(rx2.try_recv().is_err());
        let norm_events: Vec<_> = nrx2.try_iter().collect();
        assert_eq!(
            norm_events,
            vec![NormalizedEvent::RateLimit {
                five_hour_pct: None,
                seven_day_pct: Some(100.0),
                resets_at: Some("9000".into()),
            }]
        );
        assert_eq!(
            Usage::read(&claude_real)
                .unwrap()
                .known_reset(fixed_clock()),
            Some(9_000)
        );

        let gemini_stream = json!({
            "type": "error",
            "code": 429,
            "status": "RESOURCE_EXHAUSTED",
            "resetsAt": 8_500
        })
        .to_string();
        let (tx3, _rx3) = mpsc::sync_channel(16);
        let (ntx3, nrx3) = mpsc::channel();
        let limited3 = AtomicBool::new(false);
        read_output(
            &gemini,
            &peers,
            gemini_stream.as_bytes(),
            &tx3,
            &ntx3,
            &limited3,
            &cancel,
            &policy,
            None,
            fixed_clock,
        )
        .unwrap();
        assert!(limited3.load(Ordering::Acquire));
        assert!(nrx3.try_recv().is_err());
        assert!(Usage::read(&gemini).unwrap().blocked(fixed_clock()));
    }

    #[test]
    fn rotation_returns_same_provider_alternates_even_when_preferred_is_unlatched() {
        let temp = TempState::new("rotation-unlatched");
        let profiles = vec![
            Profile::new(temp.root.clone(), Provider::Claude, "main").unwrap(),
            Profile::system_default(temp.root.clone(), Provider::Claude),
            Profile::system_default(temp.root.clone(), Provider::Codex),
        ];
        let execution = Execution {
            profiles,
            workspace: temp.parent.clone(),
            clock: || usage::tick(1_000, 0),
            limits: LimitPolicy::default(),
            mode: PermissionMode::WorkspaceAuto,
            options: LaunchOptions::default(),
            active_pid: Arc::new(AtomicU32::new(0)),
        };
        assert_eq!(
            execution.rotation(Some("claude/main"), usage::tick(1_000, 0)),
            vec!["claude/default".to_owned()]
        );
    }

    #[test]
    fn select_route_reconciles_peer_accounts_before_routing_and_falls_back_to_gemini() {
        let temp = TempState::new("reconcile-before-route");
        let earlier = usage::tick(100_000, 0);
        let later = usage::tick(157_600, 57_600);

        let claude_main = Profile::new(temp.root.clone(), Provider::Claude, "main").unwrap();
        let claude_backup = Profile::new(temp.root.clone(), Provider::Claude, "backup").unwrap();
        let codex_main = Profile::new(temp.root.clone(), Provider::Codex, "main").unwrap();
        let codex_backup = Profile::new(temp.root.clone(), Provider::Codex, "backup").unwrap();
        let gemini_work = Profile::new(temp.root.clone(), Provider::Gemini, "work").unwrap();
        for profile in [
            &claude_main,
            &claude_backup,
            &codex_main,
            &codex_backup,
            &gemini_work,
        ] {
            profile.prepare().unwrap();
        }

        let shared_claude =
            r#"{"oauthAccount":{"accountUuid":"acct-shared","organizationUuid":"org-1"}}"#;
        std::fs::write(
            temp.root.join("claude").join("main").join(".claude.json"),
            shared_claude,
        )
        .unwrap();
        std::fs::write(
            temp.root.join("claude").join("backup").join(".claude.json"),
            shared_claude,
        )
        .unwrap();

        let gemini_dir = temp.root.join("gemini").join("work").join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        std::fs::write(
            gemini_dir.join("settings.json"),
            r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        )
        .unwrap();
        std::fs::write(
            gemini_dir.join("oauth_creds.json"),
            r#"{"access_token":"fixture-access-token"}"#,
        )
        .unwrap();
        std::fs::write(
            gemini_dir.join("google_accounts.json"),
            r#"{"active":"fixture@example.invalid"}"#,
        )
        .unwrap();

        Usage::observe_claude(
            &claude_backup,
            &json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "allowed",
                    "unifiedWindows": {
                        "seven_day": {"utilization": 0.77, "resetsAt": later.wall + 32_000}
                    }
                }
            }),
            earlier,
        )
        .unwrap();
        Usage::observe_claude(
            &claude_main,
            &json!({
                "type": "rate_limit_event",
                "rate_limit_info": {
                    "status": "rejected",
                    "rateLimitType": "seven_day",
                    "resetsAt": later.wall + 32_000
                }
            }),
            later,
        )
        .unwrap();
        Usage::record_codex_limit(&codex_main, later).unwrap();
        Usage::record_codex_limit(&codex_backup, later).unwrap();

        let execution = Execution {
            profiles: vec![
                claude_main.clone(),
                claude_backup.clone(),
                codex_main,
                codex_backup,
                gemini_work,
            ],
            workspace: temp.parent.clone(),
            clock: || usage::tick(157_600, 57_600),
            limits: LimitPolicy::default(),
            mode: PermissionMode::WorkspaceAuto,
            options: LaunchOptions::default(),
            active_pid: Arc::new(AtomicU32::new(0)),
        };

        assert!(!Usage::read(&claude_backup).unwrap().blocked(later));
        assert_eq!(
            execution.rotation(Some("claude/main"), later),
            vec!["claude/backup".to_owned()]
        );

        let route = Route::single("Populate my inbox", Some("claude/main".into()));
        let choice = execution.select_route(&route, later).unwrap();

        assert!(Usage::read(&claude_backup).unwrap().blocked(later));
        assert!(execution.rotation(Some("claude/main"), later).is_empty());
        assert_eq!(choice.candidate.label, "gemini/work");
        assert!(
            choice
                .reason
                .starts_with("Override claude/main skipped: latched · Auto → gemini/work"),
            "unexpected routing reason: {}",
            choice.reason
        );
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::usage::tests::Fixture;
    use serde_json::json;

    const FAKE_NOW: i64 = 1_000;

    fn prefer(profiles: &[Profile], prompt: &str) -> Route {
        Route::single(prompt, Some(profiles[0].to_string()))
    }

    fn git_workspace(fixture: &Fixture) -> PathBuf {
        let workspace = fixture.root.join("work");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("completed"), "").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "completed"],
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-qm",
                "Fixture",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .current_dir(&workspace)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        workspace
    }

    pub(crate) fn limit_fixture(name: &str, now: usage::Tick) -> (Fixture, PathBuf, Vec<Profile>) {
        let fixture = Fixture::new(
            name,
            r#"
profile=${CLAUDE_CONFIG_DIR##*/}
work="${CLAUDE_CONFIG_DIR%/profiles/claude/*}/work"
if test "$1" = auth; then
    if test "$profile" = standby && test -f "$work/.git/change-after-checkpoint" \
        && ls "${CLAUDE_CONFIG_DIR%/claude/*}"/agent-command-checkpoints-v1/*.json >/dev/null 2>&1; then
        printf 'changed\n' >> "$work/completed"
    fi
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
printf '%s %s\n' "$profile" "$*" >> "$work/.git/arguments"
prompt=$(read_prompt "$@")
if test "$profile" = first; then
    case "$prompt" in *"Keep the finished step") ;; *) exit 7 ;; esac
    printf 'done\n' >> completed
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Finished the first step"}]}}'
    printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","content":"fixture-credential-never-persist"}]}}'
    if test -f .git/completed-before-limit; then
        printf '%s\n' '{"type":"result","subtype":"success","is_error":false}'
    fi
    printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":4102444800}}'
    exit 1
fi
test "$(cat completed)" = done || exit 8
if test -f untracked; then
    case "$prompt" in
      *'"kind":"untracked","path":"untracked","index_sha256":null,"worktree_sha256":"373ca25fde04545677f483dcec193a407a9e61ba46dec587ba51f7275332b19d"'*)
        case "$prompt" in *"synthetic file contents"*|*"fixture-credential-never-persist"*) exit 11;; esac
        ;;
      *) exit 10 ;;
    esac
fi
case "$prompt" in
  *"do not redo completed work"*"Retained workspace manifest:"*'"kind":"tracked","path":"completed","index_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","worktree_sha256":"d117fa006ba9208500b2930ce69cbde436c647afa917cb7396a9bc9111a46dd2"'*"Keep the finished step"*) ;;
  *) exit 9 ;;
esac
if test -f .git/reject-continuation; then
    printf 'rejected\n' >> completed
    printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day","utilization":1,"resetsAt":4102444800}}'
    exit 1
fi
printf 'continued\n' >> completed
"#,
        );
        let workspace = git_workspace(&fixture);
        let profiles = vec![
            fixture.profile(Provider::Claude, "first"),
            fixture.profile(Provider::Claude, "standby"),
        ];
        Usage::observe_claude(
            &profiles[1],
            &json!({
                "type":"rate_limit_event","rate_limit_info":{
                    "status":"allowed_warning","rateLimitType":"seven_day",
                    "utilization":0.97,"resetsAt":now.wall + 8000
                }
            }),
            now,
        )
        .unwrap();
        (fixture, workspace, profiles)
    }

    fn drain(mission: &Mission) -> (Vec<(usize, usize)>, io::Result<()>) {
        let mut links = Vec::new();
        loop {
            match mission.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Continuing { from, to } => links.push((from, to)),
                Event::Finished(result) => return (links, result),
                _ => {}
            }
        }
    }

    #[test]
    fn stale_weekly_only_alternate_continues_once() {
        let (_fixture, workspace, profiles) = limit_fixture("continue", usage::tick(FAKE_NOW, 0));
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
            || usage::tick(FAKE_NOW + 901, 901),
        );
        let (links, result) = drain(&mission);
        result.unwrap();
        assert_eq!(links, [(0, 1)]);
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\ncontinued\n"
        );
        assert!(Usage::read(&profiles[0])
            .unwrap()
            .blocked(usage::tick(FAKE_NOW, 0)));
        let directory = profiles[0].checkpoint_directory().unwrap();
        let paths: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(paths.len(), 3);
        let checkpoint_path = paths
            .iter()
            .find(|path| path.extension().unwrap() == "json")
            .unwrap();
        let bytes = std::fs::read_to_string(checkpoint_path).unwrap();
        for excluded in [
            "fixture-credential-never-persist",
            "Finished the first step",
            "Keep the finished step",
        ] {
            assert!(!bytes.contains(excluded));
        }
        let checkpoint: Checkpoint = serde_json::from_str(&bytes).unwrap();
        for _ in 0..2 {
            let error = checkpoint
                .reserve(&profiles[0], &workspace, &profiles[1])
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("already claimed for this checkpoint by claude/standby"));
        }
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\ncontinued\n"
        );
    }

    const SNOOZE_SCRIPT: &str = r#"
home=${CLAUDE_CONFIG_DIR:-$CODEX_HOME}
profile=${home##*/}
work="${home%/profiles/*}/work"
if test "$1" = auth; then
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
for arg; do
    if test "$arg" = login; then
        printf 'Logged in using ChatGPT\n'
        exit
    fi
done
printf '%s %s\n' "$profile" "$*" >> "$work/.git/arguments"
prompt=$(read_prompt "$@")
if ! test -f "$work/.git/limited"; then
    : > "$work/.git/limited"
    printf 'done\n' >> completed
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Started"}]}}'
    printf '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"%s}}\n' "$(cat "$work/.git/reset" 2>/dev/null)"
    exit 1
fi
case "$prompt" in *"Retained workspace manifest:"*"Keep the finished step"*) ;; *) exit 9 ;; esac
printf '%s\n' "$profile" >> "$work/.git/resumed-by"
if test -n "$CODEX_HOME"; then
    printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Codex resumed"}}' '{"type":"turn.completed"}'
fi
"#;

    fn snooze_fixture(
        name: &str,
        reset: Option<i64>,
        names: &[(Provider, &str)],
    ) -> (Fixture, PathBuf, Vec<Profile>) {
        let fixture = Fixture::new(name, SNOOZE_SCRIPT);
        let workspace = git_workspace(&fixture);
        if let Some(reset) = reset {
            std::fs::write(
                workspace.join(".git/reset"),
                format!(",\"resetsAt\":{reset}"),
            )
            .unwrap();
        }
        let profiles = names
            .iter()
            .map(|(provider, name)| fixture.profile(*provider, name))
            .collect();
        (fixture, workspace, profiles)
    }

    fn limited_route(profiles: &[Profile], limits: LimitPolicy) -> Route {
        Route {
            limits,
            ..prefer(profiles, "Keep the finished step")
        }
    }

    fn auto_resume() -> LimitPolicy {
        LimitPolicy {
            auto_resume_on_reset: true,
            fallback_profile: None,
        }
    }

    fn until_snoozed(mission: &Mission) -> (usize, i64) {
        loop {
            match mission.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Snoozed { profile, until } => return (profile, until),
                Event::Finished(result) => panic!("finished before snoozing: {result:?}"),
                _ => {}
            }
        }
    }

    fn resumed_by(workspace: &std::path::Path) -> Option<String> {
        std::fs::read_to_string(workspace.join(".git/resumed-by")).ok()
    }

    fn claims(profiles: &[Profile]) -> crate::checkpoint::Census {
        crate::checkpoint::census(profiles[0].root()).unwrap()
    }

    #[test]
    fn snoozes_before_reset_and_resumes_once_after_the_fake_clock_passes_it() {
        static NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(FAKE_NOW);
        fn clock() -> usage::Tick {
            let wall = NOW.load(Ordering::SeqCst);
            usage::tick(wall, (wall - FAKE_NOW) as u64)
        }
        let (_fixture, workspace, profiles) = snooze_fixture(
            "snooze-resume",
            Some(FAKE_NOW + 60),
            &[(Provider::Claude, "solo"), (Provider::Codex, "backup")],
        );
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            limited_route(&profiles, auto_resume()),
            workspace.clone(),
            clock,
        );
        assert_eq!(until_snoozed(&mission), (0, FAKE_NOW + 60));
        thread::sleep(SNOOZE_POLL * 3);
        assert!(matches!(
            mission.events.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(resumed_by(&workspace), None);
        assert_eq!(claims(&profiles).claimed, 1);
        NOW.store(FAKE_NOW + 61, Ordering::SeqCst);
        let (links, result) = drain(&mission);
        result.unwrap();
        assert_eq!(links, [(0, 0)]);
        assert_eq!(resumed_by(&workspace).as_deref(), Some("solo\n"));
        let census = claims(&profiles);
        assert_eq!((census.checkpoints, census.launched), (1, 1));
    }

    #[test]
    fn workspace_mutated_during_snooze_refuses_to_resume() {
        static NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(FAKE_NOW);
        fn clock() -> usage::Tick {
            let wall = NOW.load(Ordering::SeqCst);
            usage::tick(wall, (wall - FAKE_NOW) as u64)
        }
        let (_fixture, workspace, profiles) = snooze_fixture(
            "snooze-mutated",
            Some(FAKE_NOW + 60),
            &[(Provider::Claude, "solo")],
        );
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            limited_route(&profiles, auto_resume()),
            workspace.clone(),
            clock,
        );
        until_snoozed(&mission);
        std::fs::write(workspace.join("completed"), "edited while snoozed\n").unwrap();
        NOW.store(FAKE_NOW + 61, Ordering::SeqCst);
        let (links, result) = drain(&mission);
        assert!(links.is_empty());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Workspace changed since the interruption"));
        assert_eq!(resumed_by(&workspace), None);
        assert_eq!(claims(&profiles).launched, 0);
    }

    #[test]
    fn unknown_reset_or_disabled_auto_resume_stops_without_snoozing() {
        for (case, reset, limits, expected) in [
            (
                "unknown",
                None,
                auto_resume(),
                "Reset time is unknown, so auto-resume stopped safely.",
            ),
            (
                "disabled",
                Some(FAKE_NOW + 60),
                LimitPolicy::default(),
                NO_ALTERNATE,
            ),
        ] {
            let (_fixture, workspace, profiles) = snooze_fixture(
                &format!("snooze-{case}"),
                reset,
                &[(Provider::Claude, "solo")],
            );
            let mission = Mission::launch_with_clock(
                profiles.clone(),
                limited_route(&profiles, limits),
                workspace.clone(),
                || usage::tick(FAKE_NOW, 0),
            );
            let mut snoozed = false;
            let result = loop {
                match mission.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                    Event::Snoozed { .. } => snoozed = true,
                    Event::Finished(result) => break result,
                    _ => {}
                }
            };
            assert!(!snoozed, "{case}");
            assert!(
                result.unwrap_err().to_string().ends_with(expected),
                "{case}"
            );
            assert_eq!(resumed_by(&workspace), None);
            assert_eq!(claims(&profiles).claimed, 0);
        }
    }

    #[test]
    fn a_reopened_snooze_claim_resumes_after_reset_in_a_new_process() {
        static NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(FAKE_NOW);
        fn clock() -> usage::Tick {
            let wall = NOW.load(Ordering::SeqCst);
            usage::tick(wall, (wall - FAKE_NOW) as u64)
        }
        let (_fixture, workspace, profiles) = snooze_fixture(
            "snooze-reopen",
            Some(FAKE_NOW + 60),
            &[(Provider::Claude, "solo")],
        );
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            limited_route(&profiles, auto_resume()),
            workspace.clone(),
            clock,
        );
        until_snoozed(&mission);
        drop(mission);
        let (index, claim) = Claim::pending(&profiles, &workspace).unwrap().unwrap();
        assert_eq!((index, claim.handoff), (0, Handoff::AfterReset));
        let resumed = Mission::resume(
            profiles.clone(),
            index,
            workspace.clone(),
            "Keep the finished step".into(),
            claim,
        );
        let (links, result) = drain(&resumed);
        result.unwrap();
        assert_eq!(links, [(0, 0)]);
        assert_eq!(resumed_by(&workspace).as_deref(), Some("solo\n"));
    }

    #[test]
    fn cross_provider_fallback_continues_only_when_no_same_provider_alternate_exists() {
        let fallback = LimitPolicy {
            auto_resume_on_reset: false,
            fallback_profile: Some("codex/backup".into()),
        };
        let (_fixture, workspace, profiles) = snooze_fixture(
            "cross-provider",
            Some(4_102_444_800),
            &[(Provider::Claude, "solo"), (Provider::Codex, "backup")],
        );
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            limited_route(&profiles, fallback.clone()),
            workspace.clone(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        result.unwrap();
        assert_eq!(links, [(0, 1)]);
        assert_eq!(resumed_by(&workspace).as_deref(), Some("backup\n"));
        let arguments = std::fs::read_to_string(workspace.join(".git/arguments")).unwrap();
        let codex = arguments
            .lines()
            .find(|line| line.starts_with("backup "))
            .unwrap();
        assert!(codex.contains("exec --json") && !codex.contains("--model"));
        let (_fixture, workspace, profiles) = snooze_fixture(
            "cross-provider-sibling",
            Some(4_102_444_800),
            &[
                (Provider::Claude, "solo"),
                (Provider::Codex, "backup"),
                (Provider::Claude, "sibling"),
            ],
        );
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            limited_route(&profiles, fallback),
            workspace.clone(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        result.unwrap();
        assert_eq!(links, [(0, 2)]);
        assert_eq!(resumed_by(&workspace).as_deref(), Some("sibling\n"));
    }

    #[test]
    fn exhausted_latched_or_corrupt_alternates_never_start() {
        for (case, status, utilization) in [
            ("latched", "rejected", 0.5),
            ("exhausted", "allowed_warning", 1.0),
            ("over-limit", "allowed_warning", 1.02),
            ("corrupt", "allowed", 0.5),
        ] {
            let now = usage::tick(FAKE_NOW, 0);
            let (_fixture, workspace, profiles) = limit_fixture(case, now);
            Usage::observe_claude(
                &profiles[1],
                &json!({"type":"rate_limit_event","rate_limit_info":{
                    "status":status,"rateLimitType":"seven_day",
                    "utilization":utilization,"resetsAt":FAKE_NOW + 3600
                }}),
                now,
            )
            .unwrap();
            if case == "corrupt" {
                std::fs::write(
                    profiles[1].usage_directory().unwrap().join("state.json"),
                    b"{",
                )
                .unwrap();
            }
            let route = prefer(&profiles, "Keep the finished step");
            let mission = Mission::launch_with_clock(profiles, route, workspace.clone(), || {
                usage::tick(FAKE_NOW, 0)
            });
            let (links, result) = drain(&mission);
            assert!(links.is_empty(), "{case}");
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("No eligible signed-in alternate"),
                "{case}"
            );
            assert_eq!(
                std::fs::read_to_string(workspace.join("completed")).unwrap(),
                "done\n"
            );
        }
    }

    #[test]
    fn rejected_unobserved_continuation_latches_and_does_not_try_third_profile() {
        let (fixture, workspace, mut profiles) =
            limit_fixture("rejected-continuation", usage::tick(FAKE_NOW, 0));
        std::fs::remove_file(profiles[1].usage_directory().unwrap().join("state.json")).unwrap();
        profiles.push(fixture.profile(Provider::Claude, "third"));
        std::fs::write(workspace.join(".git/reject-continuation"), "").unwrap();
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        assert_eq!(links, [(0, 1)]);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Continuation hit a usage limit"));
        assert!(Usage::read(&profiles[1])
            .unwrap()
            .blocked(usage::tick(FAKE_NOW, 0)));
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\nrejected\n"
        );
    }

    #[test]
    fn separate_process_shows_and_resumes_pending_claim_once_or_abandons_it() {
        for abandon in [false, true] {
            let tag = if abandon {
                "restart-abandon"
            } else {
                "restart-resume"
            };
            let (fixture, workspace, profiles) = limit_fixture(tag, usage::now());
            std::fs::write(workspace.join("completed"), "done\n").unwrap();
            std::fs::write(workspace.join("untracked"), "synthetic file contents").unwrap();
            let checkpoint = Checkpoint::save(
                &profiles[0],
                uuid::Uuid::new_v4(),
                &workspace,
                Progress::default(),
            )
            .unwrap();
            let claim = checkpoint
                .reserve(&profiles[0], &workspace, &profiles[1])
                .unwrap();
            std::fs::remove_file(profiles[1].usage_directory().unwrap().join("state.json"))
                .unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "mission::tests::recovery_child", "--nocapture"])
                .current_dir(&workspace)
                .env("SUB_AUTH_DIR", fixture.root.join("profiles"))
                .env("AC_RECOVERY_TEST_PROVIDER", &fixture.executable)
                .env(
                    "AC_RECOVERY_TEST_ACTION",
                    if abandon { "abandon" } else { "resume" },
                )
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "{}{}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            assert!(Claim::pending(&profiles, &workspace).unwrap().is_none());
            assert!(claim
                .start(&profiles[0], &workspace, "Keep the finished step")
                .is_err());
            let expected = if abandon {
                "done\n"
            } else {
                "done\ncontinued\n"
            };
            assert_eq!(
                std::fs::read_to_string(workspace.join("completed")).unwrap(),
                expected
            );
            for entry in std::fs::read_dir(profiles[0].checkpoint_directory().unwrap()).unwrap() {
                let path = entry.unwrap().path();
                let content = std::fs::read_to_string(path).unwrap();
                for excluded in [
                    "Keep the finished step",
                    "fixture-credential-never-persist",
                    "synthetic file contents",
                ] {
                    assert!(!content.contains(excluded));
                }
            }
        }
    }

    #[test]
    fn recovery_child() {
        let Some(executable) = std::env::var_os("AC_RECOVERY_TEST_PROVIDER") else {
            return;
        };
        let root = crate::auth::root().unwrap();
        let workspace = std::env::current_dir().unwrap();
        let profiles = ["first", "standby"].map(|name| {
            Profile::named_fixture(&root, Provider::Claude, name, PathBuf::from(&executable))
        });
        let mut app = crate::App::live(workspace);
        let live = app.live.as_mut().unwrap();
        live.profiles = profiles.into();
        live.pending = Claim::pending(&live.profiles, &live.workspace).unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let notice: String = (0..80).map(|x| buffer[(x, 23)].symbol()).collect();
        assert!(
            notice.contains("Interrupted cont") && notice.contains("F4 Abandon"),
            "{notice}"
        );
        if std::env::var("AC_RECOVERY_TEST_ACTION").unwrap() == "abandon" {
            let button = app.live.as_ref().unwrap().abandon_area;
            app.click(button.x, button.y);
            assert!(app.notice.contains("abandoned"));
            return;
        }
        app.click(app.input_area.x + 1, app.input_area.y + 1);
        assert!(app.editing);
        app.composer.set_text("Keep the finished step");
        app.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.mission_running() {
            assert!(std::time::Instant::now() < deadline);
            app.poll_mission();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.status(0), "DONE", "{}", app.notice);
    }

    #[test]
    fn a_completed_mission_never_creates_a_continuation() {
        let (_fixture, workspace, profiles) = limit_fixture("completed", usage::tick(FAKE_NOW, 0));
        std::fs::write(workspace.join(".git/completed-before-limit"), "").unwrap();
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        result.unwrap();
        assert!(links.is_empty());
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\n"
        );
        assert_eq!(
            std::fs::read_dir(profiles[0].checkpoint_directory().unwrap())
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn changed_workspace_refuses_the_cross_profile_continuation() {
        let (_fixture, workspace, profiles) =
            limit_fixture("changed-workspace", usage::tick(FAKE_NOW, 0));
        std::fs::write(workspace.join(".git/change-after-checkpoint"), "").unwrap();
        let mission = Mission::launch_with_clock(
            profiles.clone(),
            prefer(&profiles, "Keep the finished step"),
            workspace.clone(),
            || usage::tick(FAKE_NOW, 0),
        );
        let (links, result) = drain(&mission);
        assert!(links.is_empty());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Workspace changed since the interruption"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("completed")).unwrap(),
            "done\nchanged\n"
        );
        assert_eq!(
            std::fs::read_dir(profiles[0].checkpoint_directory().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn a_new_mission_rotates_once_to_the_first_authorized_alternate() {
        let fixture = Fixture::new(
            "launch-alternate",
            r#"
profile=${CLAUDE_CONFIG_DIR##*/}
if test "$1" = auth; then
    test "$profile" != alpha || exit 1
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
test "$(cat)" = "New work" || exit 9
printf '%s\n' "$profile" >> starts
"#,
        );
        let profiles = ["first", "zebra", "bravo", "alpha"]
            .map(|name| fixture.profile(Provider::Claude, name));
        let now = usage::now();
        let mut event = json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected", "resetsAt": now.wall + 3600,
                "unifiedWindows": {
                    "five_hour": {"utilization": 0.5, "resetsAt": now.wall + 3600},
                    "seven_day": {"utilization": 0.5, "resetsAt": now.wall + 3600}
                }
            }
        });
        Usage::observe_claude(&profiles[0], &event, now).unwrap();
        event["rate_limit_info"]["status"] = json!("allowed");
        for profile in &profiles[1..] {
            Usage::observe_claude(profile, &event, now).unwrap();
        }
        let route = prefer(&profiles, "New work");
        let mission = Mission::launch(profiles.into(), route, fixture.root.clone());
        assert!(matches!(
            mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Running {
                profile: 2,
                reason: Some(reason)
            } if reason.starts_with("Override claude/first skipped: latched · rotated → claude/bravo")
        ));
        assert!(matches!(
            mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Finished(Ok(()))
        ));
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("starts")).unwrap(),
            "bravo\n"
        );
    }

    #[test]
    fn provider_limit_stops_without_replaying_mission() {
        let fixture = Fixture::new(
            "interrupted",
            r#"
case "$1" in
    auth) printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'; exit ;;
    -c)
        shift 4
        if test "$1" = login; then printf '%s\n' 'Logged in using ChatGPT' >&2; exit; fi
        ;;
esac
test "$(cat)" = "Do work" || exit 9
if test "$1" = --print; then
    printf '%s\n' 'claude' >> starts
    printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":4102444800}}'
else
    printf '%s\n' 'codex' >> starts
    printf '%s\n' '{"type":"turn.failed","error":{"message":"You have a problem","code":"usage_limit_reached"}}'
fi
exit 1
"#,
        );
        let workspace = git_workspace(&fixture);
        for provider in [Provider::Claude, Provider::Codex] {
            let selected = fixture.profile(provider, "original");
            let route = prefer(std::slice::from_ref(&selected), "Do work");
            let mission = Mission::launch(vec![selected.clone()], route, workspace.clone());
            assert!(matches!(
                mission.events.recv_timeout(Duration::from_secs(3)).unwrap(),
                Event::Running {
                    profile: 0,
                    reason: Some(_)
                }
            ));
            let Event::Finished(result) =
                mission.events.recv_timeout(Duration::from_secs(3)).unwrap()
            else {
                panic!("expected interrupted outcome")
            };
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("No eligible signed-in alternate"));
            assert!(Usage::read(&selected).unwrap().blocked(usage::now()));
        }
        assert_eq!(
            std::fs::read_to_string(workspace.join("starts")).unwrap(),
            "claude\ncodex\n"
        );
    }

    const ROUTED_PROVIDER: &str = r#"
case "$1" in
    auth) printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'; exit ;;
    -c) if test "$5" = login; then printf '%s\n' 'Logged in using ChatGPT' >&2; exit; fi ;;
esac
mkdir running.lock || exit 5
prompt=$(read_prompt "$@")
profile=${CLAUDE_CONFIG_DIR##*/}
test -n "$CLAUDE_CONFIG_DIR" || profile=${CODEX_HOME##*/}
printf '%s|%s|%s\n' "$profile" "$*" "$prompt" >> starts
sleep 0.05
rmdir running.lock
"#;

    fn synthetic_source(
        endpoint: crate::catalog::Endpoint,
        rows: Vec<crate::catalog::Row>,
    ) -> crate::catalog::Source {
        use crate::catalog::{Origin, Snapshot, Source};
        Source {
            endpoint,
            origin: Origin::Cache,
            snapshot: Snapshot::sealed(
                endpoint,
                "synthetic-1".into(),
                "synthetic test fixture".into(),
                rows,
            ),
        }
    }

    fn synthetic_catalog(
        models: Vec<crate::catalog::Row>,
        agents: Vec<crate::catalog::Row>,
    ) -> Catalog {
        use crate::catalog::Endpoint;
        Catalog {
            sources: vec![
                synthetic_source(Endpoint::AgentsCoding, agents),
                synthetic_source(Endpoint::ModelsFree, models),
            ],
        }
    }

    fn coding_row(provider: &str, variant: &str, index: f64) -> crate::catalog::Row {
        crate::catalog::Row {
            provider: provider.into(),
            slug: variant.into(),
            effort: None,
            coding_index: Some(index),
            intelligence_index: None,
            speed: None,
        }
    }

    fn agent_row(provider: &str, slug: &str, effort: &str, index: f64) -> crate::catalog::Row {
        crate::catalog::Row {
            provider: provider.into(),
            slug: slug.into(),
            effort: Some(effort.into()),
            coding_index: Some(index),
            intelligence_index: None,
            speed: None,
        }
    }

    fn contrary_agent_rows() -> Vec<crate::catalog::Row> {
        vec![
            agent_row("claude", "claude-opus-5-5", "high", 90.0),
            agent_row("codex", "gpt-6-2-sol", "high", 41.0),
        ]
    }

    fn pins() -> Pins {
        use crate::routing::tests::pin;
        Pins::from_pins(vec![
            pin("claude/alpha", "claude-opus-5-5", Some("high")),
            pin("claude/first", "claude-opus-5-5", Some("high")),
            pin("codex/beta", "gpt-6.2-sol", Some("high")),
        ])
        .unwrap()
    }

    fn finish(mission: &Mission) -> (Vec<String>, io::Result<()>) {
        let mut reasons = Vec::new();
        loop {
            match mission.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Running {
                    reason: Some(reason),
                    ..
                } => reasons.push(reason),
                Event::Finished(result) => return (reasons, result),
                _ => {}
            }
        }
    }

    #[test]
    fn routed_dispatch_passes_selected_model_and_effort_to_native_cli() {
        let fixture = Fixture::new("routed-arguments", ROUTED_PROVIDER);
        let profiles = vec![
            fixture.profile(Provider::Claude, "alpha"),
            fixture.profile(Provider::Codex, "beta"),
        ];
        let rows = || {
            vec![
                coding_row("claude", "claude-opus-5-5-high", 60.0),
                coding_row("codex", "gpt-6-2-sol-high", 70.0),
            ]
        };
        let route = |difficulty, preferred: Option<&str>, rows| Route {
            request: Request {
                difficulty,
                units: vec!["Routed work".into()],
                ..Request::default()
            },
            preferred: preferred.map(str::to_owned),
            catalog: synthetic_catalog(rows, contrary_agent_rows()),
            pins: pins(),
            limits: LimitPolicy::default(),
            target: None,
            mode: PermissionMode::Supervised,
        };
        for (difficulty, preferred, expected) in [
            (
                routing::Difficulty::Hard,
                None,
                "Auto → codex/beta gpt-6.2-sol high · AA coding index 70.0 · speed n/a · headroom unknown",
            ),
            (
                routing::Difficulty::Hard,
                Some("claude/alpha"),
                "Override → claude/alpha claude-opus-5-5 high · AA coding index 60.0 · speed n/a · headroom unknown",
            ),
        ] {
            let mission = Mission::launch(
                profiles.clone(),
                route(difficulty, preferred, rows()),
                fixture.root.clone(),
            );
            let (reasons, result) = finish(&mission);
            result.unwrap();
            assert_eq!(reasons, [expected]);
        }
        let starts = std::fs::read_to_string(fixture.root.join("starts")).unwrap();
        assert_eq!(
            starts,
            "beta|-c cli_auth_credentials_store='file' -c forced_login_method='chatgpt' -c model='gpt-6.2-sol' -c model_reasoning_effort='high' -a never exec --json --skip-git-repo-check --sandbox workspace-write -|Routed work\n\
alpha|--model claude-opus-5-5 --effort high --print --verbose --output-format stream-json --setting-sources user --permission-mode default --|Routed work\n"
        );
        let below = vec![coding_row("claude", "claude-opus-5-5-high", 50.0)];
        let mission = Mission::launch(
            profiles,
            route(routing::Difficulty::Hard, None, below),
            fixture.root.clone(),
        );
        let (reasons, result) = finish(&mission);
        assert!(reasons.is_empty());
        assert_eq!(
            result.unwrap_err().to_string(),
            "No eligible model meets the hard floor 55 on the AA coding index; best is 50.0."
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("starts")).unwrap(),
            starts
        );
    }

    #[test]
    fn inline_target_overrides_the_entry_profile_and_read_only_reaches_the_cli() {
        let fixture = Fixture::new("targeted-arguments", ROUTED_PROVIDER);
        let profiles = vec![
            fixture.profile(Provider::Claude, "alpha"),
            fixture.profile(Provider::Codex, "beta"),
        ];
        let route = |target: &str| Route {
            request: Request {
                difficulty: routing::Difficulty::Hard,
                units: vec!["Routed work".into()],
                ..Request::default()
            },
            preferred: Some("claude/alpha".into()),
            catalog: synthetic_catalog(
                vec![
                    coding_row("claude", "claude-opus-5-5-high", 60.0),
                    coding_row("codex", "gpt-6-2-sol-high", 70.0),
                ],
                contrary_agent_rows(),
            ),
            pins: pins(),
            limits: LimitPolicy::default(),
            target: crate::targeting::extract(target).unwrap().target,
            mode: PermissionMode::ReadOnly,
        };
        for (token, expected) in [
            (
                "@codex",
                "@codex → codex/beta gpt-6.2-sol high · AA coding index 70.0 · speed n/a · headroom unknown",
            ),
            (
                "@opus",
                "@opus → claude/alpha claude-opus-5-5 high · AA coding index 60.0 · speed n/a · headroom unknown",
            ),
        ] {
            let mission = Mission::launch(profiles.clone(), route(token), fixture.root.clone());
            let (reasons, result) = finish(&mission);
            result.unwrap();
            assert_eq!(reasons, [expected]);
        }
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("starts")).unwrap(),
            "beta|-c cli_auth_credentials_store='file' -c forced_login_method='chatgpt' -c model='gpt-6.2-sol' -c model_reasoning_effort='high' -a never exec --json --skip-git-repo-check --sandbox read-only -|Routed work\n\
alpha|--model claude-opus-5-5 --effort high --print --verbose --output-format stream-json --setting-sources user --permission-mode default --disallowedTools Edit,Write,NotebookEdit --|Routed work\n"
        );
        let mission = Mission::launch(profiles, route("@astra"), fixture.root.clone());
        let (reasons, result) = finish(&mission);
        assert!(reasons.is_empty());
        assert!(result
            .unwrap_err()
            .to_string()
            .starts_with("No eligible signed-in profile or pinned model matches @astra."));
    }

    #[test]
    fn pinned_continuation_keeps_the_exact_model_and_effort() {
        let (_fixture, workspace, profiles) =
            limit_fixture("pinned-continuation", usage::tick(FAKE_NOW, 0));
        let route = Route {
            request: Request {
                difficulty: routing::Difficulty::Hard,
                units: vec!["Keep the finished step".into()],
                ..Request::default()
            },
            preferred: Some(profiles[0].to_string()),
            catalog: synthetic_catalog(
                vec![coding_row("claude", "claude-opus-5-5-high", 60.0)],
                contrary_agent_rows(),
            ),
            pins: pins(),
            limits: LimitPolicy::default(),
            target: None,
            mode: PermissionMode::Supervised,
        };
        let mission = Mission::launch_with_clock(profiles, route, workspace.clone(), || {
            usage::tick(FAKE_NOW + 901, 901)
        });
        let (links, result) = drain(&mission);
        result.unwrap();
        assert_eq!(links, [(0, 1)]);
        let claude = "--model claude-opus-5-5 --effort high --print --verbose --output-format stream-json --setting-sources user --permission-mode default --";
        assert_eq!(
            std::fs::read_to_string(workspace.join(".git/arguments")).unwrap(),
            format!("first {claude}\nstandby {claude}\n")
        );
    }

    #[test]
    fn split_mission_sends_each_unit_once_in_sequence_to_allocated_profiles() {
        let fixture = Fixture::new("split-mission", ROUTED_PROVIDER);
        let profiles = vec![
            fixture.profile(Provider::Claude, "big"),
            fixture.profile(Provider::Claude, "latched"),
            fixture.profile(Provider::Claude, "small"),
        ];
        let now = usage::now();
        for (profile, status, used) in [
            (&profiles[0], "allowed", 0.4),
            (&profiles[1], "rejected", 0.1),
            (&profiles[2], "allowed", 0.7),
        ] {
            Usage::observe_claude(
                profile,
                &json!({"type":"rate_limit_event","rate_limit_info":{
                    "status":status,"resetsAt":now.wall + 3600,
                    "unifiedWindows":{"five_hour":{"utilization":used,"resetsAt":now.wall + 3600}}
                }}),
                now,
            )
            .unwrap();
        }
        let request = routing::parse("/split unit a ;; unit b ;; unit c").unwrap();
        let mission = Mission::launch(
            profiles,
            Route {
                request,
                preferred: None,
                catalog: Catalog::default(),
                pins: Pins::default(),
                limits: LimitPolicy::default(),
                target: None,
                mode: PermissionMode::Supervised,
            },
            fixture.root.clone(),
        );
        let (reasons, result) = finish(&mission);
        result.unwrap();
        assert_eq!(reasons.len(), 3);
        assert!(reasons[0].starts_with("Unit 1/3 → claude/big provider default"));
        assert!(reasons[2].starts_with("Unit 3/3 → claude/small provider default"));
        let starts = std::fs::read_to_string(fixture.root.join("starts")).unwrap();
        let sent: Vec<_> = starts
            .lines()
            .map(|line| {
                let mut fields = line.split('|');
                (fields.next().unwrap(), fields.nth(1).unwrap())
            })
            .collect();
        assert_eq!(
            sent,
            [("big", "unit a"), ("big", "unit b"), ("small", "unit c")]
        );
        let none = Mission::launch(
            vec![fixture.profile(Provider::Claude, "latched")],
            Route {
                request: routing::parse("/split x ;; y").unwrap(),
                preferred: None,
                catalog: Catalog::default(),
                pins: Pins::default(),
                limits: LimitPolicy::default(),
                target: None,
                mode: PermissionMode::Supervised,
            },
            fixture.root.clone(),
        );
        assert_eq!(
            finish(&none).1.unwrap_err().to_string(),
            "No eligible signed-in subscription profile. Skipped: claude/latched latched."
        );
    }
}
