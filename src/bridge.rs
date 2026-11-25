use crate::{
    control::{self, Control, Scope},
    daemon::{constant_time_equal, Token},
    http::{self, HttpError, Request},
    mcp,
    soul::SoulStore,
    specialists,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt, fs,
    io::{self, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex, PoisonError,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub const BRIDGE_SERVER_NAME: &str = "agent_command";
pub const BRIDGE_TOKEN_ENV: &str = "AGENT_COMMAND_BRIDGE_TOKEN";
pub const PROGRESS_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

const ACCEPT_POLL: Duration = Duration::from_millis(25);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const BRIDGE_HEALTH_NAME: &str = "agent-command-bridge";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeProvider {
    Claude,
    Codex,
    Gemini,
}

impl BridgeProvider {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

impl From<crate::auth::Provider> for BridgeProvider {
    fn from(provider: crate::auth::Provider) -> Self {
        match provider {
            crate::auth::Provider::Claude => Self::Claude,
            crate::auth::Provider::Codex => Self::Codex,
            crate::auth::Provider::Gemini => Self::Gemini,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeInjection {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub config_file: Option<PathBuf>,
}

pub fn worker_mcp_diet_args(provider: BridgeProvider) -> Vec<String> {
    match provider {
        BridgeProvider::Claude => vec![
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            r#"{"mcpServers":{}}"#.into(),
        ],
        BridgeProvider::Codex => vec!["-c".into(), "mcp_servers={}".into()],
        BridgeProvider::Gemini => vec!["--allowed-mcp-server-names".into(), "none".into()],
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackgroundResult {
    pub worker_id: String,
    pub role: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub workspace: String,
    pub summary: String,
    pub is_error: bool,
    pub finished_at: String,
}

impl BackgroundResult {
    pub fn format_tag(&self) -> String {
        let branch_attr = self
            .branch
            .as_deref()
            .map(|branch| format!(" branch=\"{}\"", escape_attr(branch)))
            .unwrap_or_default();
        format!(
            "<background-result kind=\"worker\" worker_id=\"{}\" role=\"{}\" status=\"{}\"{branch_attr} error=\"{}\">\n{}\n</background-result>",
            escape_attr(&self.worker_id),
            escape_attr(&self.role),
            escape_attr(&self.status),
            self.is_error,
            self.summary.trim()
        )
    }
}

fn escape_attr(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

struct WakeState {
    queue: VecDeque<BackgroundResult>,
    notifier: Option<mpsc::Sender<BackgroundResult>>,
    closed: bool,
}

#[derive(Clone)]
pub struct WakeQueue {
    inner: Arc<(Mutex<WakeState>, Condvar)>,
}

impl Default for WakeQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(WakeState {
                    queue: VecDeque::new(),
                    notifier: None,
                    closed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn with_notifier(notifier: mpsc::Sender<BackgroundResult>) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(WakeState {
                    queue: VecDeque::new(),
                    notifier: Some(notifier),
                    closed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn push(&self, result: BackgroundResult) {
        let (lock, condvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(notifier) = &state.notifier {
            let _ = notifier.send(result.clone());
        }
        state.queue.push_back(result);
        condvar.notify_all();
    }

    pub fn try_pop(&self) -> Option<BackgroundResult> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queue
            .pop_front()
    }

    pub fn drain(&self) -> Vec<BackgroundResult> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queue
            .drain(..)
            .collect()
    }

    pub fn wait_next(&self, timeout: Duration) -> Option<BackgroundResult> {
        let deadline = std::time::Instant::now() + timeout;
        let (lock, condvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(item) = state.queue.pop_front() {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, _) = condvar
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
        }
    }

    pub fn pending_len(&self) -> usize {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queue
            .len()
    }

    pub fn shutdown(&self) {
        let (lock, condvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        condvar.notify_all();
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalServerSpec {
    pub name: String,
    pub transport: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_hint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalToolSpec {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolSearchHit {
    pub server: String,
    pub name: String,
    pub description: String,
    pub connected: bool,
    pub requires_auth: bool,
    pub score: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolDescription {
    pub server: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_hint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalCallResult {
    Ok(Value),
    NeedsAuth { server: String, detail: String },
    Failed(String),
}

pub trait ExternalToolExecutor: Send + Sync {
    fn execute(&self, server: &str, tool: &str, arguments: Value) -> ExternalCallResult;
}

struct UnavailableExecutor;

impl ExternalToolExecutor for UnavailableExecutor {
    fn execute(&self, server: &str, tool: &str, _arguments: Value) -> ExternalCallResult {
        ExternalCallResult::Failed(format!(
            "External MCP server {server} has no active transport to invoke {tool}."
        ))
    }
}

struct CatalogState {
    servers: BTreeMap<String, ExternalServerSpec>,
    tools: Vec<ExternalToolSpec>,
    executor: Arc<dyn ExternalToolExecutor>,
}

#[derive(Clone)]
pub struct ExternalToolCatalog {
    state: Arc<Mutex<CatalogState>>,
}

impl Default for ExternalToolCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalToolCatalog {
    pub fn new() -> Self {
        Self::with_executor(Arc::new(UnavailableExecutor))
    }

    pub fn with_executor(executor: Arc<dyn ExternalToolExecutor>) -> Self {
        Self {
            state: Arc::new(Mutex::new(CatalogState {
                servers: BTreeMap::new(),
                tools: Vec::new(),
                executor,
            })),
        }
    }

    pub fn register_server(&self, server: ExternalServerSpec) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .servers
            .insert(server.name.to_ascii_lowercase(), server);
    }

    pub fn set_server_connected(&self, server: &str, connected: bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = state.servers.get_mut(&server.to_ascii_lowercase()) {
            entry.connected = connected;
        }
    }

    pub fn register_tool(&self, tool: ExternalToolSpec) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = tool.server.to_ascii_lowercase();
        state
            .servers
            .entry(key)
            .or_insert_with(|| ExternalServerSpec {
                name: tool.server.clone(),
                transport: "mcp".into(),
                connected: true,
                auth_hint: None,
            });
        state.tools.retain(|existing| {
            !(existing.server.eq_ignore_ascii_case(&tool.server)
                && existing.name.eq_ignore_ascii_case(&tool.name))
        });
        state.tools.push(tool);
    }

    pub fn register_tools(&self, tools: Vec<ExternalToolSpec>) {
        for tool in tools {
            self.register_tool(tool);
        }
    }

    pub fn server_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .servers
            .len()
    }

    pub fn tool_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tools
            .len()
    }

    pub fn summary(&self) -> String {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.servers.is_empty() {
            return "No external MCP servers configured.".into();
        }
        let servers: Vec<String> = state
            .servers
            .values()
            .map(|server| {
                let status = if server.connected {
                    "connected"
                } else {
                    "needs sign-in"
                };
                format!("{} ({status})", server.name)
            })
            .collect();
        format!(
            "{} external MCP tools across {} via tool_search/tool_describe/tool_call.",
            state.tools.len(),
            servers.join(", ")
        )
    }

    pub fn search(&self, query: &str, server: Option<&str>, limit: usize) -> Vec<ToolSearchHit> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let trimmed = query.trim().to_ascii_lowercase();
        let terms: Vec<&str> = trimmed.split_whitespace().collect();
        let mut hits: Vec<ToolSearchHit> = state
            .tools
            .iter()
            .filter(|tool| {
                server.is_none_or(|wanted| tool.server.eq_ignore_ascii_case(wanted.trim()))
            })
            .filter_map(|tool| {
                let score = score_tool(tool, &trimmed, &terms);
                if !terms.is_empty() && score == 0 {
                    return None;
                }
                let server_spec = state.servers.get(&tool.server.to_ascii_lowercase());
                let connected = server_spec.is_none_or(|spec| spec.connected);
                Some(ToolSearchHit {
                    server: tool.server.clone(),
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    connected,
                    requires_auth: !connected,
                    score: if terms.is_empty() { 1 } else { score },
                })
            })
            .collect();
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.server.cmp(&right.server))
                .then_with(|| left.name.cmp(&right.name))
        });
        hits.truncate(limit.clamp(1, 50));
        hits
    }

    pub fn describe(&self, server: &str, name: &str) -> Result<ToolDescription, String> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let tool = state
            .tools
            .iter()
            .find(|tool| {
                tool.server.eq_ignore_ascii_case(server.trim())
                    && tool.name.eq_ignore_ascii_case(name.trim())
            })
            .ok_or_else(|| format!("No external tool {server}/{name}. Use tool_search first."))?;
        let spec = state.servers.get(&tool.server.to_ascii_lowercase());
        Ok(ToolDescription {
            server: tool.server.clone(),
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
            connected: spec.is_none_or(|entry| entry.connected),
            auth_hint: spec.and_then(|entry| entry.auth_hint.clone()),
        })
    }

    pub fn call(&self, server: &str, name: &str, arguments: Value) -> ExternalCallResult {
        let (canonical_server, canonical_tool, executor, needs_auth) = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(tool) = state.tools.iter().find(|tool| {
                tool.server.eq_ignore_ascii_case(server.trim())
                    && tool.name.eq_ignore_ascii_case(name.trim())
            }) else {
                return ExternalCallResult::Failed(format!(
                    "No external tool {server}/{name}. Use tool_search first."
                ));
            };
            let spec = state.servers.get(&tool.server.to_ascii_lowercase());
            let auth_detail = spec.filter(|entry| !entry.connected).map(|entry| {
                entry.auth_hint.clone().unwrap_or_else(|| {
                    format!(
                        "{} requires sign-in before calling {}.",
                        entry.name, tool.name
                    )
                })
            });
            (
                tool.server.clone(),
                tool.name.clone(),
                Arc::clone(&state.executor),
                auth_detail,
            )
        };
        if let Some(detail) = needs_auth {
            return ExternalCallResult::NeedsAuth {
                server: canonical_server,
                detail,
            };
        }
        executor.execute(&canonical_server, &canonical_tool, arguments)
    }
}

fn score_tool(tool: &ExternalToolSpec, query: &str, terms: &[&str]) -> u32 {
    let name_lower = tool.name.to_ascii_lowercase();
    let server_lower = tool.server.to_ascii_lowercase();
    let desc_lower = tool.description.to_ascii_lowercase();
    let tags_lower: Vec<String> = tool
        .tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect();

    let mut score = 0u32;
    if !query.is_empty() && name_lower == query {
        score += 50;
    } else if !query.is_empty() && name_lower.contains(query) {
        score += 20;
    }
    for term in terms {
        let mut matched = false;
        if name_lower.contains(term) {
            score += 12;
            matched = true;
        }
        if server_lower.contains(term) {
            score += 8;
            matched = true;
        }
        if tags_lower.iter().any(|tag| tag.contains(term)) {
            score += 10;
            matched = true;
        }
        if desc_lower.contains(term) {
            score += 4;
            matched = true;
        }
        if !matched && is_subsequence(term, &name_lower) {
            score += 3;
        }
    }
    score
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|candidate| candidate == wanted))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AskUserRequest {
    pub request_id: String,
    pub question: String,
    pub choices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub allow_freeform: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AskUserAnswer {
    pub selected_index: Option<usize>,
    pub answer: String,
}

struct AskBrokerState {
    pending: Vec<AskUserRequest>,
    answers: BTreeMap<String, AskUserAnswer>,
    interactive: bool,
}

#[derive(Clone)]
pub struct AskUserBroker {
    inner: Arc<(Mutex<AskBrokerState>, Condvar)>,
}

impl Default for AskUserBroker {
    fn default() -> Self {
        Self::new(false)
    }
}

impl AskUserBroker {
    pub fn new(interactive: bool) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(AskBrokerState {
                    pending: Vec::new(),
                    answers: BTreeMap::new(),
                    interactive,
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn pending(&self) -> Vec<AskUserRequest> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .clone()
    }

    pub fn answer(&self, request_id: &str, answer: AskUserAnswer) -> bool {
        let (lock, condvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let existed = state
            .pending
            .iter()
            .any(|item| item.request_id == request_id);
        state.pending.retain(|item| item.request_id != request_id);
        state.answers.insert(request_id.to_owned(), answer);
        condvar.notify_all();
        existed
    }

    pub(crate) fn ask(&self, request: AskUserRequest, timeout: Duration) -> Option<AskUserAnswer> {
        let (lock, condvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let request_id = request.request_id.clone();
        state.pending.push(request);
        condvar.notify_all();
        if !state.interactive {
            return None;
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(answer) = state.answers.remove(&request_id) {
                return Some(answer);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, _) = condvar
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
        }
    }
}

pub struct BridgeSessionConfig {
    pub workspace_root: PathBuf,
    pub profile_root: PathBuf,
    pub soul: Option<SoulStore>,
    pub catalog: ExternalToolCatalog,
    pub ask_user: AskUserBroker,
    pub wake_queue: WakeQueue,
    pub shutdown: Arc<AtomicBool>,
}

type RpcHandler = Arc<dyn Fn(&str) -> Option<Value> + Send + Sync>;

pub struct LoopbackBridge {
    addr: SocketAddr,
    session_id: String,
    token: Token,
    shutdown: Arc<AtomicBool>,
    control: Option<Control>,
    temp_files: Mutex<Vec<PathBuf>>,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for LoopbackBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoopbackBridge")
            .field("addr", &self.addr)
            .field("session_id", &self.session_id)
            .field("token", &"redacted")
            .finish()
    }
}

impl LoopbackBridge {
    pub fn start_session(config: BridgeSessionConfig) -> io::Result<Self> {
        let control = Control::open_with(
            specialists::Store::new(config.profile_root.clone()),
            &config.workspace_root,
            Box::new(control::NativeEngine::new(config.profile_root)),
            Arc::clone(&config.shutdown),
            Some(config.wake_queue),
        )?;
        let mut server = mcp::Server::new(control.clone(), Scope::Commander)
            .with_catalog(config.catalog)
            .with_ask_user(config.ask_user);
        if let Some(soul) = config.soul {
            server = server.with_soul(soul);
        }
        let mut bridge = Self::start(Arc::new(server))?;
        bridge.control = Some(control);
        Ok(bridge)
    }

    pub(crate) fn start(server: Arc<mcp::Server>) -> io::Result<Self> {
        let control = server.control().clone();
        let handler: RpcHandler = Arc::new(move |text| server.handle_text(text));
        let mut bridge = Self::start_with_handler(handler)?;
        bridge.control = Some(control);
        Ok(bridge)
    }

    pub fn start_with_handler(handler: RpcHandler) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let token = Token::generate();
        let session_id = uuid::Uuid::new_v4().simple().to_string();
        let shutdown = Arc::new(AtomicBool::new(false));
        let loop_shutdown = Arc::clone(&shutdown);
        let loop_token = token.clone();
        let loop_session = session_id.clone();

        let worker = thread::spawn(move || {
            serve_loop(listener, handler, loop_token, loop_session, loop_shutdown);
        });

        Ok(Self {
            addr,
            session_id,
            token,
            shutdown,
            control: None,
            temp_files: Mutex::new(Vec::new()),
            worker: Some(worker),
        })
    }

    pub fn wake_queue(&self) -> Option<&WakeQueue> {
        self.control.as_ref().and_then(Control::wake_queue)
    }

    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.control
            .as_ref()
            .map(Control::shutdown_flag)
            .unwrap_or_else(|| Arc::clone(&self.shutdown))
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    pub fn token(&self) -> &str {
        self.token.secret()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn gemini_mcp_settings_snippet(&self) -> Value {
        json!({
            "mcpServers": {
                BRIDGE_SERVER_NAME: {
                    "httpUrl": self.url(),
                    "headers": {
                        "Authorization": format!("Bearer ${{{BRIDGE_TOKEN_ENV}}}")
                    }
                }
            }
        })
    }

    pub fn codex_stdio_injection(&self, executable: &Path) -> BridgeInjection {
        let program = executable.to_string_lossy().into_owned();
        let args_toml = format!(
            "[\"mcp\", \"--connect\", \"{}\", \"--token-env\", \"{}\"]",
            self.base_url(),
            BRIDGE_TOKEN_ENV
        );
        BridgeInjection {
            args: vec![
                "-c".into(),
                format!(
                    "mcp_servers.{BRIDGE_SERVER_NAME}.command={}",
                    toml_string(&program)
                ),
                "-c".into(),
                format!("mcp_servers.{BRIDGE_SERVER_NAME}.args={args_toml}"),
            ],
            env: vec![(BRIDGE_TOKEN_ENV.into(), self.token.secret().to_owned())],
            config_file: None,
        }
    }

    pub fn injection_for(&self, provider: BridgeProvider) -> io::Result<BridgeInjection> {
        match provider {
            BridgeProvider::Claude => {
                let config = json!({
                    "mcpServers": {
                        BRIDGE_SERVER_NAME: {
                            "type": "http",
                            "url": self.url(),
                            "headers": {
                                "Authorization": format!("Bearer {}", self.token.secret())
                            }
                        }
                    }
                });
                let path = self.write_temp_config("claude", &config)?;
                Ok(BridgeInjection {
                    args: vec!["--mcp-config".into(), path.to_string_lossy().into_owned()],
                    env: vec![(BRIDGE_TOKEN_ENV.into(), self.token.secret().to_owned())],
                    config_file: Some(path),
                })
            }
            BridgeProvider::Codex => Ok(BridgeInjection {
                args: vec![
                    "-c".into(),
                    format!("mcp_servers.{BRIDGE_SERVER_NAME}.url='{}'", self.url()),
                    "-c".into(),
                    format!(
                        "mcp_servers.{BRIDGE_SERVER_NAME}.bearer_token_env_var='{BRIDGE_TOKEN_ENV}'"
                    ),
                ],
                env: vec![(BRIDGE_TOKEN_ENV.into(), self.token.secret().to_owned())],
                config_file: None,
            }),
            BridgeProvider::Gemini => {
                let config = self.gemini_mcp_settings_snippet();
                let path = self.write_temp_config("gemini", &config)?;
                Ok(BridgeInjection {
                    args: vec![
                        "--allowed-mcp-server-names".into(),
                        BRIDGE_SERVER_NAME.into(),
                    ],
                    env: vec![(BRIDGE_TOKEN_ENV.into(), self.token.secret().to_owned())],
                    config_file: Some(path),
                })
            }
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(control) = &self.control {
            control.shutdown();
        }
        let _ = TcpStream::connect(self.addr);
    }

    fn write_temp_config(&self, prefix: &str, value: &Value) -> io::Result<PathBuf> {
        let filename = format!(
            "agent-command-mcp-{prefix}-{}-{}.json",
            self.session_id,
            uuid::Uuid::new_v4().simple()
        );
        let path = std::env::temp_dir().join(filename);
        let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        write_private_file(&path, &bytes)?;
        self.temp_files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(path.clone());
        Ok(path)
    }
}

impl Drop for LoopbackBridge {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let files = self
            .temp_files
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for path in files.iter() {
            let _ = fs::remove_file(path);
        }
    }
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn serve_loop(
    listener: TcpListener,
    handler: RpcHandler,
    token: Token,
    session_id: String,
    shutdown: Arc<AtomicBool>,
) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let handler = Arc::clone(&handler);
                let token = token.clone();
                let session_id = session_id.clone();
                workers.push(thread::spawn(move || {
                    let _ = handle_connection(stream, &handler, &token, &session_id);
                }));
                workers.retain(|handle| !handle.is_finished());
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    for handle in workers {
        let _ = handle.join();
    }
}

fn handle_connection(
    stream: TcpStream,
    handler: &RpcHandler,
    token: &Token,
    session_id: &str,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let request = match http::read_request(&mut reader) {
        Ok(request) => request,
        Err(HttpError::Closed) => return Ok(()),
        Err(error) => {
            return write_json(
                &mut writer,
                error.status(),
                &json!({"error": error.to_string()}),
            );
        }
    };
    if !authorized(&request, token) {
        return http::write_response(
            &mut writer,
            401,
            "application/json",
            &[("WWW-Authenticate", "Bearer realm=\"agent-command-bridge\"")],
            json!({"error": "unauthorized"}).to_string().as_bytes(),
        );
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => write_json(
            &mut writer,
            200,
            &json!({
                "status": "ok",
                "name": BRIDGE_HEALTH_NAME,
                "session_id": session_id,
                "version": env!("CARGO_PKG_VERSION"),
            }),
        ),
        ("POST", "/mcp") => {
            let is_json = request
                .header("content-type")
                .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"));
            if !is_json {
                return write_json(
                    &mut writer,
                    415,
                    &json!({"error": "Content-Type must be application/json"}),
                );
            }
            let text = String::from_utf8_lossy(&request.body);
            match handler(&text) {
                Some(response) => write_json(&mut writer, 200, &response),
                None => http::write_response(&mut writer, 202, "application/json", &[], b""),
            }
        }
        (_, "/health" | "/mcp") => {
            write_json(&mut writer, 405, &json!({"error": "method not allowed"}))
        }
        _ => write_json(&mut writer, 404, &json!({"error": "not found"})),
    }
}

fn authorized(request: &Request, token: &Token) -> bool {
    request
        .header("authorization")
        .and_then(|value| {
            let (scheme, credential) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(credential)
        })
        .is_some_and(|credential| {
            token.matches(credential)
                || constant_time_equal(token.secret().as_bytes(), credential.trim().as_bytes())
        })
}

fn write_json(writer: &mut impl Write, status: u16, body: &Value) -> io::Result<()> {
    http::write_response(
        writer,
        status,
        "application/json",
        &[],
        body.to_string().as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_http(addr: SocketAddr, request: &str) -> http::Response {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        http::read_response(&mut BufReader::new(stream)).unwrap()
    }

    #[test]
    fn loopback_bridge_requires_bearer_token_routes_mcp_and_cleans_up_on_drop() {
        let bridge = LoopbackBridge::start_with_handler(Arc::new(|text| {
            let value: Value = serde_json::from_str(text).ok()?;
            let id = value.get("id")?.clone();
            Some(json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}))
        }))
        .unwrap();

        assert!(bridge.addr().ip().is_loopback());
        assert!(!format!("{bridge:?}").contains(bridge.token()));

        let denied = raw_http(
            bridge.addr(),
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        );
        assert_eq!(denied.status, 401);

        let health = raw_http(
            bridge.addr(),
            &format!(
                "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n\r\n",
                bridge.token()
            ),
        );
        assert_eq!(health.status, 200);
        let health_json: Value = serde_json::from_slice(&health.body).unwrap();
        assert_eq!(health_json["status"], "ok");
        assert_eq!(health_json["session_id"], bridge.session_id());

        let payload = r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#;
        let rpc = raw_http(
            bridge.addr(),
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                bridge.token(),
                payload.len()
            ),
        );
        assert_eq!(rpc.status, 200);
        let rpc_json: Value = serde_json::from_slice(&rpc.body).unwrap();
        assert_eq!(rpc_json["id"], 7);
        assert_eq!(rpc_json["result"]["ok"], true);

        let notification = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let accepted = raw_http(
            bridge.addr(),
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{notification}",
                bridge.token(),
                notification.len()
            ),
        );
        assert_eq!(accepted.status, 202);

        let claude = bridge.injection_for(BridgeProvider::Claude).unwrap();
        let claude_path = claude.config_file.clone().unwrap();
        assert!(claude_path.is_file());
        assert_eq!(claude.args[0], "--mcp-config");
        assert!(!claude.args.iter().any(|arg| arg.contains(bridge.token())));
        let claude_json: Value =
            serde_json::from_str(&fs::read_to_string(&claude_path).unwrap()).unwrap();
        assert_eq!(
            claude_json["mcpServers"][BRIDGE_SERVER_NAME]["url"],
            bridge.url()
        );

        let codex = bridge.injection_for(BridgeProvider::Codex).unwrap();
        assert!(!codex.args.iter().any(|arg| arg.contains(bridge.token())));
        assert!(codex.args.iter().any(|arg| arg.contains(&bridge.url())));
        assert!(codex.args.iter().any(|arg| arg.contains(BRIDGE_TOKEN_ENV)));
        assert_eq!(codex.env[0].0, BRIDGE_TOKEN_ENV);

        let gemini = bridge.injection_for(BridgeProvider::Gemini).unwrap();
        let gemini_path = gemini.config_file.clone().unwrap();
        assert!(gemini_path.is_file());
        assert_eq!(
            gemini.args,
            vec!["--allowed-mcp-server-names", BRIDGE_SERVER_NAME]
        );

        drop(bridge);
        assert!(!claude_path.exists());
        assert!(!gemini_path.exists());
    }

    #[test]
    fn worker_mcp_diet_args_strip_external_mcps_for_each_provider() {
        assert_eq!(
            worker_mcp_diet_args(BridgeProvider::Claude),
            vec![
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#
            ]
        );
        assert_eq!(
            worker_mcp_diet_args(BridgeProvider::Codex),
            vec!["-c", "mcp_servers={}"]
        );
        assert_eq!(
            worker_mcp_diet_args(BridgeProvider::Gemini),
            vec!["--allowed-mcp-server-names", "none"]
        );
    }

    struct EchoExecutor;

    impl ExternalToolExecutor for EchoExecutor {
        fn execute(&self, server: &str, tool: &str, arguments: Value) -> ExternalCallResult {
            ExternalCallResult::Ok(json!({
                "server": server,
                "tool": tool,
                "arguments": arguments,
            }))
        }
    }

    #[test]
    fn external_tool_catalog_supports_lazy_search_describe_call_and_needs_auth() {
        let catalog = ExternalToolCatalog::with_executor(Arc::new(EchoExecutor));
        catalog.register_server(ExternalServerSpec {
            name: "porkbun".into(),
            transport: "http".into(),
            connected: false,
            auth_hint: Some("Sign in to Porkbun MCP to manage DNS records.".into()),
        });
        catalog.register_server(ExternalServerSpec {
            name: "linear".into(),
            transport: "http".into(),
            connected: true,
            auth_hint: None,
        });
        catalog.register_tools(vec![
            ExternalToolSpec {
                server: "porkbun".into(),
                name: "dns_create".into(),
                description: "Create a DNS record for a domain.".into(),
                input_schema: json!({"type": "object", "required": ["domain"]}),
                tags: vec!["dns".into(), "domains".into()],
            },
            ExternalToolSpec {
                server: "linear".into(),
                name: "issue_create".into(),
                description: "Create a Linear issue.".into(),
                input_schema: json!({"type": "object", "required": ["title"]}),
                tags: vec!["issues".into()],
            },
        ]);

        let dns_hits = catalog.search("dns record", None, 10);
        assert_eq!(dns_hits.len(), 1);
        assert_eq!(dns_hits[0].server, "porkbun");
        assert!(dns_hits[0].requires_auth);

        let desc = catalog.describe("linear", "issue_create").unwrap();
        assert_eq!(desc.input_schema["required"][0], "title");
        assert!(desc.connected);

        let auth_needed = catalog.call("porkbun", "dns_create", json!({"domain": "x.dev"}));
        assert!(matches!(
            auth_needed,
            ExternalCallResult::NeedsAuth { server, .. } if server == "porkbun"
        ));

        catalog.set_server_connected("porkbun", true);
        let ok = catalog.call("porkbun", "dns_create", json!({"domain": "x.dev"}));
        assert!(matches!(
            ok,
            ExternalCallResult::Ok(value) if value["arguments"]["domain"] == "x.dev"
        ));
    }

    #[test]
    fn wake_queue_delivers_background_results_reactively() {
        let (tx, rx) = mpsc::channel();
        let queue = WakeQueue::with_notifier(tx);
        let worker_queue = queue.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            worker_queue.push(BackgroundResult {
                worker_id: "nova".into(),
                role: "builder".into(),
                status: "DONE".into(),
                branch: Some("ac/nova-fix".into()),
                workspace: "/tmp/wt".into(),
                summary: "All 42 tests pass.".into(),
                is_error: false,
                finished_at: "12:00:00".into(),
            });
        });
        let item = queue.wait_next(Duration::from_secs(2)).unwrap();
        handle.join().unwrap();
        assert_eq!(item.worker_id, "nova");
        assert_eq!(rx.try_recv().unwrap().worker_id, "nova");
        let tag = item.format_tag();
        assert!(tag.starts_with("<background-result kind=\"worker\" worker_id=\"nova\""));
        assert!(tag.contains("branch=\"ac/nova-fix\""));
        assert!(tag.contains("All 42 tests pass."));
        assert!(tag.ends_with("</background-result>"));
    }
}
