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
    collections::HashMap,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc, Mutex, PoisonError,
    },
    thread,
    time::Duration,
};

const SNOOZE_POLL: Duration = Duration::from_millis(200);
const STDERR_TAIL_LIMIT: usize = 20;
const OUTPUT_PREVIEW_CHARS: usize = 400;
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
}

impl Drop for Mission {
    fn drop(&mut self) {
        self.interrupt();
    }
}

impl Mission {
    pub fn interrupt(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(tx) = self
            .control
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(ControlMessage::Interrupt);
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
    #[cfg(all(test, unix))]
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
    #[cfg(all(test, unix))]
    pub(crate) fn held() -> (Self, std::sync::mpsc::Sender<Event>) {
        let (sender, events) = std::sync::mpsc::channel();
        let (_norm_tx, normalized) = mpsc::channel();
        let mission = Self {
            events,
            normalized,
            cancel: Arc::default(),
            control: Arc::new(Mutex::new(None)),
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
            let mut parser = EventParser::new(Provider::Claude);
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

pub fn classify_health(stderr_lines: &[String], io_error: Option<&io::Error>) -> HealthCode {
    if let Some(err) = io_error {
        match err.kind() {
            io::ErrorKind::NotFound => return HealthCode::AgentMissing,
            io::ErrorKind::PermissionDenied => return HealthCode::AgentBlocked,
            _ => {}
        }
    }
    let joined = stderr_lines.join("\n").to_ascii_lowercase();
    const AUTH_SIGNALS: [&str; 9] = [
        "not logged in",
        "authentication failed",
        "failed to authenticate",
        "unauthorized",
        "invalid api key",
        "invalid_grant",
        "oauth",
        "credentials",
        "sign in first",
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
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
    ] {
        if let Some(val) = input.get(key).and_then(Value::as_str) {
            let single_line = val.lines().next().unwrap_or(val).trim();
            let clipped: String = single_line.chars().take(120).collect();
            return format!("{name}: {clipped}");
        }
    }
    name.to_owned()
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

pub struct EventParser {
    provider: Provider,
    last_cumulative_cost_usd: f64,
    streamed_text_this_turn: bool,
    final_text: String,
    tool_names_by_id: HashMap<String, (String, Value)>,
}

impl EventParser {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            last_cumulative_cost_usd: 0.0,
            streamed_text_this_turn: false,
            final_text: String::new(),
            tool_names_by_id: HashMap::new(),
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
                    });
                }
            }
            Some("stream_event") => {
                let inner = &event["event"];
                if inner["type"] == "content_block_delta" {
                    let delta = &inner["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str() {
                                self.streamed_text_this_turn = true;
                                self.final_text.push_str(text);
                                out.push(NormalizedEvent::TextDelta(text.to_owned()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(thinking) = delta["thinking"].as_str() {
                                out.push(NormalizedEvent::ThinkingDelta(thinking.to_owned()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                if let Some(items) = event["message"]["content"].as_array() {
                    for item in items {
                        match item["type"].as_str() {
                            Some("text") => {
                                if !self.streamed_text_this_turn {
                                    if let Some(text) = item["text"].as_str() {
                                        self.final_text.push_str(text);
                                        out.push(NormalizedEvent::TextDelta(text.to_owned()));
                                    }
                                }
                            }
                            Some("thinking") => {
                                if !self.streamed_text_this_turn {
                                    if let Some(thinking) = item["thinking"].as_str() {
                                        out.push(NormalizedEvent::ThinkingDelta(
                                            thinking.to_owned(),
                                        ));
                                    }
                                }
                            }
                            Some("tool_use") => {
                                let call_id = item["id"].as_str().unwrap_or_default().to_owned();
                                let name = item["name"].as_str().unwrap_or("tool").to_owned();
                                let input = item["input"].clone();
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
                if let Some(items) = event["message"]["content"].as_array() {
                    for item in items {
                        if item["type"] == "tool_result" {
                            let call_id =
                                item["tool_use_id"].as_str().unwrap_or_default().to_owned();
                            let ok = item["is_error"] != true;
                            let content_str = match &item["content"] {
                                Value::String(text) => text.clone(),
                                Value::Array(parts) => parts
                                    .iter()
                                    .filter_map(|p| p["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                other => other.to_string(),
                            };
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
                let five = windows["five_hour"]["utilization"]
                    .as_f64()
                    .map(|u| (u * 100.0) as f32);
                let seven = windows["seven_day"]["utilization"]
                    .as_f64()
                    .map(|u| (u * 100.0) as f32);
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
                self.streamed_text_this_turn = false;
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

    fn parse_codex(&mut self, event: &Value, interrupted: bool) -> Vec<NormalizedEvent> {
        let mut out = Vec::new();
        match event["type"].as_str() {
            Some("thread.started") => {
                if let Some(thread_id) = event["thread_id"].as_str() {
                    out.push(NormalizedEvent::SessionStarted {
                        session_id: thread_id.to_owned(),
                        model: None,
                    });
                }
            }
            Some("item.started") => {
                let item = &event["item"];
                let call_id = item["id"].as_str().unwrap_or_default().to_owned();
                match item["type"].as_str() {
                    Some("command_execution") => {
                        let cmd = item["command"].as_str().unwrap_or_default();
                        out.push(NormalizedEvent::ToolStart {
                            call_id,
                            kind: ToolKind::Shell,
                            name: "command_execution".into(),
                            summary: format!("shell: {}", preview_text(cmd)),
                            raw_input: json!({ "command": cmd }),
                        });
                    }
                    Some("mcp_tool_call") => {
                        let tool = item["tool"].as_str().unwrap_or("mcp_tool").to_owned();
                        out.push(NormalizedEvent::ToolStart {
                            call_id,
                            kind: ToolKind::Mcp,
                            summary: summarize_tool(&tool, &item["arguments"]),
                            name: tool,
                            raw_input: item["arguments"].clone(),
                        });
                    }
                    Some("collab_tool_call") => {
                        let tool = item["tool"].as_str().unwrap_or("collab").to_owned();
                        out.push(NormalizedEvent::ToolStart {
                            call_id,
                            kind: ToolKind::Subagent,
                            summary: format!("subagent: {tool}"),
                            name: tool,
                            raw_input: item.clone(),
                        });
                    }
                    Some("web_search") => {
                        let query = item["query"].as_str().unwrap_or_default();
                        out.push(NormalizedEvent::ToolStart {
                            call_id,
                            kind: ToolKind::Web,
                            name: "web_search".into(),
                            summary: format!("web_search: {}", preview_text(query)),
                            raw_input: json!({ "query": query }),
                        });
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
                            self.final_text.push_str(text);
                            out.push(NormalizedEvent::TextDelta(text.to_owned()));
                        }
                    }
                    Some("reasoning") => {
                        if let Some(text) = item["text"].as_str() {
                            out.push(NormalizedEvent::ThinkingDelta(text.to_owned()));
                        }
                    }
                    Some("command_execution") => {
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
                        if let Some(changes) = item["changes"].as_array() {
                            for change in changes {
                                if let Some(path) = change["path"].as_str() {
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
                    }
                    Some("mcp_tool_call" | "collab_tool_call" | "web_search") => {
                        let ok = item["error"].is_null() && item["status"] != "failed";
                        let preview = item["error"]
                            .as_str()
                            .or_else(|| item["result"].as_str())
                            .map(preview_text)
                            .unwrap_or_else(|| preview_text(&item["result"].to_string()));
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
                if let Some(session_id) = event["session_id"].as_str() {
                    out.push(NormalizedEvent::SessionStarted {
                        session_id: session_id.to_owned(),
                        model: event["model"].as_str().map(str::to_owned),
                    });
                }
            }
            Some("message") if event["role"] == "assistant" => {
                if let Some(text) = event["content"].as_str() {
                    self.final_text.push_str(text);
                    out.push(NormalizedEvent::TextDelta(text.to_owned()));
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
        let (candidates, exclusions) =
            routing::candidates(&self.profiles, &route.catalog, &route.pins, now);
        let ranked = restrict(
            routing::rank(candidates, route.request.policy, route.request.difficulty),
            route.target,
        );
        if route.request.split {
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
        self.dispatch(
            &choice.candidate,
            &route.request.units[0],
            Some(choice.reason),
            sender,
            norm_sender,
            cancel,
            control,
        )
    }

    fn rotation(&self, preferred: Option<&str>, now: usage::Tick) -> Vec<String> {
        let Some(index) = preferred.and_then(|label| {
            self.profiles
                .iter()
                .position(|profile| profile.to_string() == label)
        }) else {
            return Vec::new();
        };
        usage::choices(&self.profiles, index, now)
            .unwrap_or_default()
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
        let limit = run(
            Launch {
                command,
                profile: profile.clone(),
                index,
                reason,
                configuration: candidate.configuration.clone(),
                mode: self.mode,
                options: self.options.clone(),
            },
            &self.workspace,
            prompt.to_owned(),
            sender,
            norm_sender,
            cancel,
            control,
            self.clock,
            None,
        )?;
        let Some(progress) = limit else { return Ok(()) };
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
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
        let mut options = self.options.clone();
        options.resume_session = None;
        if run(
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
            prompt.to_owned(),
            sender,
            norm_sender,
            cancel,
            control,
            self.clock,
            Some(Continuation {
                claim,
                from: index,
                profile: &self.profiles[index],
            }),
        )?
        .is_some()
        {
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

struct Launch {
    command: Command,
    profile: Profile,
    index: usize,
    reason: Option<String>,
    configuration: Configuration,
    mode: PermissionMode,
    options: LaunchOptions,
}

struct Continuation<'a> {
    claim: &'a Claim,
    from: usize,
    profile: &'a Profile,
}

#[allow(clippy::too_many_arguments)]
fn run(
    launch: Launch,
    workspace: &Path,
    prompt: String,
    sender: &SyncSender<Event>,
    norm_sender: &Sender<NormalizedEvent>,
    cancel: &AtomicBool,
    control: &Arc<Mutex<Option<Sender<ControlMessage>>>>,
    clock: fn() -> usage::Tick,
    continuation: Option<Continuation<'_>>,
) -> io::Result<Option<Progress>> {
    let Launch {
        mut command,
        profile,
        index,
        reason,
        configuration,
        mode,
        options,
    } = launch;
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    command.args(configuration.arguments());
    command.args(build_provider_arguments(profile.provider, mode, &options));
    permissions::refuse_bypass(command.get_args()).map_err(io::Error::other)?;
    command
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let prompt = match &continuation {
        Some(next) => next.claim.start(next.profile, workspace, &prompt)?,
        None => prompt,
    };
    let mut child = command.spawn().inspect_err(|error| {
        let code = classify_health(&[], Some(error));
        let _ = norm_sender.send(NormalizedEvent::TurnFinished {
            interrupted: false,
            is_error: true,
            error_code: Some(code),
            final_text: error.to_string(),
        });
    })?;
    if let Some(next) = continuation {
        let _ = sender.send(Event::Continuing {
            from: next.from,
            to: index,
        });
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
    let policy = options.policy.clone();
    let persistent_claude = profile.provider == Provider::Claude && options.persistent_stdio;

    let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let stderr_collect = Arc::clone(&stderr_lines);
    let stderr_reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = stderr_norm.send(NormalizedEvent::StderrLine(trimmed.to_owned()));
            let mut guard = stderr_collect
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if guard.len() >= STDERR_TAIL_LIMIT {
                guard.remove(0);
            }
            guard.push(trimmed.to_owned());
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
    let reader = thread::spawn(move || {
        read_output(
            &profile,
            stdout,
            &stdout_sender,
            &stdout_norm,
            &output_limit,
            &policy,
            persistent_claude.then_some(reader_control),
            clock,
        )
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            if control_handle {
                if let Some(tx) = control
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    let _ = tx.send(ControlMessage::Interrupt);
                }
                thread::sleep(Duration::from_millis(150));
            }
            let _ = child.kill();
            child.wait()?;
            control
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let _ = stderr_reader.join();
            return Ok(None);
        }
        if limited.load(Ordering::Acquire) {
            let _ = child.kill();
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(50));
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
        return Ok((!completed).then_some(progress));
    }
    if !status.success() {
        let captured = stderr_lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let code = classify_health(&captured, None);
        let detail = if captured.is_empty() {
            format!("Provider exited with {status}")
        } else {
            format!("Provider exited with {status}: {}", captured.join(" | "))
        };
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
    Ok(None)
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
    stdout: impl io::Read,
    sender: &SyncSender<Event>,
    norm_sender: &Sender<NormalizedEvent>,
    limited: &AtomicBool,
    policy: &PermissionPolicy,
    control: Option<Arc<Mutex<Option<Sender<ControlMessage>>>>>,
    clock: fn() -> usage::Tick,
) -> io::Result<(Progress, bool)> {
    let mut progress = Progress::default();
    let mut completed = false;
    let mut parser = EventParser::new(profile.provider);
    for line in BufReader::new(stdout).lines() {
        let line = line?;
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
        let limit = match profile.provider {
            Provider::Claude => Usage::observe_claude(profile, &event, observed)?,
            Provider::Codex if codex_limit(&event) => {
                Usage::record_codex_limit(profile, observed)?;
                true
            }
            Provider::Codex | Provider::Gemini => false,
        };
        if limit {
            limited.store(true, Ordering::Release);
        }
        for normalized in parser.parse_value(&event, false, Some(policy)) {
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
    Ok((progress, completed))
}

fn record_progress(provider: Provider, event: &Value, progress: &mut Progress) {
    match (provider, event["type"].as_str()) {
        (Provider::Claude, Some("assistant")) => progress.assistant_messages += 1,
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
            classify_health(&["fatal: disk full".into()], None),
            HealthCode::AgentFailed
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
prompt=$(cat)
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
prompt=$(cat)
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
                "{}",
                String::from_utf8_lossy(&child.stdout)
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
        app.live.as_mut().unwrap().profiles = profiles.into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let notice: String = (0..80).map(|x| buffer[(x, 22)].symbol()).collect();
        assert!(notice.contains("Interrupted continuation pending"));
        if std::env::var("AC_RECOVERY_TEST_ACTION").unwrap() == "abandon" {
            let button = app.live.as_ref().unwrap().abandon_area;
            app.click(button.x, button.y);
            assert!(app.notice.contains("abandoned"));
            return;
        }
        app.input = "Keep the finished step".into();
        app.click(app.send_area.x, app.send_area.y);
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
prompt=$(cat)
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
