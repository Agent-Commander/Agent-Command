use crate::{
    auth::{self, Profile, ProfileSource},
    control::{self, Control, Scope},
    daemon::{constant_time_equal, Token},
    evolution::EvolutionStore,
    http::{self, HttpError, Request},
    mcp,
    memory::MemoryStore,
    soul::SoulStore,
    specialists,
    vault::{self, VaultStore},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
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
const BRIDGE_CONFIG_DIRECTORY: &str = "agent-command-bridge-v1";
const LEDGER_DIRECTORY: &str = "agent-command-ledger-v1";
const LEDGER_FILE: &str = "ledger.json";
const LEDGER_DOCS_DIRECTORY: &str = "ledger-docs";
const MAX_UI_EVENTS: usize = 256;
const MAX_INBOX_ITEMS: usize = 200;
const AGY_EAGER_TOOLS: [&str; 8] = [
    "env_status",
    "spawn_worker",
    "inbox_list",
    "ledger_list",
    "ui_post_table",
    "ui_post_card",
    "vault_list",
    "vault_check",
];

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
        BridgeProvider::Gemini => vec![
            "--settings".into(),
            r#"{"permissions":{"deny":["mcp(*)"]}}"#.into(),
        ],
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileHeadroom {
    pub label: String,
    pub provider: String,
    pub logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_remaining: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub week_remaining: Option<u8>,
    pub latched: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnvSnapshot {
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub git_dirty_files: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub profiles: Vec<ProfileHeadroom>,
    #[serde(default)]
    pub external_servers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiInboxItem {
    pub key: String,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub thread: String,
    pub level: String,
    pub needs_action: bool,
    #[serde(default)]
    pub dismissed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDecisionOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub recommended: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDecisionRecord {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub question: String,
    pub recommendation: String,
    pub options: Vec<UiDecisionOption>,
    #[serde(default)]
    pub directions: String,
    pub status: String,
    pub record_version: u64,
    pub meaning_fingerprint: String,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDecisionPatch {
    pub id: String,
    pub expected_version: Option<u64>,
    pub status: Option<String>,
    pub title: Option<String>,
    pub question: Option<String>,
    pub recommendation: Option<String>,
    pub options: Option<Vec<UiDecisionOption>>,
    pub directions: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiTableCard {
    pub title: String,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    #[serde(default)]
    pub alignments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiInfoCard {
    pub title: String,
    pub level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    #[serde(default)]
    pub fields: Vec<(String, String)>,
    #[serde(default)]
    pub body_markdown: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VaultSecretRequest {
    pub request_id: String,
    pub name: String,
    pub namespace: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultRequestStatus {
    Saved,
    Cancelled,
    Pending,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VaultRequestOutcome {
    pub status: VaultRequestStatus,
    pub name: String,
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_8: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UiEvent {
    InboxPosted(UiInboxItem),
    DecisionSubmitted(UiDecisionRecord),
    DecisionUpdated(UiDecisionRecord),
    InboxDismissed { key: String },
    LedgerChanged,
    PostTable(UiTableCard),
    PostCard(UiInfoCard),
    VaultRequested(VaultSecretRequest),
}

pub fn decision_meaning_fingerprint(
    question: &str,
    recommendation: &str,
    options: &[UiDecisionOption],
    directions: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(question.trim().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(recommendation.trim().as_bytes());
    hasher.update(b"\x1f");
    for option in options {
        hasher.update(option.id.trim().as_bytes());
        hasher.update(b"=");
        hasher.update(option.label.trim().as_bytes());
        hasher.update(b"|");
        hasher.update(option.summary.trim().as_bytes());
        hasher.update(b"\x1e");
    }
    hasher.update(b"\x1f");
    hasher.update(directions.trim().as_bytes());
    vault::encode_hex(&hasher.finalize())
}

struct UiBridgeState {
    env_snapshot: Option<EnvSnapshot>,
    inbox_items: Vec<UiInboxItem>,
    dismissed_keys: HashSet<String>,
    decisions: Vec<UiDecisionRecord>,
    events: VecDeque<UiEvent>,
    notifier: Option<mpsc::Sender<UiEvent>>,
    ledger_path: Option<PathBuf>,
    ledger_docs_dir: Option<PathBuf>,
    interactive: bool,
    pending_vault_requests: Vec<VaultSecretRequest>,
    vault_outcomes: BTreeMap<String, VaultRequestOutcome>,
}

#[derive(Clone)]
pub struct UiBridge {
    inner: Arc<(Mutex<UiBridgeState>, Condvar)>,
}

impl Default for UiBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl UiBridge {
    pub fn new() -> Self {
        Self::create(false, None)
    }

    pub fn interactive(notifier: Option<mpsc::Sender<UiEvent>>) -> Self {
        Self::create(true, notifier)
    }

    fn create(interactive: bool, notifier: Option<mpsc::Sender<UiEvent>>) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(UiBridgeState {
                    env_snapshot: None,
                    inbox_items: Vec::new(),
                    dismissed_keys: HashSet::new(),
                    decisions: Vec::new(),
                    events: VecDeque::new(),
                    notifier,
                    ledger_path: None,
                    ledger_docs_dir: None,
                    interactive,
                    pending_vault_requests: Vec::new(),
                    vault_outcomes: BTreeMap::new(),
                }),
                Condvar::new(),
            )),
        }
    }

    pub fn with_ledger_paths(self, ledger_path: PathBuf, docs_dir: PathBuf) -> Self {
        {
            let (lock, _) = &*self.inner;
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            state.ledger_path = Some(ledger_path);
            state.ledger_docs_dir = Some(docs_dir);
        }
        self
    }

    fn emit(state: &mut UiBridgeState, event: UiEvent) {
        if let Some(notifier) = &state.notifier {
            let _ = notifier.send(event.clone());
        }
        state.events.push_back(event);
        while state.events.len() > MAX_UI_EVENTS {
            state.events.pop_front();
        }
    }

    pub fn set_env_snapshot(&self, snapshot: EnvSnapshot) {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        state.env_snapshot = Some(snapshot);
    }

    pub fn env_snapshot(&self) -> Option<EnvSnapshot> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .env_snapshot
            .clone()
    }

    pub fn sync_inbox_items(&self, items: Vec<UiInboxItem>) {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let agent_items: Vec<UiInboxItem> = state
            .inbox_items
            .iter()
            .filter(|item| item.kind == "agent_notice")
            .cloned()
            .collect();
        let mut merged = items;
        for agent_item in agent_items {
            if !merged.iter().any(|existing| existing.key == agent_item.key) {
                merged.push(agent_item);
            }
        }
        for item in &mut merged {
            if state.dismissed_keys.contains(&item.key) {
                item.dismissed = true;
            }
        }
        state.inbox_items = merged;
    }

    pub fn list_inbox(
        &self,
        include_dismissed: bool,
        limit: usize,
    ) -> (Vec<UiInboxItem>, Vec<UiDecisionRecord>) {
        let (lock, _) = &*self.inner;
        let state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let items: Vec<UiInboxItem> = state
            .inbox_items
            .iter()
            .filter(|item| {
                include_dismissed || (!item.dismissed && !state.dismissed_keys.contains(&item.key))
            })
            .take(limit)
            .cloned()
            .collect();
        let decisions: Vec<UiDecisionRecord> = state
            .decisions
            .iter()
            .filter(|decision| {
                include_dismissed || !matches!(decision.status.as_str(), "retired" | "resolved")
            })
            .take(limit)
            .cloned()
            .collect();
        (items, decisions)
    }

    pub fn inbox_counts(&self) -> (usize, usize) {
        let (lock, _) = &*self.inner;
        let state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let items = state
            .inbox_items
            .iter()
            .filter(|item| !item.dismissed && !state.dismissed_keys.contains(&item.key))
            .count();
        let open_decisions = state
            .decisions
            .iter()
            .filter(|decision| !matches!(decision.status.as_str(), "retired" | "resolved"))
            .count();
        (items, open_decisions)
    }

    pub fn post_inbox_item(&self, item: UiInboxItem) {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        state.dismissed_keys.remove(&item.key);
        state
            .inbox_items
            .retain(|existing| existing.key != item.key);
        state.inbox_items.push(item.clone());
        let excess = state.inbox_items.len().saturating_sub(MAX_INBOX_ITEMS);
        if excess > 0 {
            state.inbox_items.drain(..excess);
        }
        Self::emit(&mut state, UiEvent::InboxPosted(item));
    }

    pub fn submit_decision(&self, record: UiDecisionRecord) -> Result<UiDecisionRecord, String> {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .decisions
            .iter()
            .any(|existing| existing.id.eq_ignore_ascii_case(&record.id))
        {
            return Err(format!(
                "Decision {} already exists; use inbox_update_decision to update it.",
                record.id
            ));
        }
        state.decisions.push(record.clone());
        Self::emit(&mut state, UiEvent::DecisionSubmitted(record.clone()));
        Ok(record)
    }

    pub fn update_decision(&self, patch: UiDecisionPatch) -> Result<UiDecisionRecord, String> {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(existing) = state
            .decisions
            .iter_mut()
            .find(|item| item.id.eq_ignore_ascii_case(&patch.id))
        else {
            return Err(format!("No decision found with id {}.", patch.id));
        };
        if let Some(expected) = patch.expected_version {
            if existing.record_version != expected {
                return Err(format!(
                    "Decision {} version mismatch: expected {expected}, current {}.",
                    existing.id, existing.record_version
                ));
            }
        }
        if let Some(title) = patch.title {
            existing.title = title;
        }
        if let Some(question) = patch.question {
            existing.question = question;
        }
        if let Some(recommendation) = patch.recommendation {
            existing.recommendation = recommendation;
        }
        if let Some(options) = patch.options {
            existing.options = options;
        }
        if let Some(directions) = patch.directions {
            existing.directions = directions;
        }
        if let Some(status) = patch.status {
            existing.status = status;
        }
        existing.record_version += 1;
        existing.updated_at = chrono::Utc::now().timestamp();
        existing.meaning_fingerprint = decision_meaning_fingerprint(
            &existing.question,
            &existing.recommendation,
            &existing.options,
            &existing.directions,
        );
        let updated = existing.clone();
        Self::emit(&mut state, UiEvent::DecisionUpdated(updated.clone()));
        Ok(updated)
    }

    pub fn dismiss_inbox(&self, key: &str) -> Result<bool, String> {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err("Inbox key must not be empty.".into());
        }
        if trimmed.starts_with("gate:") {
            return Err(
                "Refused to dismiss an unresolved ProdGate approval; only the operator can approve or deny it."
                    .into(),
            );
        }
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changed = state.dismissed_keys.insert(trimmed.to_owned());
        for item in &mut state.inbox_items {
            if item.key == trimmed {
                item.dismissed = true;
                changed = true;
            }
        }
        if let Some(decision) = state.decisions.iter_mut().find(|decision| {
            decision.id == trimmed || format!("decision:{}", decision.id) == trimmed
        }) {
            decision.status = "retired".into();
            decision.record_version += 1;
            decision.updated_at = chrono::Utc::now().timestamp();
            changed = true;
        }
        Self::emit(
            &mut state,
            UiEvent::InboxDismissed {
                key: trimmed.to_owned(),
            },
        );
        Ok(changed)
    }

    pub fn post_table(&self, card: UiTableCard) {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        Self::emit(&mut state, UiEvent::PostTable(card));
    }

    pub fn post_card(&self, card: UiInfoCard) {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        Self::emit(&mut state, UiEvent::PostCard(card));
    }

    pub fn notify_ledger_changed(&self) {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        Self::emit(&mut state, UiEvent::LedgerChanged);
    }

    pub fn ledger_path(&self) -> Option<PathBuf> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ledger_path
            .clone()
    }

    pub fn ledger_docs_dir(&self) -> Option<PathBuf> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ledger_docs_dir
            .clone()
    }

    pub fn drain_events(&self) -> Vec<UiEvent> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .drain(..)
            .collect()
    }

    pub fn pending_vault_requests(&self) -> Vec<VaultSecretRequest> {
        let (lock, _) = &*self.inner;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_vault_requests
            .clone()
    }

    pub fn resolve_vault_request(&self, request_id: &str, outcome: VaultRequestOutcome) -> bool {
        let (lock, condvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let existed = state
            .pending_vault_requests
            .iter()
            .any(|req| req.request_id == request_id);
        state
            .pending_vault_requests
            .retain(|req| req.request_id != request_id);
        state.vault_outcomes.insert(request_id.to_owned(), outcome);
        condvar.notify_all();
        existed
    }

    pub(crate) fn request_vault_secret(
        &self,
        request: VaultSecretRequest,
        timeout: Duration,
    ) -> Option<VaultRequestOutcome> {
        let (lock, condvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let request_id = request.request_id.clone();
        state.pending_vault_requests.push(request.clone());
        Self::emit(&mut state, UiEvent::VaultRequested(request));
        condvar.notify_all();
        if !state.interactive {
            return None;
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(outcome) = state.vault_outcomes.remove(&request_id) {
                return Some(outcome);
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

struct ManagedAgyConfig {
    path: PathBuf,
    server_name: String,
    created_file: bool,
}

pub struct LoopbackBridge {
    addr: SocketAddr,
    session_id: String,
    token: Token,
    shutdown: Arc<AtomicBool>,
    control: Option<Control>,
    ui_bridge: Option<UiBridge>,
    vault: Option<VaultStore>,
    memory: Option<MemoryStore>,
    evolution: Option<EvolutionStore>,
    profile_root: Option<PathBuf>,
    temp_files: Mutex<Vec<PathBuf>>,
    managed_agy_configs: Mutex<Vec<ManagedAgyConfig>>,
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
        let ledger_root = config.profile_root.join(LEDGER_DIRECTORY);
        let ui_bridge = UiBridge::new().with_ledger_paths(
            ledger_root.join(LEDGER_FILE),
            ledger_root.join(LEDGER_DOCS_DIRECTORY),
        );
        let vault = VaultStore::open(config.profile_root.clone());
        Self::start_session_with_ui(config, ui_bridge, Some(vault))
    }

    pub fn start_session_with_ui(
        config: BridgeSessionConfig,
        ui_bridge: UiBridge,
        vault: Option<VaultStore>,
    ) -> io::Result<Self> {
        let profile_root = config.profile_root.clone();
        let memory = MemoryStore::for_workspace(profile_root.clone(), &config.workspace_root);
        let evolution = EvolutionStore::for_workspace(profile_root.clone(), &config.workspace_root);
        let control = Control::open_with(
            specialists::Store::new(config.profile_root.clone()),
            &config.workspace_root,
            Box::new(control::NativeEngine::new(config.profile_root)),
            Arc::clone(&config.shutdown),
            Some(config.wake_queue),
        )?;
        let mut server = mcp::Server::new(control.clone(), Scope::Commander)
            .with_catalog(config.catalog)
            .with_ask_user(config.ask_user)
            .with_ui(ui_bridge.clone())
            .with_memory(memory.clone())
            .with_evolution(evolution.clone());
        if let Some(soul) = config.soul {
            server = server.with_soul(soul);
        }
        if let Some(vault_store) = vault.clone() {
            server = server.with_vault(vault_store);
        }
        let mut bridge = Self::start(Arc::new(server))?;
        bridge.control = Some(control);
        bridge.ui_bridge = Some(ui_bridge);
        bridge.vault = vault;
        bridge.memory = Some(memory);
        bridge.evolution = Some(evolution);
        bridge.profile_root = Some(profile_root);
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
            ui_bridge: None,
            vault: None,
            memory: None,
            evolution: None,
            profile_root: None,
            temp_files: Mutex::new(Vec::new()),
            managed_agy_configs: Mutex::new(Vec::new()),
            worker: Some(worker),
        })
    }

    pub fn wake_queue(&self) -> Option<&WakeQueue> {
        self.control.as_ref().and_then(Control::wake_queue)
    }

    pub fn ui_bridge(&self) -> Option<&UiBridge> {
        self.ui_bridge.as_ref()
    }

    pub fn vault(&self) -> Option<&VaultStore> {
        self.vault.as_ref()
    }

    pub fn memory(&self) -> Option<&MemoryStore> {
        self.memory.as_ref()
    }

    pub fn evolution(&self) -> Option<&EvolutionStore> {
        self.evolution.as_ref()
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
                    "serverUrl": self.url(),
                    "headers": {
                        "Authorization": format!("Bearer ${{{BRIDGE_TOKEN_ENV}}}")
                    }
                }
            }
        })
    }

    pub fn agy_stdio_server_entry(&self, executable: &Path) -> Value {
        let mut eager_tools = Map::new();
        for tool_name in AGY_EAGER_TOOLS {
            eager_tools.insert(tool_name.to_owned(), json!({"eager": true}));
        }
        json!({
            "command": executable.to_string_lossy().into_owned(),
            "args": [
                "mcp",
                "--connect",
                self.base_url(),
                "--token-env",
                BRIDGE_TOKEN_ENV,
            ],
            "disabled": false,
            "tools": Value::Object(eager_tools),
        })
    }

    pub fn claude_stdio_config(&self, executable: &Path) -> Value {
        json!({
            "mcpServers": {
                BRIDGE_SERVER_NAME: {
                    "command": executable.to_string_lossy().into_owned(),
                    "args": [
                        "mcp",
                        "--connect",
                        self.base_url(),
                        "--token-env",
                        BRIDGE_TOKEN_ENV,
                    ]
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
                let executable = current_bridge_executable();
                let config = self.claude_stdio_config(&executable);
                let path =
                    self.write_temp_config("claude", &config, self.profile_root.as_deref())?;
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
                let path =
                    self.write_temp_config("gemini", &config, self.profile_root.as_deref())?;
                Ok(BridgeInjection {
                    args: vec![
                        "--settings".into(),
                        json!({
                            "permissions": {
                                "allow": [format!("mcp({BRIDGE_SERVER_NAME}/*)")]
                            }
                        })
                        .to_string(),
                    ],
                    env: vec![(BRIDGE_TOKEN_ENV.into(), self.token.secret().to_owned())],
                    config_file: Some(path),
                })
            }
        }
    }

    pub fn injection_for_profile(
        &self,
        provider: BridgeProvider,
        profile: &Profile,
    ) -> io::Result<BridgeInjection> {
        let executable = current_bridge_executable();
        match provider {
            BridgeProvider::Claude => {
                let private_root = profile_private_root(profile).or(self.profile_root.as_deref());
                let config = self.claude_stdio_config(&executable);
                let path = self.write_temp_config("claude", &config, private_root)?;
                Ok(BridgeInjection {
                    args: vec!["--mcp-config".into(), path.to_string_lossy().into_owned()],
                    env: vec![(BRIDGE_TOKEN_ENV.into(), self.token.secret().to_owned())],
                    config_file: Some(path),
                })
            }
            BridgeProvider::Codex => self.injection_for(BridgeProvider::Codex),
            BridgeProvider::Gemini => {
                let home = profile_home_directory(profile)?;
                let mcp_config_path = home.join(".gemini").join("config").join("mcp_config.json");
                self.upsert_agy_mcp_config(&mcp_config_path, &executable)?;
                Ok(BridgeInjection {
                    args: vec![
                        "--settings".into(),
                        json!({
                            "permissions": {
                                "allow": [format!("mcp({BRIDGE_SERVER_NAME}/*)")]
                            }
                        })
                        .to_string(),
                    ],
                    env: vec![(BRIDGE_TOKEN_ENV.into(), self.token.secret().to_owned())],
                    config_file: Some(mcp_config_path),
                })
            }
        }
    }

    pub fn upsert_agy_mcp_config(&self, config_path: &Path, executable: &Path) -> io::Result<()> {
        let existed = config_path.exists();
        let mut root_value = if existed {
            let raw = fs::read_to_string(config_path)?;
            let parsed: Value = serde_json::from_str(&raw).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Refused to overwrite unreadable MCP config at {}: {error}",
                        config_path.display()
                    ),
                )
            })?;
            if !parsed.is_object() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Expected JSON object in MCP config at {}",
                        config_path.display()
                    ),
                ));
            }
            parsed
        } else {
            json!({})
        };

        let root_obj = root_value
            .as_object_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MCP config root invalid"))?;
        let servers_value = root_obj
            .entry("mcpServers".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let servers_obj = servers_value.as_object_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected mcpServers object in {}", config_path.display()),
            )
        })?;
        servers_obj.insert(
            BRIDGE_SERVER_NAME.to_owned(),
            self.agy_stdio_server_entry(executable),
        );

        let bytes = serde_json::to_vec_pretty(&root_value).map_err(io::Error::other)?;
        write_atomic_json_file(config_path, &bytes)?;

        let mut managed = self
            .managed_agy_configs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !managed.iter().any(|entry| entry.path == config_path) {
            managed.push(ManagedAgyConfig {
                path: config_path.to_path_buf(),
                server_name: BRIDGE_SERVER_NAME.to_owned(),
                created_file: !existed,
            });
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(control) = &self.control {
            control.shutdown();
        }
        let _ = TcpStream::connect(self.addr);
    }

    fn write_temp_config(
        &self,
        prefix: &str,
        value: &Value,
        profile_root: Option<&Path>,
    ) -> io::Result<PathBuf> {
        let filename = format!(
            "agent-command-mcp-{prefix}-{}-{}.json",
            self.session_id,
            uuid::Uuid::new_v4().simple()
        );
        let directory = match profile_root {
            Some(root) => auth::private_state_directory(root, BRIDGE_CONFIG_DIRECTORY)
                .unwrap_or_else(|_| std::env::temp_dir()),
            None => std::env::temp_dir(),
        };
        let path = directory.join(filename);
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
        drop(files);
        let managed = self
            .managed_agy_configs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for entry in managed.iter() {
            let _ = remove_agy_mcp_entry(&entry.path, &entry.server_name, entry.created_file);
        }
    }
}

fn current_bridge_executable() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agent-command"))
}

fn profile_private_root(profile: &Profile) -> Option<&Path> {
    match profile.source() {
        ProfileSource::Isolated { root, .. } => Some(root.as_path()),
        ProfileSource::SystemDefault => None,
    }
}

fn profile_home_directory(profile: &Profile) -> io::Result<PathBuf> {
    match profile.source() {
        ProfileSource::Isolated { root, name, .. } => {
            Ok(root.join(profile.provider.binary()).join(name))
        }
        ProfileSource::SystemDefault => std::env::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No home directory available")),
    }
}

fn remove_agy_mcp_entry(
    config_path: &Path,
    server_name: &str,
    created_file: bool,
) -> io::Result<()> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut parsed: Value = serde_json::from_str(&raw).map_err(io::Error::other)?;
    let Some(servers) = parsed
        .as_object_mut()
        .and_then(|obj| obj.get_mut("mcpServers"))
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    servers.remove(server_name);
    let empty_servers = servers.is_empty();
    let only_mcp_servers_key = parsed
        .as_object()
        .is_some_and(|obj| obj.len() == 1 && obj.contains_key("mcpServers"));
    if created_file && empty_servers && only_mcp_servers_key {
        let _ = fs::remove_file(config_path);
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(&parsed).map_err(io::Error::other)?;
    write_atomic_json_file(config_path, &bytes)
}

fn write_atomic_json_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let write_result = write_private_file(&temporary, bytes);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
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
        let claude_raw = fs::read_to_string(&claude_path).unwrap();
        assert!(!claude_raw.contains(bridge.token()));
        let claude_json: Value = serde_json::from_str(&claude_raw).unwrap();
        assert_eq!(
            claude_json["mcpServers"][BRIDGE_SERVER_NAME]["args"][0],
            "mcp"
        );
        assert_eq!(
            claude_json["mcpServers"][BRIDGE_SERVER_NAME]["args"][2],
            bridge.base_url()
        );
        assert_eq!(claude.env[0].0, BRIDGE_TOKEN_ENV);

        let codex = bridge.injection_for(BridgeProvider::Codex).unwrap();
        assert!(!codex.args.iter().any(|arg| arg.contains(bridge.token())));
        assert!(codex.args.iter().any(|arg| arg.contains(&bridge.url())));
        assert!(codex.args.iter().any(|arg| arg.contains(BRIDGE_TOKEN_ENV)));
        assert_eq!(codex.env[0].0, BRIDGE_TOKEN_ENV);

        let gemini = bridge.injection_for(BridgeProvider::Gemini).unwrap();
        let gemini_path = gemini.config_file.clone().unwrap();
        assert!(gemini_path.is_file());
        assert!(!gemini.args.iter().any(|arg| arg.contains(bridge.token())));
        assert!(!gemini
            .args
            .iter()
            .any(|arg| arg == "--allowed-mcp-server-names"));
        assert_eq!(
            gemini.args,
            vec![
                "--settings",
                r#"{"permissions":{"allow":["mcp(agent_command/*)"]}}"#
            ]
        );
        let gemini_json: Value =
            serde_json::from_str(&fs::read_to_string(&gemini_path).unwrap()).unwrap();
        assert_eq!(
            gemini_json["mcpServers"][BRIDGE_SERVER_NAME]["serverUrl"],
            bridge.url()
        );
        assert!(gemini_json["mcpServers"][BRIDGE_SERVER_NAME]
            .get("httpUrl")
            .is_none());

        drop(bridge);
        assert!(!claude_path.exists());
        assert!(!gemini_path.exists());
    }

    #[test]
    fn injection_for_profile_wires_all_three_providers_with_zero_tokens_on_disk() {
        let scratch = std::env::temp_dir().join(format!(
            "ac-bridge-profile-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&scratch).unwrap();
        let root = scratch.join("sub-auth");
        let bridge = LoopbackBridge::start_with_handler(Arc::new(|_| None)).unwrap();
        let token = bridge.token().to_owned();

        let claude_profile = Profile::new(root.clone(), auth::Provider::Claude, "work").unwrap();
        let claude = bridge
            .injection_for_profile(BridgeProvider::Claude, &claude_profile)
            .unwrap();
        let claude_file = claude.config_file.clone().unwrap();
        assert!(claude_file.is_file());
        assert!(claude_file.starts_with(&root));
        let claude_text = fs::read_to_string(&claude_file).unwrap();
        assert!(!claude_text.contains(&token));
        assert_eq!(claude.env, vec![(BRIDGE_TOKEN_ENV.into(), token.clone())]);

        let agy_home = root.join("gemini").join("work");
        let existing_cfg = agy_home
            .join(".gemini")
            .join("config")
            .join("mcp_config.json");
        fs::create_dir_all(existing_cfg.parent().unwrap()).unwrap();
        fs::write(
            &existing_cfg,
            r#"{"mcpServers":{"user_custom":{"serverUrl":"https://example.invalid/mcp"}}}"#,
        )
        .unwrap();

        let codex_profile = Profile::new(root.clone(), auth::Provider::Codex, "work").unwrap();
        let codex = bridge
            .injection_for_profile(BridgeProvider::Codex, &codex_profile)
            .unwrap();
        assert!(codex.config_file.is_none());
        assert_eq!(codex.args[0], "-c");
        assert!(!codex.args.iter().any(|arg| arg.contains(&token)));
        assert_eq!(codex.env, vec![(BRIDGE_TOKEN_ENV.into(), token.clone())]);

        let gemini_profile = Profile::new(root.clone(), auth::Provider::Gemini, "work").unwrap();
        let gemini = bridge
            .injection_for_profile(BridgeProvider::Gemini, &gemini_profile)
            .unwrap();
        assert_eq!(gemini.config_file.as_deref(), Some(existing_cfg.as_path()));
        assert_eq!(
            gemini.args,
            vec![
                "--settings",
                r#"{"permissions":{"allow":["mcp(agent_command/*)"]}}"#
            ]
        );
        let merged_raw = fs::read_to_string(&existing_cfg).unwrap();
        assert!(!merged_raw.contains(&token));
        let merged: Value = serde_json::from_str(&merged_raw).unwrap();
        assert_eq!(
            merged["mcpServers"]["user_custom"]["serverUrl"],
            "https://example.invalid/mcp"
        );
        assert_eq!(
            merged["mcpServers"][BRIDGE_SERVER_NAME]["args"][2],
            bridge.base_url()
        );
        assert_eq!(
            merged["mcpServers"][BRIDGE_SERVER_NAME]["tools"]["env_status"]["eager"],
            true
        );

        drop(bridge);
        assert!(!claude_file.exists());
        let restored: Value =
            serde_json::from_str(&fs::read_to_string(&existing_cfg).unwrap()).unwrap();
        assert!(restored["mcpServers"].get(BRIDGE_SERVER_NAME).is_none());
        assert_eq!(
            restored["mcpServers"]["user_custom"]["serverUrl"],
            "https://example.invalid/mcp"
        );
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn ui_bridge_handles_decisions_cas_prod_gate_dismiss_guard_and_vault_requests() {
        let (tx, rx) = mpsc::channel();
        let ui = UiBridge::interactive(Some(tx));
        let options = vec![
            UiDecisionOption {
                id: "a".into(),
                label: "Ship A".into(),
                summary: "Fast path".into(),
                recommended: true,
            },
            UiDecisionOption {
                id: "b".into(),
                label: "Ship B".into(),
                summary: "Safe path".into(),
                recommended: false,
            },
        ];
        let fp = decision_meaning_fingerprint("Which path?", "Ship A", &options, "");
        let submitted = ui
            .submit_decision(UiDecisionRecord {
                id: "dec-1".into(),
                kind: "architecture".into(),
                title: "Choose path".into(),
                question: "Which path?".into(),
                recommendation: "Ship A".into(),
                options,
                directions: String::new(),
                status: "open".into(),
                record_version: 1,
                meaning_fingerprint: fp.clone(),
                updated_at: 10,
            })
            .unwrap();
        assert_eq!(submitted.record_version, 1);

        let stale = ui.update_decision(UiDecisionPatch {
            id: "dec-1".into(),
            expected_version: Some(99),
            status: Some("deferred".into()),
            title: None,
            question: None,
            recommendation: None,
            options: None,
            directions: None,
        });
        assert!(stale.unwrap_err().contains("version mismatch"));

        let updated = ui
            .update_decision(UiDecisionPatch {
                id: "dec-1".into(),
                expected_version: Some(1),
                status: Some("explain_more".into()),
                title: None,
                question: None,
                recommendation: Some("Ship B instead".into()),
                options: None,
                directions: None,
            })
            .unwrap();
        assert_eq!(updated.record_version, 2);
        assert_ne!(updated.meaning_fingerprint, fp);

        assert!(ui.dismiss_inbox("gate:req-123").is_err());
        assert!(ui.dismiss_inbox("dec-1").unwrap());

        let waiter = ui.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let pending = waiter.pending_vault_requests();
            assert_eq!(pending.len(), 1);
            waiter.resolve_vault_request(
                &pending[0].request_id,
                VaultRequestOutcome {
                    status: VaultRequestStatus::Saved,
                    name: pending[0].name.clone(),
                    namespace: pending[0].namespace.clone(),
                    sha256_8: Some("deadbeef".into()),
                },
            );
        });
        let outcome = ui
            .request_vault_secret(
                VaultSecretRequest {
                    request_id: "vr-1".into(),
                    name: "STRIPE_KEY".into(),
                    namespace: "global".into(),
                    title: "Stripe API key".into(),
                    description: String::new(),
                },
                Duration::from_secs(2),
            )
            .unwrap();
        handle.join().unwrap();
        assert_eq!(outcome.status, VaultRequestStatus::Saved);
        assert_eq!(outcome.sha256_8.as_deref(), Some("deadbeef"));
        assert!(rx.try_recv().is_ok());
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
            vec!["--settings", r#"{"permissions":{"deny":["mcp(*)"]}}"#]
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
