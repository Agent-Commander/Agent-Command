use crate::{
    bridge::{
        decision_meaning_fingerprint, AskUserBroker, AskUserRequest, ExternalCallResult,
        ExternalToolCatalog, UiBridge, UiDecisionOption, UiDecisionPatch, UiDecisionRecord,
        UiInboxItem, UiInfoCard, UiTableCard, VaultRequestStatus, VaultSecretRequest,
    },
    connect,
    control::{self, Brief, Control, Failure, Scope, SpawnWorkerRequest},
    evolution::{
        BotCharter, EvolutionStore, FailureMode, ForgeBotRequest, MutateBotRequest, OutcomeSignal,
        WorkClass, MAX_PROBATION_RUNS, MIN_PROBATION_RUNS,
    },
    ledger::{Ledger, LedgerDocument, LedgerFilter, TaskKind, TaskStatus, TaskUpdate},
    memory::{
        MemoryKind, MemoryOrigin, MemoryScope, MemoryStore, TombstoneReason, WriteMemoryInput,
        DEFAULT_SEARCH_LIMIT, DEFAULT_TOKEN_BUDGET, MAX_ACTIVE_ENTRIES, MAX_BODY_CHARS,
        MAX_BOT_ID_CHARS, MAX_TAGS, MAX_TAG_CHARS, MAX_TITLE_CHARS,
    },
    permissions::PermissionMode,
    soul::{self, SoulStore},
    vault::{self, VaultStore},
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, PoisonError},
    thread,
    time::Duration,
};

pub(crate) const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: [&str; 3] = [LATEST_PROTOCOL, "2025-03-26", "2024-11-05"];
pub(crate) const MAX_MESSAGE_BYTES: usize = 1 << 20;
const MAX_READ_LIMIT: usize = 200;
const MAX_GITHUB_LIMIT: usize = 100;
const DEFAULT_GITHUB_LIMIT: usize = 20;
const MAX_MEMORY_LIMIT: usize = 50;
const DEFAULT_MEMORY_LIMIT: usize = 20;
const MAX_ASK_CHOICES: usize = 3;
const MAX_INBOX_LIMIT: usize = 100;
const DEFAULT_INBOX_LIMIT: usize = 50;
const MAX_LEDGER_DOC_BYTES: usize = 256 * 1024;
const MAX_TABLE_COLUMNS: usize = 12;
const MAX_TABLE_ROWS: usize = 100;
const MAX_CARD_FIELDS: usize = 20;
const MAX_VAULT_CHECK_NAMES: usize = 50;
const ASK_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

const READ_TOOLS: [&str; 22] = [
    "list_threads",
    "search_threads",
    "read_thread",
    "wait_thread",
    "list_workers",
    "read_worker",
    "search_memory",
    "tool_search",
    "tool_describe",
    "github_pr_list",
    "github_pr_view",
    "github_pr_checks",
    "github_issue_list",
    "github_issue_view",
    "env_status",
    "inbox_list",
    "ledger_list",
    "ledger_read_doc",
    "vault_list",
    "vault_check",
    "memory_search",
    "bot_scorecard",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GhOutput {
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) trait GhExecutor: Send + Sync {
    fn run(&self, cwd: &Path, args: &[String]) -> io::Result<GhOutput>;
}

struct CliGhExecutor;

impl GhExecutor for CliGhExecutor {
    fn run(&self, cwd: &Path, args: &[String]) -> io::Result<GhOutput> {
        let output = Command::new("gh")
            .current_dir(cwd)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        Ok(GhOutput {
            exit_code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

enum Reply {
    Result(Value),
    ToolFailure(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    name: String,
    since: Option<u64>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArguments {
    name: String,
    role: String,
    task: Option<String>,
    workspace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskArguments {
    name: String,
    task: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArguments {
    name: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateArguments {
    name: String,
    brief: Brief,
    workspace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArguments {
    name: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArguments {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnWorkerArguments {
    name: String,
    role: String,
    task: String,
    model: Option<String>,
    mode: Option<String>,
    workspace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageWorkerArguments {
    worker_id: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadWorkerArguments {
    worker_id: String,
    since: Option<u64>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerIdArguments {
    worker_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserArguments {
    question: String,
    choices: Vec<String>,
    context: Option<String>,
    allow_freeform: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberArguments {
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchMemoryArguments {
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolSearchArguments {
    query: String,
    server: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolDescribeArguments {
    server: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallArguments {
    server: String,
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubPrListArguments {
    state: Option<String>,
    limit: Option<usize>,
    author: Option<String>,
    head: Option<String>,
    base: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubNumberArguments {
    number: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubPrCreateArguments {
    title: String,
    body: Option<String>,
    base: Option<String>,
    head: Option<String>,
    draft: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubIssueListArguments {
    state: Option<String>,
    limit: Option<usize>,
    label: Option<String>,
    assignee: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubIssueCreateArguments {
    title: String,
    body: Option<String>,
    labels: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubPrMergeArguments {
    number: u64,
    expected_head_sha: String,
    method: Option<String>,
    delete_branch: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxListArguments {
    include_dismissed: Option<bool>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxPostArguments {
    title: String,
    detail: String,
    level: Option<String>,
    thread: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionOptionInput {
    id: String,
    label: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    recommended: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxSubmitDecisionArguments {
    id: Option<String>,
    kind: Option<String>,
    title: String,
    question: String,
    recommendation: String,
    options: Vec<DecisionOptionInput>,
    directions: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxUpdateDecisionArguments {
    id: String,
    expected_version: Option<u64>,
    status: Option<String>,
    title: Option<String>,
    question: Option<String>,
    recommendation: Option<String>,
    options: Option<Vec<DecisionOptionInput>>,
    directions: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxDismissArguments {
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerListArguments {
    filter: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerRecordArguments {
    kind: String,
    owner: String,
    title: String,
    status: Option<String>,
    branch: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerUpdateArguments {
    id_or_owner: String,
    status: String,
    branch: Option<String>,
    changed_files: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerReadDocArguments {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerAppendDocArguments {
    path: String,
    section: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UiPostTableArguments {
    title: String,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    alignments: Option<Vec<String>>,
    caption: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CardFieldInput {
    label: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UiPostCardArguments {
    title: String,
    level: Option<String>,
    badge: Option<String>,
    fields: Option<Vec<CardFieldInput>>,
    body_markdown: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultListArguments {
    namespace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultCheckArguments {
    names: Vec<String>,
    namespace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultRequestArguments {
    name: String,
    title: String,
    namespace: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultDeleteArguments {
    name: String,
    namespace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemorySearchArguments {
    scope: String,
    bot_id: Option<String>,
    query: String,
    limit: Option<usize>,
    token_budget: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryWriteArguments {
    scope: String,
    bot_id: Option<String>,
    kind: String,
    title: String,
    body: String,
    tags: Option<Vec<String>>,
    author: Option<String>,
    salience: Option<f64>,
    confidence: Option<f64>,
    pinned: Option<bool>,
    ttl_secs: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemorySupersedeArguments {
    scope: String,
    bot_id: Option<String>,
    target_id: String,
    kind: String,
    title: String,
    body: String,
    tags: Option<Vec<String>>,
    author: Option<String>,
    salience: Option<f64>,
    confidence: Option<f64>,
    pinned: Option<bool>,
    ttl_secs: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryDeleteArguments {
    scope: String,
    bot_id: Option<String>,
    id: String,
    reason: String,
    actor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BotScorecardArguments {
    bot_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CharterInput {
    job: String,
    anti_jobs: Vec<String>,
    #[serde(default)]
    voice_and_style: String,
    verification_bar: String,
    #[serde(default)]
    allowed_tools: Vec<String>,
    instructions: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BotForgeArguments {
    bot_id: String,
    role: String,
    work_class: String,
    charter: CharterInput,
    permission_mode: Option<String>,
    preferred_model: Option<String>,
    pinned_core: Option<bool>,
    initial_memory_seeds: Option<Vec<String>>,
    probation_runs: Option<u32>,
    rationale: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BotRecordFeedbackArguments {
    bot_id: String,
    signal: String,
    task_id: Option<String>,
    duration_ms: Option<u64>,
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BotEvolveArguments {
    bot_id: String,
    failure_mode: String,
    task_summary: String,
    root_cause: String,
    patched_charter: CharterInput,
    diff_summary: Vec<String>,
    memory_correction: Option<String>,
    probation_runs: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BotDecommissionArguments {
    bot_id: String,
    reason: String,
    confirmed: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

pub(crate) struct Server {
    control: Control,
    scope: Scope,
    soul: Option<SoulStore>,
    catalog: ExternalToolCatalog,
    ask_user: AskUserBroker,
    ui: UiBridge,
    vault: Option<VaultStore>,
    memory: Option<MemoryStore>,
    evolution: Option<EvolutionStore>,
    gh: Arc<dyn GhExecutor>,
}

impl Server {
    pub(crate) fn new(control: Control, scope: Scope) -> Self {
        let profile_root = crate::auth::root().ok();
        let memory = profile_root
            .as_ref()
            .map(|root| MemoryStore::for_workspace(root.clone(), control.root()));
        let evolution = profile_root
            .as_ref()
            .map(|root| EvolutionStore::for_workspace(root.clone(), control.root()));
        Self {
            control,
            scope,
            soul: SoulStore::from_env(),
            catalog: ExternalToolCatalog::new(),
            ask_user: AskUserBroker::default(),
            ui: UiBridge::new(),
            vault: VaultStore::from_env().ok(),
            memory,
            evolution,
            gh: Arc::new(CliGhExecutor),
        }
    }

    pub(crate) fn with_soul(mut self, soul: SoulStore) -> Self {
        self.soul = Some(soul);
        self
    }

    pub(crate) fn with_catalog(mut self, catalog: ExternalToolCatalog) -> Self {
        self.catalog = catalog;
        self
    }

    pub(crate) fn with_ask_user(mut self, broker: AskUserBroker) -> Self {
        self.ask_user = broker;
        self
    }

    pub(crate) fn with_ui(mut self, ui: UiBridge) -> Self {
        self.ui = ui;
        self
    }

    pub(crate) fn with_vault(mut self, vault: VaultStore) -> Self {
        self.vault = Some(vault);
        self
    }

    pub(crate) fn with_memory(mut self, memory: MemoryStore) -> Self {
        self.memory = Some(memory);
        self
    }

    pub(crate) fn with_evolution(mut self, evolution: EvolutionStore) -> Self {
        self.evolution = Some(evolution);
        self
    }

    pub(crate) fn advertised_tool_names(&self) -> Vec<String> {
        self.tools()
            .into_iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn with_gh_executor(mut self, gh: Arc<dyn GhExecutor>) -> Self {
        self.gh = gh;
        self
    }

    pub(crate) fn control(&self) -> &Control {
        &self.control
    }

    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    pub(crate) fn handle_text(&self, text: &str) -> Option<Value> {
        match serde_json::from_str::<Value>(text) {
            Ok(message) => self.handle(message),
            Err(error) => Some(error_response(
                Value::Null,
                RpcError::new(PARSE_ERROR, format!("Parse error: {error}")),
            )),
        }
    }

    pub(crate) fn handle(&self, message: Value) -> Option<Value> {
        let Value::Object(object) = message else {
            return Some(error_response(
                Value::Null,
                RpcError::new(INVALID_REQUEST, "Expected one JSON-RPC 2.0 request object"),
            ));
        };
        let id = object.get("id").cloned();
        if let Some(id) = &id {
            if !matches!(id, Value::String(_) | Value::Number(_)) {
                return Some(error_response(
                    Value::Null,
                    RpcError::new(INVALID_REQUEST, "id must be a string or number"),
                ));
            }
        }
        let respond_id = id.clone().unwrap_or(Value::Null);
        if object.get("jsonrpc") != Some(&json!("2.0")) {
            return Some(error_response(
                respond_id,
                RpcError::new(INVALID_REQUEST, "jsonrpc must be \"2.0\""),
            ));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            if object.contains_key("result") || object.contains_key("error") {
                return None;
            }
            return Some(error_response(
                respond_id,
                RpcError::new(INVALID_REQUEST, "method must be a string"),
            ));
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let outcome = self.dispatch(method, params);
        let id = id?;
        Some(match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => error_response(id, error),
        })
    }

    fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "notifications/initialized" | "notifications/cancelled" => Ok(Value::Null),
            "tools/list" => Ok(json!({"tools": self.tools()})),
            "tools/call" => self.call(params),
            other => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("Method not found: {other}"),
            )),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|version| SUPPORTED_PROTOCOLS.contains(version))
            .unwrap_or(LATEST_PROTOCOL);
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "agent-command", "version": env!("CARGO_PKG_VERSION")},
            "instructions": format!(
                "Agent Command control plane. Workers run in isolated git worktrees (or the project directory outside git). Scope: {}. Full bypass flags are never available.",
                self.scope.label()
            ),
        })
    }

    fn tools(&self) -> Vec<Value> {
        tool_definitions()
            .into_iter()
            .filter(|tool| {
                self.scope.allows_changes()
                    || tool["name"]
                        .as_str()
                        .is_some_and(|name| READ_TOOLS.contains(&name))
            })
            .collect()
    }

    fn call(&self, params: Value) -> Result<Value, RpcError> {
        let call: CallParams = serde_json::from_value(params)
            .map_err(|error| RpcError::params(format!("Invalid tools/call params: {error}")))?;
        let arguments = call.arguments.unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return Err(RpcError::params("arguments must be an object"));
        }
        let known = tool_definitions()
            .iter()
            .any(|tool| tool["name"] == call.name.as_str());
        if !known {
            return Err(RpcError::params(format!("Unknown tool: {}", call.name)));
        }
        if !self.scope.allows_changes() && !READ_TOOLS.contains(&call.name.as_str()) {
            return Ok(tool_result(Reply::ToolFailure(format!(
                "{} needs supervised or commander scope; this server is read-only.",
                call.name
            ))));
        }
        let reply = match self.run_tool(&call.name, arguments) {
            Ok(value) => Reply::Result(value),
            Err(ToolError::Rpc(error)) => return Err(error),
            Err(ToolError::Failed(message)) => Reply::ToolFailure(message),
        };
        Ok(tool_result(reply))
    }

    fn run_tool(&self, name: &str, arguments: Value) -> Result<Value, ToolError> {
        let control = &self.control;
        match name {
            "list_threads" => {
                let NoArguments {} = parse(arguments)?;
                Ok(json!({"threads": control.list()}))
            }
            "search_threads" => {
                let args: SearchArguments = parse(arguments)?;
                Ok(json!({"threads": control.search(&args.query)?}))
            }
            "read_thread" => {
                let args: ReadArguments = parse(arguments)?;
                let limit = bounded_limit(args.limit)?;
                Ok(json!(control.read(
                    &args.name,
                    args.since.unwrap_or(0),
                    limit
                )?))
            }
            "create_thread" => {
                let args: CreateArguments = parse(arguments)?;
                Ok(json!({"thread": control.create(
                    &args.name,
                    &args.role,
                    args.task.as_deref(),
                    args.workspace.as_deref(),
                )?}))
            }
            "launch_mission" => {
                let args: TaskArguments = parse(arguments)?;
                Ok(json!({"thread": control.launch(&args.name, &args.task)?}))
            }
            "message_thread" => {
                let args: MessageArguments = parse(arguments)?;
                Ok(json!({"thread": control.message(&args.name, &args.message)?}))
            }
            "delegate_task" => {
                let args: DelegateArguments = parse(arguments)?;
                Ok(json!({"thread": control.delegate(
                    &args.name,
                    &args.brief,
                    args.workspace.as_deref(),
                )?}))
            }
            "wait_thread" => {
                let args: WaitArguments = parse(arguments)?;
                let timeout = args
                    .timeout_ms
                    .map_or(control::DEFAULT_WAIT, Duration::from_millis);
                Ok(json!(control.wait(&args.name, timeout)?))
            }
            "interrupt_thread" => {
                let args: NameArguments = parse(arguments)?;
                Ok(json!({"thread": control.interrupt(&args.name)?}))
            }
            "spawn_worker" => {
                let args: SpawnWorkerArguments = parse(arguments)?;
                let mode = match args.mode.as_deref() {
                    Some(raw) => Some(PermissionMode::parse(raw).map_err(ToolError::invalid)?),
                    None => None,
                };
                let spawned = control.spawn_worker(&SpawnWorkerRequest {
                    name: args.name,
                    role: args.role,
                    task: args.task,
                    model: args.model,
                    mode,
                    workspace: args.workspace,
                    allow_elevated_mode: self.scope.allows_elevated_worker_modes(),
                })?;
                Ok(json!(spawned))
            }
            "message_worker" => {
                let args: MessageWorkerArguments = parse(arguments)?;
                Ok(json!({"worker": control.message(&args.worker_id, &args.message)?}))
            }
            "read_worker" => {
                let args: ReadWorkerArguments = parse(arguments)?;
                let limit = bounded_limit(args.limit)?;
                let transcript = control.read(&args.worker_id, args.since.unwrap_or(0), limit)?;
                Ok(json!({
                    "worker": transcript.thread,
                    "entries": transcript.entries,
                }))
            }
            "list_workers" => {
                let NoArguments {} = parse(arguments)?;
                Ok(json!({"workers": control.list()}))
            }
            "interrupt_worker" => {
                let args: WorkerIdArguments = parse(arguments)?;
                Ok(json!({"worker": control.interrupt(&args.worker_id)?}))
            }
            "ask_user" => self.run_ask_user(parse(arguments)?),
            "remember" => self.run_remember(parse(arguments)?),
            "search_memory" => self.run_search_memory(parse(arguments)?),
            "tool_search" => self.run_tool_search(parse(arguments)?),
            "tool_describe" => self.run_tool_describe(parse(arguments)?),
            "tool_call" => self.run_tool_call(parse(arguments)?),
            "github_pr_list" => self.run_github_pr_list(parse(arguments)?),
            "github_pr_view" => self.run_github_pr_view(parse(arguments)?),
            "github_pr_create" => self.run_github_pr_create(parse(arguments)?),
            "github_pr_checks" => self.run_github_pr_checks(parse(arguments)?),
            "github_issue_list" => self.run_github_issue_list(parse(arguments)?),
            "github_issue_view" => self.run_github_issue_view(parse(arguments)?),
            "github_issue_create" => self.run_github_issue_create(parse(arguments)?),
            "github_pr_merge" => self.run_github_pr_merge(parse(arguments)?),
            "env_status" => {
                let NoArguments {} = parse(arguments)?;
                self.run_env_status()
            }
            "inbox_list" => self.run_inbox_list(parse(arguments)?),
            "inbox_post" => self.run_inbox_post(parse(arguments)?),
            "inbox_submit_decision" => self.run_inbox_submit_decision(parse(arguments)?),
            "inbox_update_decision" => self.run_inbox_update_decision(parse(arguments)?),
            "inbox_dismiss" => self.run_inbox_dismiss(parse(arguments)?),
            "ledger_list" => self.run_ledger_list(parse(arguments)?),
            "ledger_record" => self.run_ledger_record(parse(arguments)?),
            "ledger_update" => self.run_ledger_update(parse(arguments)?),
            "ledger_read_doc" => self.run_ledger_read_doc(parse(arguments)?),
            "ledger_append_doc" => self.run_ledger_append_doc(parse(arguments)?),
            "ui_post_table" => self.run_ui_post_table(parse(arguments)?),
            "ui_post_card" => self.run_ui_post_card(parse(arguments)?),
            "vault_list" => self.run_vault_list(parse(arguments)?),
            "vault_check" => self.run_vault_check(parse(arguments)?),
            "vault_request" => self.run_vault_request(parse(arguments)?),
            "vault_delete" => self.run_vault_delete(parse(arguments)?),
            "memory_search" => self.run_memory_search(parse(arguments)?),
            "memory_write" => self.run_memory_write(parse(arguments)?),
            "memory_supersede" => self.run_memory_supersede(parse(arguments)?),
            "memory_delete" => self.run_memory_delete(parse(arguments)?),
            "bot_scorecard" => self.run_bot_scorecard(parse(arguments)?),
            "bot_forge" => self.run_bot_forge(parse(arguments)?),
            "bot_record_feedback" => self.run_bot_record_feedback(parse(arguments)?),
            "bot_evolve" => self.run_bot_evolve(parse(arguments)?),
            "bot_decommission" => self.run_bot_decommission(parse(arguments)?),
            other => Err(ToolError::Rpc(RpcError::params(format!(
                "Unknown tool: {other}"
            )))),
        }
    }

    fn run_ask_user(&self, args: AskUserArguments) -> Result<Value, ToolError> {
        control::bounded_text("question", &args.question, control::MAX_TEXT_CHARS)?;
        if args.choices.is_empty() || args.choices.len() > MAX_ASK_CHOICES {
            return Err(ToolError::invalid(format!(
                "choices must contain between 1 and {MAX_ASK_CHOICES} options."
            )));
        }
        let mut cleaned_choices = Vec::with_capacity(args.choices.len());
        for choice in &args.choices {
            control::bounded_text("choice", choice, 240)?;
            cleaned_choices.push(choice.trim().to_owned());
        }
        if let Some(context) = &args.context {
            control::bounded_text("context", context, control::MAX_BRIEF_FIELD_CHARS)?;
        }
        let request = AskUserRequest {
            request_id: uuid::Uuid::new_v4().simple().to_string(),
            question: args.question.trim().to_owned(),
            choices: cleaned_choices,
            context: args.context.map(|text| text.trim().to_owned()),
            allow_freeform: args.allow_freeform.unwrap_or(true),
        };
        let request_id = request.request_id.clone();
        match self.ask_user.ask(request.clone(), ASK_WAIT_TIMEOUT) {
            Some(answer) => Ok(json!({
                "status": "answered",
                "request_id": request_id,
                "question": request.question,
                "choices": request.choices,
                "selected_index": answer.selected_index,
                "answer": answer.answer,
            })),
            None => Ok(json!({
                "status": "pending",
                "request_id": request_id,
                "question": request.question,
                "choices": request.choices,
                "allow_freeform": request.allow_freeform,
            })),
        }
    }

    fn run_remember(&self, args: RememberArguments) -> Result<Value, ToolError> {
        control::bounded_text("note", &args.note, 500)?;
        let Some(store) = &self.soul else {
            return Err(ToolError::Failed(
                "No durable soul store configured for this session.".into(),
            ));
        };
        store
            .append_learning(&args.note)
            .map_err(|error| match error.kind() {
                io::ErrorKind::InvalidInput => ToolError::Failed(error.to_string()),
                _ => ToolError::Failed(format!("Failed to save learning: {error}")),
            })?;
        Ok(json!({
            "saved": true,
            "note": args.note.trim(),
            "path": store.path().display().to_string(),
        }))
    }

    fn run_search_memory(&self, args: SearchMemoryArguments) -> Result<Value, ToolError> {
        control::bounded_text("query", &args.query, control::MAX_QUERY_CHARS)?;
        let limit = match args.limit {
            None => DEFAULT_MEMORY_LIMIT,
            Some(value) if (1..=MAX_MEMORY_LIMIT).contains(&value) => value,
            Some(_) => {
                return Err(ToolError::invalid(format!(
                    "limit must be between 1 and {MAX_MEMORY_LIMIT}"
                )))
            }
        };
        let mut learnings = match &self.soul {
            Some(store) => store
                .search_learnings(&args.query)
                .map_err(|error| ToolError::Failed(format!("Memory search failed: {error}")))?,
            None => Vec::new(),
        };
        learnings.truncate(limit);
        let mut threads = self.control.search(&args.query)?;
        threads.truncate(limit);
        Ok(json!({
            "query": args.query.trim(),
            "learnings": learnings,
            "threads": threads,
        }))
    }

    fn run_tool_search(&self, args: ToolSearchArguments) -> Result<Value, ToolError> {
        if args.query.chars().count() > control::MAX_QUERY_CHARS || args.query.contains('\0') {
            return Err(ToolError::invalid("Invalid search query."));
        }
        let limit = args.limit.unwrap_or(10).clamp(1, 50);
        let hits = self
            .catalog
            .search(&args.query, args.server.as_deref(), limit);
        Ok(json!({
            "tools": hits,
            "total_registered": self.catalog.tool_count(),
        }))
    }

    fn run_tool_describe(&self, args: ToolDescribeArguments) -> Result<Value, ToolError> {
        control::bounded_text("server", &args.server, 80)?;
        control::bounded_text("name", &args.name, 120)?;
        let description = self
            .catalog
            .describe(&args.server, &args.name)
            .map_err(ToolError::Failed)?;
        Ok(json!(description))
    }

    fn run_tool_call(&self, args: ToolCallArguments) -> Result<Value, ToolError> {
        control::bounded_text("server", &args.server, 80)?;
        control::bounded_text("name", &args.name, 120)?;
        let payload = args.arguments.unwrap_or_else(|| json!({}));
        if !payload.is_object() {
            return Err(ToolError::invalid("arguments must be a JSON object."));
        }
        match self.catalog.call(&args.server, &args.name, payload) {
            ExternalCallResult::Ok(value) => Ok(json!({
                "status": "ok",
                "server": args.server,
                "tool": args.name,
                "result": value,
            })),
            ExternalCallResult::NeedsAuth { server, detail } => Ok(json!({
                "status": "needs_auth",
                "server": server,
                "detail": detail,
            })),
            ExternalCallResult::Failed(message) => Err(ToolError::Failed(message)),
        }
    }

    fn invoke_gh(&self, args: Vec<String>) -> Result<GhOutput, ToolError> {
        self.gh.run(self.control.root(), &args).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ToolError::Failed(
                    "GitHub CLI `gh` is not installed or not on PATH. Install `gh` and run `gh auth login`."
                        .into(),
                )
            } else {
                ToolError::Failed(format!("Failed to execute gh: {error}"))
            }
        })
    }

    fn gh_json_or_auth(
        &self,
        args: Vec<String>,
        allow_nonzero_json: bool,
    ) -> Result<Value, ToolError> {
        let output = self.invoke_gh(args)?;
        if is_gh_auth_error(&output.stderr) {
            return Ok(json!({
                "status": "needs_auth",
                "provider": "github",
                "detail": output.stderr.trim(),
            }));
        }
        if output.exit_code != 0 {
            if allow_nonzero_json {
                if let Ok(parsed) = serde_json::from_str::<Value>(&output.stdout) {
                    return Ok(parsed);
                }
            }
            let detail = if output.stderr.trim().is_empty() {
                format!("gh exited with code {}", output.exit_code)
            } else {
                output.stderr.trim().to_owned()
            };
            return Err(ToolError::Failed(detail));
        }
        serde_json::from_str::<Value>(&output.stdout)
            .map_err(|error| ToolError::Failed(format!("Invalid JSON from gh: {error}")))
    }

    fn run_github_pr_list(&self, args: GithubPrListArguments) -> Result<Value, ToolError> {
        let state = validate_enum(
            "state",
            args.state.as_deref().unwrap_or("open"),
            &["open", "closed", "merged", "all"],
        )?;
        let limit = bounded_github_limit(args.limit)?;
        let mut command = vec![
            "pr".into(),
            "list".into(),
            "--state".into(),
            state.into(),
            "--limit".into(),
            limit.to_string(),
            "--json".into(),
            "number,title,state,url,headRefName,baseRefName,headRefOid,author,isDraft,updatedAt"
                .into(),
        ];
        if let Some(author) = args.author.as_deref() {
            control::bounded_text("author", author, 80)?;
            command.extend(["--author".into(), author.trim().into()]);
        }
        if let Some(head) = args.head.as_deref() {
            control::bounded_text("head", head, 120)?;
            command.extend(["--head".into(), head.trim().into()]);
        }
        if let Some(base) = args.base.as_deref() {
            control::bounded_text("base", base, 120)?;
            command.extend(["--base".into(), base.trim().into()]);
        }
        let parsed = self.gh_json_or_auth(command, false)?;
        if parsed.get("status") == Some(&json!("needs_auth")) {
            return Ok(parsed);
        }
        Ok(json!({"pull_requests": parsed}))
    }

    fn run_github_pr_view(&self, args: GithubNumberArguments) -> Result<Value, ToolError> {
        validate_positive_number("number", args.number)?;
        let command = vec![
            "pr".into(),
            "view".into(),
            args.number.to_string(),
            "--json".into(),
            "number,title,body,state,url,headRefName,baseRefName,headRefOid,mergeable,reviewDecision,author,isDraft,commits,files".into(),
        ];
        let parsed = self.gh_json_or_auth(command, false)?;
        if parsed.get("status") == Some(&json!("needs_auth")) {
            return Ok(parsed);
        }
        Ok(json!({"pull_request": parsed}))
    }

    fn run_github_pr_create(&self, args: GithubPrCreateArguments) -> Result<Value, ToolError> {
        control::bounded_text("title", &args.title, 256)?;
        let mut command = vec![
            "pr".into(),
            "create".into(),
            "--title".into(),
            args.title.trim().into(),
        ];
        if let Some(body) = args.body.as_deref() {
            if body.contains('\0') || body.chars().count() > control::MAX_TEXT_CHARS {
                return Err(ToolError::invalid("Invalid PR body."));
            }
            command.extend(["--body".into(), body.into()]);
        } else {
            command.extend(["--body".into(), String::new()]);
        }
        if let Some(base) = args.base.as_deref() {
            control::bounded_text("base", base, 120)?;
            command.extend(["--base".into(), base.trim().into()]);
        }
        if let Some(head) = args.head.as_deref() {
            control::bounded_text("head", head, 120)?;
            command.extend(["--head".into(), head.trim().into()]);
        }
        if args.draft.unwrap_or(false) {
            command.push("--draft".into());
        }
        let output = self.invoke_gh(command)?;
        if is_gh_auth_error(&output.stderr) {
            return Ok(json!({
                "status": "needs_auth",
                "provider": "github",
                "detail": output.stderr.trim(),
            }));
        }
        if output.exit_code != 0 {
            return Err(ToolError::Failed(format!(
                "gh pr create failed: {}",
                output.stderr.trim()
            )));
        }
        let url = output
            .stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned();
        Ok(json!({
            "created": true,
            "url": url,
            "title": args.title.trim(),
            "draft": args.draft.unwrap_or(false),
        }))
    }

    fn run_github_pr_checks(&self, args: GithubNumberArguments) -> Result<Value, ToolError> {
        validate_positive_number("number", args.number)?;
        let command = vec![
            "pr".into(),
            "checks".into(),
            args.number.to_string(),
            "--json".into(),
            "name,state,bucket,workflow,link".into(),
        ];
        let parsed = self.gh_json_or_auth(command, true)?;
        if parsed.get("status") == Some(&json!("needs_auth")) {
            return Ok(parsed);
        }
        Ok(json!({
            "number": args.number,
            "checks": parsed,
        }))
    }

    fn run_github_issue_list(&self, args: GithubIssueListArguments) -> Result<Value, ToolError> {
        let state = validate_enum(
            "state",
            args.state.as_deref().unwrap_or("open"),
            &["open", "closed", "all"],
        )?;
        let limit = bounded_github_limit(args.limit)?;
        let mut command = vec![
            "issue".into(),
            "list".into(),
            "--state".into(),
            state.into(),
            "--limit".into(),
            limit.to_string(),
            "--json".into(),
            "number,title,state,url,author,labels,assignees,updatedAt".into(),
        ];
        if let Some(label) = args.label.as_deref() {
            control::bounded_text("label", label, 80)?;
            command.extend(["--label".into(), label.trim().into()]);
        }
        if let Some(assignee) = args.assignee.as_deref() {
            control::bounded_text("assignee", assignee, 80)?;
            command.extend(["--assignee".into(), assignee.trim().into()]);
        }
        let parsed = self.gh_json_or_auth(command, false)?;
        if parsed.get("status") == Some(&json!("needs_auth")) {
            return Ok(parsed);
        }
        Ok(json!({"issues": parsed}))
    }

    fn run_github_issue_view(&self, args: GithubNumberArguments) -> Result<Value, ToolError> {
        validate_positive_number("number", args.number)?;
        let command = vec![
            "issue".into(),
            "view".into(),
            args.number.to_string(),
            "--json".into(),
            "number,title,body,state,url,author,labels,assignees,comments".into(),
        ];
        let parsed = self.gh_json_or_auth(command, false)?;
        if parsed.get("status") == Some(&json!("needs_auth")) {
            return Ok(parsed);
        }
        Ok(json!({"issue": parsed}))
    }

    fn run_github_issue_create(
        &self,
        args: GithubIssueCreateArguments,
    ) -> Result<Value, ToolError> {
        control::bounded_text("title", &args.title, 256)?;
        let mut command = vec![
            "issue".into(),
            "create".into(),
            "--title".into(),
            args.title.trim().into(),
        ];
        if let Some(body) = args.body.as_deref() {
            if body.contains('\0') || body.chars().count() > control::MAX_TEXT_CHARS {
                return Err(ToolError::invalid("Invalid issue body."));
            }
            command.extend(["--body".into(), body.into()]);
        } else {
            command.extend(["--body".into(), String::new()]);
        }
        if let Some(labels) = args.labels {
            for label in labels {
                control::bounded_text("label", &label, 80)?;
                command.extend(["--label".into(), label.trim().into()]);
            }
        }
        let output = self.invoke_gh(command)?;
        if is_gh_auth_error(&output.stderr) {
            return Ok(json!({
                "status": "needs_auth",
                "provider": "github",
                "detail": output.stderr.trim(),
            }));
        }
        if output.exit_code != 0 {
            return Err(ToolError::Failed(format!(
                "gh issue create failed: {}",
                output.stderr.trim()
            )));
        }
        let url = output
            .stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned();
        Ok(json!({
            "created": true,
            "url": url,
            "title": args.title.trim(),
        }))
    }

    fn run_github_pr_merge(&self, args: GithubPrMergeArguments) -> Result<Value, ToolError> {
        validate_positive_number("number", args.number)?;
        let expected = args.expected_head_sha.trim();
        if !(7..=40).contains(&expected.len()) || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ToolError::invalid(
                "expected_head_sha must be 7-40 hexadecimal characters.",
            ));
        }
        let method = validate_enum(
            "method",
            args.method.as_deref().unwrap_or("squash"),
            &["merge", "squash", "rebase"],
        )?;
        let inspect = self.gh_json_or_auth(
            vec![
                "pr".into(),
                "view".into(),
                args.number.to_string(),
                "--json".into(),
                "headRefOid,state,url".into(),
            ],
            false,
        )?;
        if inspect.get("status") == Some(&json!("needs_auth")) {
            return Ok(inspect);
        }
        let actual_sha = inspect["headRefOid"]
            .as_str()
            .map(str::trim)
            .filter(|sha| !sha.is_empty())
            .ok_or_else(|| ToolError::Failed("Could not read live PR headRefOid.".into()))?;
        let matches = actual_sha
            .to_ascii_lowercase()
            .starts_with(&expected.to_ascii_lowercase());
        if !matches {
            return Err(ToolError::Failed(format!(
                "Refused to merge PR #{}: head SHA moved (expected {expected}, actual {actual_sha}). Inspect new commits before retrying.",
                args.number
            )));
        }
        let method_flag = match method {
            "merge" => "--merge",
            "rebase" => "--rebase",
            _ => "--squash",
        };
        let mut command = vec![
            "pr".into(),
            "merge".into(),
            args.number.to_string(),
            method_flag.into(),
            "--match-head-commit".into(),
            actual_sha.into(),
        ];
        if args.delete_branch.unwrap_or(false) {
            command.push("--delete-branch".into());
        }
        let output = self.invoke_gh(command)?;
        if is_gh_auth_error(&output.stderr) {
            return Ok(json!({
                "status": "needs_auth",
                "provider": "github",
                "detail": output.stderr.trim(),
            }));
        }
        if output.exit_code != 0 {
            return Err(ToolError::Failed(format!(
                "gh pr merge failed: {}",
                output.stderr.trim()
            )));
        }
        Ok(json!({
            "merged": true,
            "number": args.number,
            "head_sha": actual_sha,
            "method": method,
            "url": inspect["url"].as_str().unwrap_or_default(),
        }))
    }

    fn ledger_path(&self) -> Result<PathBuf, ToolError> {
        if let Some(path) = self.ui.ledger_path() {
            return Ok(path);
        }
        let root = crate::auth::root().map_err(|error| {
            ToolError::Failed(format!("Failed to locate profile root: {error}"))
        })?;
        let directory = crate::auth::private_state_directory(&root, "agent-command-ledger-v1")
            .map_err(|error| {
                ToolError::Failed(format!("Failed to prepare ledger directory: {error}"))
            })?;
        Ok(directory.join("ledger.json"))
    }

    fn ledger_docs_dir(&self) -> Result<PathBuf, ToolError> {
        if let Some(dir) = self.ui.ledger_docs_dir() {
            return Ok(dir);
        }
        let root = crate::auth::root().map_err(|error| {
            ToolError::Failed(format!("Failed to locate profile root: {error}"))
        })?;
        let directory = crate::auth::private_state_directory(&root, "agent-command-ledger-v1")
            .map_err(|error| {
                ToolError::Failed(format!("Failed to prepare ledger directory: {error}"))
            })?;
        Ok(directory.join("ledger-docs"))
    }

    fn run_env_status(&self) -> Result<Value, ToolError> {
        let snapshot = self.ui.env_snapshot().unwrap_or_default();
        let workspace = if snapshot.workspace.trim().is_empty() {
            self.control.root().display().to_string()
        } else {
            snapshot.workspace
        };
        let workers = self.control.list();
        let workers_running = workers.iter().filter(|worker| worker.running).count();
        let (inbox_items, open_decisions) = self.ui.inbox_counts();
        let (ledger_total, ledger_active) = match self.ledger_path() {
            Ok(path) => {
                let ledger = Ledger::load(&path);
                (ledger.total(), ledger.active_count())
            }
            Err(_) => (0, 0),
        };
        let vault_secret_count = self
            .vault
            .as_ref()
            .and_then(|store| store.count().ok())
            .unwrap_or(0);
        let mut payload = json!({
            "scope": self.scope.label(),
            "workspace": workspace,
            "git_branch": snapshot.git_branch,
            "git_dirty_files": snapshot.git_dirty_files,
            "active_provider": snapshot.active_provider,
            "active_profile": snapshot.active_profile,
            "permission_mode": snapshot.permission_mode,
            "profiles": snapshot.profiles,
            "external_servers": snapshot.external_servers,
            "external_tool_count": self.catalog.tool_count(),
            "workers_total": workers.len(),
            "workers_running": workers_running,
            "inbox_items": inbox_items,
            "open_decisions": open_decisions,
            "ledger_total": ledger_total,
            "ledger_active": ledger_active,
            "vault_backend": VaultStore::backend_name(),
            "vault_secret_count": vault_secret_count,
            "mcp_tools": self.advertised_tool_names(),
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_inbox_list(&self, args: InboxListArguments) -> Result<Value, ToolError> {
        let limit = bounded_inbox_limit(args.limit)?;
        let include_dismissed = args.include_dismissed.unwrap_or(false);
        let (items, decisions) = self.ui.list_inbox(include_dismissed, limit);
        let count = items.len() + decisions.len();
        Ok(json!({
            "items": items,
            "decisions": decisions,
            "count": count,
        }))
    }

    fn run_inbox_post(&self, args: InboxPostArguments) -> Result<Value, ToolError> {
        control::bounded_text("title", &args.title, 200)?;
        control::bounded_text("detail", &args.detail, control::MAX_TEXT_CHARS)?;
        let level = validate_enum(
            "level",
            args.level.as_deref().unwrap_or("info"),
            &["info", "attention", "warning", "error"],
        )?;
        let thread = match args.thread.as_deref() {
            Some(value) => {
                control::bounded_text("thread", value, 80)?;
                vault::redact_values(value.trim())
            }
            None => "commander".into(),
        };
        let item = UiInboxItem {
            key: format!("agent:{}", uuid::Uuid::new_v4().simple()),
            kind: "agent_notice".into(),
            title: vault::redact_values(args.title.trim()),
            detail: vault::redact_values(args.detail.trim()),
            thread,
            level: level.into(),
            needs_action: matches!(level, "attention" | "warning" | "error"),
            dismissed: false,
        };
        self.ui.post_inbox_item(item.clone());
        Ok(json!({
            "posted": true,
            "item": item,
        }))
    }

    fn run_inbox_submit_decision(
        &self,
        args: InboxSubmitDecisionArguments,
    ) -> Result<Value, ToolError> {
        let id = match args.id.as_deref() {
            Some(raw) => validate_decision_id(raw)?,
            None => format!("DEC-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        };
        let kind = validate_enum(
            "kind",
            args.kind.as_deref().unwrap_or("decision"),
            &["decision", "design", "risk", "ops", "release"],
        )?;
        control::bounded_text("title", &args.title, 200)?;
        control::bounded_text("question", &args.question, 1000)?;
        control::bounded_text("recommendation", &args.recommendation, 1000)?;
        let directions = validate_optional_text("directions", args.directions.as_deref(), 2000)?;
        reject_placeholder_text("title", &args.title)?;
        reject_placeholder_text("question", &args.question)?;
        reject_placeholder_text("recommendation", &args.recommendation)?;
        reject_placeholder_text("directions", &directions)?;
        let options = validate_decision_options(args.options)?;

        let title = vault::redact_values(args.title.trim());
        let question = vault::redact_values(args.question.trim());
        let recommendation = vault::redact_values(args.recommendation.trim());
        let directions = vault::redact_values(&directions);
        let meaning_fingerprint =
            decision_meaning_fingerprint(&question, &recommendation, &options, &directions);
        let record = UiDecisionRecord {
            id,
            kind: kind.into(),
            title,
            question,
            recommendation,
            options,
            directions,
            status: "open".into(),
            record_version: 1,
            meaning_fingerprint,
            updated_at: chrono::Utc::now().timestamp(),
        };
        let saved = self.ui.submit_decision(record).map_err(ToolError::Failed)?;
        Ok(json!({
            "submitted": true,
            "decision": saved,
        }))
    }

    fn run_inbox_update_decision(
        &self,
        args: InboxUpdateDecisionArguments,
    ) -> Result<Value, ToolError> {
        let id = validate_decision_id(&args.id)?;
        if args.status.is_none()
            && args.title.is_none()
            && args.question.is_none()
            && args.recommendation.is_none()
            && args.options.is_none()
            && args.directions.is_none()
        {
            return Err(ToolError::invalid(
                "Provide at least one decision field to update.",
            ));
        }
        let status = match args.status.as_deref() {
            Some(raw) => Some(
                validate_enum(
                    "status",
                    raw,
                    &["open", "deferred", "explain_more", "retired", "resolved"],
                )?
                .to_owned(),
            ),
            None => None,
        };
        let title = match args.title.as_deref() {
            Some(raw) => {
                control::bounded_text("title", raw, 200)?;
                reject_placeholder_text("title", raw)?;
                Some(vault::redact_values(raw.trim()))
            }
            None => None,
        };
        let question = match args.question.as_deref() {
            Some(raw) => {
                control::bounded_text("question", raw, 1000)?;
                reject_placeholder_text("question", raw)?;
                Some(vault::redact_values(raw.trim()))
            }
            None => None,
        };
        let recommendation = match args.recommendation.as_deref() {
            Some(raw) => {
                control::bounded_text("recommendation", raw, 1000)?;
                reject_placeholder_text("recommendation", raw)?;
                Some(vault::redact_values(raw.trim()))
            }
            None => None,
        };
        let options = match args.options {
            Some(raw) => Some(validate_decision_options(raw)?),
            None => None,
        };
        let directions = match args.directions.as_deref() {
            Some(raw) => {
                let clean = validate_optional_text("directions", Some(raw), 2000)?;
                reject_placeholder_text("directions", &clean)?;
                Some(vault::redact_values(&clean))
            }
            None => None,
        };
        let patch = UiDecisionPatch {
            id,
            expected_version: args.expected_version,
            status,
            title,
            question,
            recommendation,
            options,
            directions,
        };
        let updated = self.ui.update_decision(patch).map_err(ToolError::Failed)?;
        Ok(json!({
            "updated": true,
            "decision": updated,
        }))
    }

    fn run_inbox_dismiss(&self, args: InboxDismissArguments) -> Result<Value, ToolError> {
        control::bounded_text("key", &args.key, 160)?;
        let dismissed = self
            .ui
            .dismiss_inbox(&args.key)
            .map_err(ToolError::Failed)?;
        Ok(json!({
            "dismissed": dismissed,
            "key": args.key.trim(),
        }))
    }

    fn run_ledger_list(&self, args: LedgerListArguments) -> Result<Value, ToolError> {
        let filter_name = validate_enum(
            "filter",
            args.filter.as_deref().unwrap_or("all"),
            &["all", "active", "attention", "done"],
        )?;
        let filter = match filter_name {
            "active" => LedgerFilter::Active,
            "attention" => LedgerFilter::Attention,
            "done" => LedgerFilter::Done,
            _ => LedgerFilter::All,
        };
        let limit = match args.limit {
            None => 50,
            Some(value) if (1..=200).contains(&value) => value,
            Some(_) => {
                return Err(ToolError::invalid("limit must be between 1 and 200."));
            }
        };
        let path = self.ledger_path()?;
        let ledger = Ledger::load(&path);
        let mut entries: Vec<_> = ledger.filtered(filter).into_iter().cloned().collect();
        entries.truncate(limit);
        Ok(json!({
            "filter": filter_name,
            "entries": entries,
            "total": ledger.total(),
            "active": ledger.active_count(),
        }))
    }

    fn ledger_state_dir(&self) -> Result<PathBuf, ToolError> {
        let docs_dir = self.ledger_docs_dir()?;
        if docs_dir
            .file_name()
            .is_some_and(|name| name == "ledger-docs")
        {
            if let Some(parent) = docs_dir.parent() {
                return Ok(parent.to_path_buf());
            }
        }
        if let Some(parent) = self.ledger_path()?.parent() {
            return Ok(parent.to_path_buf());
        }
        Ok(docs_dir)
    }

    fn run_ledger_record(&self, args: LedgerRecordArguments) -> Result<Value, ToolError> {
        let kind = parse_task_kind(&args.kind)?;
        control::bounded_text("owner", &args.owner, 80)?;
        control::bounded_text("title", &args.title, 300)?;
        let status = match args.status.as_deref() {
            Some(raw) => Some(parse_task_status(raw)?),
            None => None,
        };
        let branch = match args.branch.as_deref() {
            Some(raw) => {
                control::bounded_text("branch", raw, 120)?;
                Some(vault::redact_values(raw.trim()))
            }
            None => None,
        };
        let path = self.ledger_path()?;
        let mut ledger = Ledger::load(&path);
        let now = chrono::Utc::now().timestamp();
        let owner = args.owner.trim();
        let clean_title = vault::redact_values(args.title.trim());
        let id = ledger.record(kind, owner, &clean_title, now);
        if status.is_some() || branch.is_some() {
            let mut update = TaskUpdate::status(status.unwrap_or(TaskStatus::Running));
            update.branch = branch;
            ledger.update_latest_of_kind(owner, kind, &update, now);
        }
        ledger
            .save(&path)
            .map_err(|error| ToolError::Failed(format!("Failed to save ledger: {error}")))?;
        let entry = ledger
            .entries()
            .iter()
            .rev()
            .find(|item| item.id == id)
            .cloned();
        if let Some(entry_ref) = entry.as_ref() {
            let state_dir = self.ledger_state_dir()?;
            let doc = LedgerDocument::from_entry(entry_ref, &clean_title);
            ledger
                .write_document(&state_dir, &id, &doc)
                .map_err(|error| {
                    ToolError::Failed(format!("Failed to write ledger document: {error}"))
                })?;
        }
        self.ui.notify_ledger_changed();
        Ok(json!({
            "recorded": true,
            "id": id,
            "entry": entry,
        }))
    }

    fn run_ledger_update(&self, args: LedgerUpdateArguments) -> Result<Value, ToolError> {
        control::bounded_text("id_or_owner", &args.id_or_owner, 80)?;
        let status = parse_task_status(&args.status)?;
        let branch = match args.branch.as_deref() {
            Some(raw) => {
                control::bounded_text("branch", raw, 120)?;
                Some(vault::redact_values(raw.trim()))
            }
            None => None,
        };
        let path = self.ledger_path()?;
        let mut ledger = Ledger::load(&path);
        let now = chrono::Utc::now().timestamp();
        let target = args.id_or_owner.trim();
        let mut update = TaskUpdate::status(status);
        update.branch = branch;
        update.changed_files = args.changed_files;

        let by_id = ledger
            .entries()
            .iter()
            .rev()
            .find(|entry| entry.id.eq_ignore_ascii_case(target))
            .map(|entry| (entry.id.clone(), entry.owner.clone(), entry.kind));

        let (matched_id, changed) = if let Some((id, owner, kind)) = by_id {
            let changed = ledger.update_latest_of_kind(&owner, kind, &update, now);
            (id, changed)
        } else if let Some(entry) = ledger.latest_for(target) {
            let id = entry.id.clone();
            let changed = ledger.update_latest(target, &update, now);
            (id, changed)
        } else {
            return Err(ToolError::Failed(format!(
                "No ledger entry matched {target}."
            )));
        };

        if changed {
            ledger
                .save(&path)
                .map_err(|error| ToolError::Failed(format!("Failed to save ledger: {error}")))?;
            self.ui.notify_ledger_changed();
        }
        Ok(json!({
            "updated": changed,
            "id": matched_id,
            "status": status.label(),
        }))
    }

    fn run_ledger_read_doc(&self, args: LedgerReadDocArguments) -> Result<Value, ToolError> {
        let docs_dir = self.ledger_docs_dir()?;
        let (normalized, full_path) = resolve_safe_ledger_doc_path(&docs_dir, &args.path)?;
        verify_no_symlink_ancestors(&docs_dir, &full_path)?;
        let metadata = fs::symlink_metadata(&full_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ToolError::Failed(format!("Ledger document {normalized} does not exist."))
            } else {
                ToolError::Failed(format!("Cannot read ledger document {normalized}: {error}"))
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ToolError::Failed(format!(
                "Refused to read non-regular ledger document {normalized}."
            )));
        }
        if metadata.len() > MAX_LEDGER_DOC_BYTES as u64 {
            return Err(ToolError::Failed(format!(
                "Ledger document {normalized} exceeds {MAX_LEDGER_DOC_BYTES} bytes."
            )));
        }
        let raw = fs::read_to_string(&full_path)
            .map_err(|error| ToolError::Failed(format!("Failed to read {normalized}: {error}")))?;
        let content = vault::redact_values(&raw);
        Ok(json!({
            "path": normalized,
            "bytes": content.len(),
            "content": content,
        }))
    }

    fn run_ledger_append_doc(&self, args: LedgerAppendDocArguments) -> Result<Value, ToolError> {
        if args.section.contains('\0') {
            return Err(ToolError::invalid("section must not contain NUL bytes."));
        }
        let trimmed_section = args.section.trim();
        if trimmed_section.is_empty() {
            return Err(ToolError::invalid("section must not be empty."));
        }
        if args.section.len() > MAX_LEDGER_DOC_BYTES {
            return Err(ToolError::invalid(format!(
                "section exceeds {MAX_LEDGER_DOC_BYTES} bytes."
            )));
        }
        if soul::contains_secret_like(trimmed_section) {
            return Err(ToolError::Failed(
                "Refused to append secret-like material to a ledger document.".into(),
            ));
        }
        let scrubbed_section = vault::redact_values(trimmed_section);
        let docs_dir = self.ledger_docs_dir()?;
        let (normalized, full_path) = resolve_safe_ledger_doc_path(&docs_dir, &args.path)?;
        verify_no_symlink_ancestors(&docs_dir, &full_path)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                ToolError::Failed(format!("Failed to prepare ledger docs directory: {error}"))
            })?;
        }
        verify_no_symlink_ancestors(&docs_dir, &full_path)?;
        let existing = match fs::symlink_metadata(&full_path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(ToolError::Failed(format!(
                        "Refused to append to non-regular file {normalized}."
                    )));
                }
                if metadata.len() > MAX_LEDGER_DOC_BYTES as u64 {
                    return Err(ToolError::Failed(format!(
                        "Ledger document {normalized} exceeds {MAX_LEDGER_DOC_BYTES} bytes."
                    )));
                }
                fs::read_to_string(&full_path).map_err(|error| {
                    ToolError::Failed(format!("Failed to read existing {normalized}: {error}"))
                })?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(ToolError::Failed(format!(
                    "Failed to inspect {normalized}: {error}"
                )));
            }
        };

        let mut combined = existing;
        if !combined.is_empty() {
            if !combined.ends_with('\n') {
                combined.push('\n');
            }
            if !combined.ends_with("\n\n") {
                combined.push('\n');
            }
        }
        combined.push_str(&scrubbed_section);
        combined.push('\n');
        if combined.len() > MAX_LEDGER_DOC_BYTES {
            return Err(ToolError::Failed(format!(
                "Appending would exceed {MAX_LEDGER_DOC_BYTES} byte limit for {normalized}."
            )));
        }
        write_doc_atomic(&full_path, combined.as_bytes()).map_err(|error| {
            ToolError::Failed(format!(
                "Failed to write ledger document {normalized}: {error}"
            ))
        })?;
        self.ui.notify_ledger_changed();
        Ok(json!({
            "path": normalized,
            "appended": true,
            "bytes": combined.len(),
        }))
    }

    fn run_ui_post_table(&self, args: UiPostTableArguments) -> Result<Value, ToolError> {
        control::bounded_text("title", &args.title, 200)?;
        if args.headers.is_empty() || args.headers.len() > MAX_TABLE_COLUMNS {
            return Err(ToolError::invalid(format!(
                "headers must contain between 1 and {MAX_TABLE_COLUMNS} columns."
            )));
        }
        if args.rows.len() > MAX_TABLE_ROWS {
            return Err(ToolError::invalid(format!(
                "rows must contain at most {MAX_TABLE_ROWS} rows."
            )));
        }
        let mut headers = Vec::with_capacity(args.headers.len());
        for header in &args.headers {
            control::bounded_text("header", header, 80)?;
            headers.push(vault::redact_values(header.trim()));
        }
        let mut rows = Vec::with_capacity(args.rows.len());
        for (index, row) in args.rows.iter().enumerate() {
            if row.len() != headers.len() {
                return Err(ToolError::invalid(format!(
                    "Row {index} has {} cells, expected {}.",
                    row.len(),
                    headers.len()
                )));
            }
            let mut clean_row = Vec::with_capacity(row.len());
            for cell in row {
                if cell.contains('\0') || cell.chars().count() > 500 {
                    return Err(ToolError::invalid(
                        "Table cells must be at most 500 characters without NUL bytes.",
                    ));
                }
                clean_row.push(vault::redact_values(cell.trim()));
            }
            rows.push(clean_row);
        }
        let alignments = match args.alignments {
            Some(list) => {
                if list.len() != headers.len() {
                    return Err(ToolError::invalid(format!(
                        "alignments length ({}) must match headers length ({}).",
                        list.len(),
                        headers.len()
                    )));
                }
                let mut clean = Vec::with_capacity(list.len());
                for item in &list {
                    clean.push(
                        validate_enum("alignment", item, &["left", "center", "right"])?.to_owned(),
                    );
                }
                clean
            }
            None => Vec::new(),
        };
        let caption = match args.caption.as_deref() {
            Some(raw) => {
                let clean = validate_optional_text("caption", Some(raw), 300)?;
                if clean.is_empty() {
                    None
                } else {
                    Some(vault::redact_values(&clean))
                }
            }
            None => None,
        };
        let title = vault::redact_values(args.title.trim());
        let row_count = rows.len();
        let column_count = headers.len();
        self.ui.post_table(UiTableCard {
            title: title.clone(),
            headers,
            rows,
            alignments,
            caption,
        });
        Ok(json!({
            "posted": true,
            "title": title,
            "columns": column_count,
            "rows": row_count,
        }))
    }

    fn run_ui_post_card(&self, args: UiPostCardArguments) -> Result<Value, ToolError> {
        control::bounded_text("title", &args.title, 200)?;
        let level = validate_enum(
            "level",
            args.level.as_deref().unwrap_or("info"),
            &["info", "success", "warning", "error"],
        )?;
        let badge = match args.badge.as_deref() {
            Some(raw) => {
                control::bounded_text("badge", raw, 40)?;
                Some(vault::redact_values(raw.trim()))
            }
            None => None,
        };
        let mut fields = Vec::new();
        if let Some(raw_fields) = args.fields {
            if raw_fields.len() > MAX_CARD_FIELDS {
                return Err(ToolError::invalid(format!(
                    "fields must contain at most {MAX_CARD_FIELDS} entries."
                )));
            }
            for field in raw_fields {
                control::bounded_text("field.label", &field.label, 80)?;
                if field.value.contains('\0') || field.value.chars().count() > 500 {
                    return Err(ToolError::invalid(
                        "field.value must be at most 500 characters without NUL bytes.",
                    ));
                }
                fields.push((
                    vault::redact_values(field.label.trim()),
                    vault::redact_values(field.value.trim()),
                ));
            }
        }
        let body_markdown = match args.body_markdown.as_deref() {
            Some(raw) => {
                let clean =
                    validate_optional_text("body_markdown", Some(raw), control::MAX_TEXT_CHARS)?;
                vault::redact_values(&clean)
            }
            None => String::new(),
        };
        let title = vault::redact_values(args.title.trim());
        self.ui.post_card(UiInfoCard {
            title: title.clone(),
            level: level.into(),
            badge,
            fields,
            body_markdown,
        });
        Ok(json!({
            "posted": true,
            "title": title,
            "level": level,
        }))
    }

    fn run_vault_list(&self, args: VaultListArguments) -> Result<Value, ToolError> {
        let Some(store) = &self.vault else {
            return Err(ToolError::Failed(
                "Vault store is not configured for this session.".into(),
            ));
        };
        let secrets = store
            .list(args.namespace.as_deref())
            .map_err(map_vault_io_error)?;
        Ok(json!({
            "secrets": secrets,
            "count": secrets.len(),
            "backend": VaultStore::backend_name(),
        }))
    }

    fn run_vault_check(&self, args: VaultCheckArguments) -> Result<Value, ToolError> {
        if args.names.is_empty() || args.names.len() > MAX_VAULT_CHECK_NAMES {
            return Err(ToolError::invalid(format!(
                "names must contain between 1 and {MAX_VAULT_CHECK_NAMES} secret names."
            )));
        }
        let Some(store) = &self.vault else {
            return Err(ToolError::Failed(
                "Vault store is not configured for this session.".into(),
            ));
        };
        let namespace = match args.namespace.as_deref() {
            Some(ns) => vault::validate_namespace(ns).map_err(ToolError::invalid)?,
            None => vault::DEFAULT_NAMESPACE.to_owned(),
        };
        let check = store
            .check(&args.names, Some(&namespace))
            .map_err(map_vault_io_error)?;
        Ok(json!({
            "namespace": namespace,
            "present": check.present,
            "missing": check.missing,
        }))
    }

    fn run_vault_request(&self, args: VaultRequestArguments) -> Result<Value, ToolError> {
        let name = vault::validate_secret_name(&args.name).map_err(ToolError::invalid)?;
        let namespace = match args.namespace.as_deref() {
            Some(ns) => vault::validate_namespace(ns).map_err(ToolError::invalid)?,
            None => vault::DEFAULT_NAMESPACE.to_owned(),
        };
        control::bounded_text("title", &args.title, 200)?;
        if soul::contains_secret_like(&args.title) {
            return Err(ToolError::invalid(
                "title must not contain secret-like material.",
            ));
        }
        let description = match args.description.as_deref() {
            Some(raw) => vault::validate_description(raw).map_err(ToolError::invalid)?,
            None => String::new(),
        };
        let request = VaultSecretRequest {
            request_id: uuid::Uuid::new_v4().simple().to_string(),
            name: name.clone(),
            namespace: namespace.clone(),
            title: vault::redact_values(args.title.trim()),
            description,
        };
        let request_id = request.request_id.clone();
        match self.ui.request_vault_secret(request, ASK_WAIT_TIMEOUT) {
            Some(outcome) => Ok(json!({
                "status": outcome.status,
                "request_id": request_id,
                "name": outcome.name,
                "namespace": outcome.namespace,
                "sha256_8": outcome.sha256_8,
            })),
            None => Ok(json!({
                "status": VaultRequestStatus::Pending,
                "request_id": request_id,
                "name": name,
                "namespace": namespace,
            })),
        }
    }

    fn run_vault_delete(&self, args: VaultDeleteArguments) -> Result<Value, ToolError> {
        let name = vault::validate_secret_name(&args.name).map_err(ToolError::invalid)?;
        let namespace = match args.namespace.as_deref() {
            Some(ns) => vault::validate_namespace(ns).map_err(ToolError::invalid)?,
            None => vault::DEFAULT_NAMESPACE.to_owned(),
        };
        let Some(store) = &self.vault else {
            return Err(ToolError::Failed(
                "Vault store is not configured for this session.".into(),
            ));
        };
        let deleted = store
            .delete(&name, Some(&namespace))
            .map_err(map_vault_io_error)?;
        Ok(json!({
            "deleted": deleted,
            "name": name,
            "namespace": namespace,
        }))
    }

    fn require_memory(&self) -> Result<&MemoryStore, ToolError> {
        self.memory.as_ref().ok_or_else(|| {
            ToolError::Failed("Two-tier memory store is not configured for this session.".into())
        })
    }

    fn require_evolution(&self) -> Result<&EvolutionStore, ToolError> {
        self.evolution.as_ref().ok_or_else(|| {
            ToolError::Failed("Bot evolution store is not configured for this session.".into())
        })
    }

    fn run_memory_search(&self, args: MemorySearchArguments) -> Result<Value, ToolError> {
        if args.query.contains('\0') || args.query.chars().count() > control::MAX_QUERY_CHARS {
            return Err(ToolError::invalid("Invalid memory search query."));
        }
        let limit = match args.limit {
            None => DEFAULT_SEARCH_LIMIT,
            Some(value) if (1..=MAX_ACTIVE_ENTRIES).contains(&value) => value,
            Some(_) => {
                return Err(ToolError::invalid(format!(
                    "limit must be between 1 and {MAX_ACTIVE_ENTRIES}."
                )));
            }
        };
        let token_budget = match args.token_budget {
            None => DEFAULT_TOKEN_BUDGET,
            Some(value) if (32..=16_000).contains(&value) => value,
            Some(_) => {
                return Err(ToolError::invalid(
                    "token_budget must be between 32 and 16000.",
                ));
            }
        };
        let scopes = parse_memory_search_scopes(&args.scope, args.bot_id.as_deref())?;
        let store = self.require_memory()?;
        let now = chrono::Utc::now().timestamp();
        let result = store
            .search(&scopes, &args.query, limit, token_budget, now)
            .map_err(map_store_io_error)?;
        let count = result.hits.len();
        let mut payload = json!({
            "query": result.query,
            "hits": result.hits,
            "count": count,
            "truncated_by_budget": result.truncated_by_budget,
            "approx_tokens": result.approx_tokens,
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_memory_write(&self, args: MemoryWriteArguments) -> Result<Value, ToolError> {
        let scope = MemoryScope::parse_wire(&args.scope, args.bot_id.as_deref())
            .map_err(map_store_io_error)?;
        let kind = MemoryKind::parse(&args.kind).map_err(ToolError::invalid)?;
        let tags = args.tags.unwrap_or_default();
        reject_memory_secrets(&args.title, &args.body, &tags)?;
        let origin = match &scope {
            MemoryScope::Shared => MemoryOrigin::Commander,
            MemoryScope::Bot(_) => MemoryOrigin::Bot,
        };
        let author = args.author.unwrap_or_else(|| match &scope {
            MemoryScope::Shared => "commander".to_owned(),
            MemoryScope::Bot(id) => id.clone(),
        });
        let input = WriteMemoryInput {
            scope,
            kind,
            title: args.title,
            body: args.body,
            tags,
            author,
            origin,
            salience: args.salience,
            confidence: args.confidence,
            pinned: args.pinned.unwrap_or(false),
            ttl_secs: args.ttl_secs,
        };
        let store = self.require_memory()?;
        let now = chrono::Utc::now().timestamp();
        let outcome = store.write(input, now).map_err(map_store_io_error)?;
        let mut payload = json!({
            "status": outcome.status,
            "entry": outcome.entry,
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_memory_supersede(&self, args: MemorySupersedeArguments) -> Result<Value, ToolError> {
        let scope = MemoryScope::parse_wire(&args.scope, args.bot_id.as_deref())
            .map_err(map_store_io_error)?;
        let kind = MemoryKind::parse(&args.kind).map_err(ToolError::invalid)?;
        let tags = args.tags.unwrap_or_default();
        reject_memory_secrets(&args.title, &args.body, &tags)?;
        let origin = match &scope {
            MemoryScope::Shared => MemoryOrigin::Commander,
            MemoryScope::Bot(_) => MemoryOrigin::Bot,
        };
        let author = args.author.unwrap_or_else(|| match &scope {
            MemoryScope::Shared => "commander".to_owned(),
            MemoryScope::Bot(id) => id.clone(),
        });
        let replacement = WriteMemoryInput {
            scope: scope.clone(),
            kind,
            title: args.title,
            body: args.body,
            tags,
            author,
            origin,
            salience: args.salience,
            confidence: args.confidence,
            pinned: args.pinned.unwrap_or(false),
            ttl_secs: args.ttl_secs,
        };
        let store = self.require_memory()?;
        let now = chrono::Utc::now().timestamp();
        let created = store
            .supersede(&scope, &args.target_id, replacement, now)
            .map_err(map_store_io_error)?;
        let mut payload = json!({
            "superseded_id": args.target_id.trim(),
            "entry": created,
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_memory_delete(&self, args: MemoryDeleteArguments) -> Result<Value, ToolError> {
        let scope = MemoryScope::parse_wire(&args.scope, args.bot_id.as_deref())
            .map_err(map_store_io_error)?;
        let reason = TombstoneReason::parse(&args.reason).map_err(ToolError::invalid)?;
        if reason == TombstoneReason::Superseded {
            return Err(ToolError::invalid(
                "Use memory_supersede instead of memory_delete for superseded entries.",
            ));
        }
        let actor = args.actor.as_deref().unwrap_or("commander");
        let store = self.require_memory()?;
        let now = chrono::Utc::now().timestamp();
        let deleted = store
            .delete(&scope, &args.id, reason, actor, now)
            .map_err(map_store_io_error)?;
        Ok(json!({
            "deleted": deleted,
            "id": args.id.trim(),
            "reason": reason.label(),
        }))
    }

    fn run_bot_scorecard(&self, args: BotScorecardArguments) -> Result<Value, ToolError> {
        let store = self.require_evolution()?;
        let now = chrono::Utc::now().timestamp();
        let scorecards = store
            .scorecard_report(args.bot_id.as_deref())
            .map_err(map_store_io_error)?;
        let healthcheck = store.healthcheck_audit(now).map_err(map_store_io_error)?;
        let mut payload = json!({
            "scorecards": scorecards,
            "count": scorecards.len(),
            "healthcheck_quiet": healthcheck.is_quiet(),
            "findings": healthcheck.findings,
            "ready_gaps": healthcheck.ready_gaps,
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_bot_forge(&self, args: BotForgeArguments) -> Result<Value, ToolError> {
        let work_class = WorkClass::parse(&args.work_class).map_err(ToolError::invalid)?;
        let permission_mode = match args.permission_mode.as_deref() {
            Some(raw) => {
                let mode = PermissionMode::parse(raw).map_err(ToolError::invalid)?;
                if mode == PermissionMode::WorkspaceAuto
                    && !self.scope.allows_elevated_worker_modes()
                {
                    return Err(ToolError::Failed(
                        "workspace-auto requires commander scope.".into(),
                    ));
                }
                Some(mode)
            }
            None => None,
        };
        reject_charter_secrets(&args.charter)?;
        if soul::contains_secret_like(&args.rationale) {
            return Err(ToolError::Failed(
                "Refused secret-like material in bot_forge rationale.".into(),
            ));
        }
        let initial_memory_seeds = args.initial_memory_seeds.unwrap_or_default();
        for seed in &initial_memory_seeds {
            if soul::contains_secret_like(seed) {
                return Err(ToolError::Failed(
                    "Refused secret-like material in initial_memory_seeds.".into(),
                ));
            }
        }
        let memory = self.require_memory()?;
        let evolution = self.require_evolution()?;
        let now = chrono::Utc::now().timestamp();
        let request = ForgeBotRequest {
            bot_id: args.bot_id,
            role: args.role,
            work_class,
            charter: charter_from_input(args.charter),
            permission_mode,
            preferred_model: args.preferred_model,
            pinned_core: args.pinned_core.unwrap_or(false),
            initial_memory_seeds,
            probation_runs: args.probation_runs,
            rationale: args.rationale,
        };
        let bot = evolution
            .forge_bot(memory, request, now)
            .map_err(map_store_io_error)?;
        let mut payload = json!({
            "forged": true,
            "bot": bot,
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_bot_record_feedback(
        &self,
        args: BotRecordFeedbackArguments,
    ) -> Result<Value, ToolError> {
        let signal = OutcomeSignal::parse(&args.signal).map_err(ToolError::invalid)?;
        if args.note.as_deref().is_some_and(soul::contains_secret_like) {
            return Err(ToolError::Failed(
                "Refused secret-like material in bot feedback note.".into(),
            ));
        }
        let evolution = self.require_evolution()?;
        let now = chrono::Utc::now().timestamp();
        let report = evolution
            .record_feedback(
                &args.bot_id,
                signal,
                args.task_id.as_deref(),
                args.duration_ms,
                args.note.as_deref(),
                now,
            )
            .map_err(map_store_io_error)?;
        let mut payload = json!({
            "recorded": true,
            "report": report,
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_bot_evolve(&self, args: BotEvolveArguments) -> Result<Value, ToolError> {
        let failure_mode = FailureMode::parse(&args.failure_mode).map_err(ToolError::invalid)?;
        reject_charter_secrets(&args.patched_charter)?;
        if soul::contains_secret_like(&args.task_summary)
            || soul::contains_secret_like(&args.root_cause)
            || args
                .diff_summary
                .iter()
                .any(|delta| soul::contains_secret_like(delta))
            || args
                .memory_correction
                .as_deref()
                .is_some_and(soul::contains_secret_like)
        {
            return Err(ToolError::Failed(
                "Refused secret-like material in bot_evolve payload.".into(),
            ));
        }
        let memory = self.require_memory()?;
        let evolution = self.require_evolution()?;
        let now = chrono::Utc::now().timestamp();
        let request = MutateBotRequest {
            bot_id: args.bot_id,
            failure_mode,
            task_summary: args.task_summary,
            root_cause: args.root_cause,
            patched_charter: charter_from_input(args.patched_charter),
            diff_summary: args.diff_summary,
            memory_correction: args.memory_correction,
            probation_runs: args.probation_runs,
        };
        let revision = evolution
            .propose_mutation(memory, request, now)
            .map_err(map_store_io_error)?;
        let mut payload = json!({
            "evolved": true,
            "revision": revision,
        });
        vault::redact_json(&mut payload);
        Ok(payload)
    }

    fn run_bot_decommission(&self, args: BotDecommissionArguments) -> Result<Value, ToolError> {
        if soul::contains_secret_like(&args.reason) {
            return Err(ToolError::Failed(
                "Refused secret-like material in decommission reason.".into(),
            ));
        }
        let memory = self.require_memory()?;
        let evolution = self.require_evolution()?;
        let now = chrono::Utc::now().timestamp();
        let outcome = evolution
            .decommission_bot(
                memory,
                &args.bot_id,
                &args.reason,
                args.confirmed.unwrap_or(false),
                now,
            )
            .map_err(map_store_io_error)?;
        let mut payload = json!(outcome);
        vault::redact_json(&mut payload);
        Ok(payload)
    }
}

fn map_store_io_error(error: io::Error) -> ToolError {
    let message = error.to_string();
    if error.kind() == io::ErrorKind::InvalidInput
        && !message.to_ascii_lowercase().contains("secret")
    {
        ToolError::invalid(message)
    } else {
        ToolError::Failed(message)
    }
}

fn parse_memory_search_scopes(
    scope: &str,
    bot_id: Option<&str>,
) -> Result<Vec<MemoryScope>, ToolError> {
    match scope.trim().to_ascii_lowercase().as_str() {
        "shared" => Ok(vec![MemoryScope::Shared]),
        "self" | "bot" => {
            let parsed = MemoryScope::parse_wire("self", bot_id).map_err(map_store_io_error)?;
            Ok(vec![parsed])
        }
        "both" => {
            let parsed = MemoryScope::parse_wire("self", bot_id).map_err(map_store_io_error)?;
            Ok(vec![MemoryScope::Shared, parsed])
        }
        other => Err(ToolError::invalid(format!(
            "Unknown memory scope {other}. Use shared, self, or both."
        ))),
    }
}

fn reject_memory_secrets(title: &str, body: &str, tags: &[String]) -> Result<(), ToolError> {
    if soul::contains_secret_like(title)
        || soul::contains_secret_like(body)
        || tags.iter().any(|tag| soul::contains_secret_like(tag))
    {
        return Err(ToolError::Failed(
            "Refused to store secret-like material in durable memory.".into(),
        ));
    }
    Ok(())
}

fn reject_charter_secrets(charter: &CharterInput) -> Result<(), ToolError> {
    if soul::contains_secret_like(&charter.job)
        || charter
            .anti_jobs
            .iter()
            .any(|item| soul::contains_secret_like(item))
        || soul::contains_secret_like(&charter.voice_and_style)
        || soul::contains_secret_like(&charter.verification_bar)
        || charter
            .allowed_tools
            .iter()
            .any(|tool| soul::contains_secret_like(tool))
        || soul::contains_secret_like(&charter.instructions)
    {
        return Err(ToolError::Failed(
            "Refused secret-like material in bot charter.".into(),
        ));
    }
    Ok(())
}

fn charter_from_input(input: CharterInput) -> BotCharter {
    BotCharter {
        job: input.job,
        anti_jobs: input.anti_jobs,
        voice_and_style: input.voice_and_style,
        verification_bar: input.verification_bar,
        allowed_tools: input.allowed_tools,
        instructions: input.instructions,
    }
}

fn charter_schema() -> Value {
    object_schema(
        json!({
            "job": text_schema(240),
            "anti_jobs": {
                "type": "array",
                "minItems": 1,
                "maxItems": 12,
                "items": text_schema(240),
            },
            "voice_and_style": {"type": "string", "maxLength": 300},
            "verification_bar": text_schema(400),
            "allowed_tools": {
                "type": "array",
                "maxItems": 32,
                "items": text_schema(64),
            },
            "instructions": text_schema(4_000),
        }),
        &["job", "anti_jobs", "verification_bar", "instructions"],
    )
}

fn map_vault_io_error(error: io::Error) -> ToolError {
    match error.kind() {
        io::ErrorKind::InvalidInput => ToolError::invalid(error.to_string()),
        _ => ToolError::Failed(error.to_string()),
    }
}

fn bounded_inbox_limit(limit: Option<usize>) -> Result<usize, ToolError> {
    match limit {
        None => Ok(DEFAULT_INBOX_LIMIT),
        Some(value) if (1..=MAX_INBOX_LIMIT).contains(&value) => Ok(value),
        Some(_) => Err(ToolError::invalid(format!(
            "limit must be between 1 and {MAX_INBOX_LIMIT}."
        ))),
    }
}

fn validate_decision_id(raw: &str) -> Result<String, ToolError> {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        return Err(ToolError::invalid(
            "Decision id must be 1-48 characters of A-Za-z0-9_- starting with a letter or digit.",
        ));
    }
    Ok(trimmed.to_owned())
}

fn validate_optional_text(
    label: &str,
    raw: Option<&str>,
    max_chars: usize,
) -> Result<String, ToolError> {
    let Some(value) = raw else {
        return Ok(String::new());
    };
    if value.contains('\0') {
        return Err(ToolError::invalid(format!(
            "{label} must not contain NUL bytes."
        )));
    }
    if value.chars().count() > max_chars {
        return Err(ToolError::invalid(format!(
            "{label} must be at most {max_chars} characters."
        )));
    }
    Ok(value.trim().to_owned())
}

fn contains_placeholder_token(text: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case("tbd") || token.eq_ignore_ascii_case("todo"))
}

fn reject_placeholder_text(label: &str, text: &str) -> Result<(), ToolError> {
    if contains_placeholder_token(text) {
        return Err(ToolError::invalid(format!(
            "{label} must not contain placeholder text (TBD/TODO)."
        )));
    }
    Ok(())
}

fn validate_decision_options(
    inputs: Vec<DecisionOptionInput>,
) -> Result<Vec<UiDecisionOption>, ToolError> {
    if !(2..=4).contains(&inputs.len()) {
        return Err(ToolError::invalid(
            "options must contain between 2 and 4 concrete choices.",
        ));
    }
    let mut seen_ids = Vec::with_capacity(inputs.len());
    let mut validated = Vec::with_capacity(inputs.len());
    for option in inputs {
        let id = option.id.trim();
        let valid_id = !id.is_empty()
            && id.len() <= 40
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        if !valid_id {
            return Err(ToolError::invalid(
                "Each decision option id must be 1-40 characters of A-Za-z0-9_-.",
            ));
        }
        if seen_ids
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(id))
        {
            return Err(ToolError::invalid(format!(
                "Duplicate decision option id: {id}."
            )));
        }
        control::bounded_text("option.label", &option.label, 160)?;
        reject_placeholder_text("option.label", &option.label)?;
        let summary = validate_optional_text("option.summary", Some(&option.summary), 500)?;
        reject_placeholder_text("option.summary", &summary)?;
        seen_ids.push(id.to_owned());
        validated.push(UiDecisionOption {
            id: id.to_owned(),
            label: vault::redact_values(option.label.trim()),
            summary: vault::redact_values(&summary),
            recommended: option.recommended,
        });
    }
    Ok(validated)
}

fn parse_task_kind(raw: &str) -> Result<TaskKind, ToolError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "mission" => Ok(TaskKind::Mission),
        "spawn" => Ok(TaskKind::Spawn),
        "goal" => Ok(TaskKind::Goal),
        "review" => Ok(TaskKind::Review),
        _ => Err(ToolError::invalid(
            "kind must be one of: mission, spawn, goal, review.",
        )),
    }
}

fn parse_task_status(raw: &str) -> Result<TaskStatus, ToolError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "queued" => Ok(TaskStatus::Queued),
        "running" => Ok(TaskStatus::Running),
        "needs_approval" => Ok(TaskStatus::NeedsApproval),
        "done" => Ok(TaskStatus::Done),
        "error" => Ok(TaskStatus::Error),
        "snoozed" => Ok(TaskStatus::Snoozed),
        "cancelled" => Ok(TaskStatus::Cancelled),
        _ => Err(ToolError::invalid(
            "status must be one of: queued, running, needs_approval, done, error, snoozed, cancelled.",
        )),
    }
}

fn resolve_safe_ledger_doc_path(
    docs_dir: &Path,
    raw: &str,
) -> Result<(String, PathBuf), ToolError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 160
        || trimmed.contains('\0')
        || trimmed.contains('\r')
        || trimmed.contains('\n')
        || trimmed.contains('\\')
    {
        return Err(ToolError::invalid(
            "path must be a relative Markdown path using forward slashes.",
        ));
    }
    let stripped = trimmed.strip_prefix("ledger-docs/").unwrap_or(trimmed);
    if stripped.is_empty() {
        return Err(ToolError::invalid("path must name a Markdown file."));
    }
    let relative = Path::new(stripped);
    let mut parts = Vec::new();
    let mut full = docs_dir.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(segment) => {
                let text = segment
                    .to_str()
                    .ok_or_else(|| ToolError::invalid("path segments must be valid UTF-8."))?;
                if text.is_empty() || text.starts_with('.') {
                    return Err(ToolError::invalid(
                        "Hidden path segments are not allowed for ledger documents.",
                    ));
                }
                parts.push(text.to_owned());
                full.push(segment);
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                return Err(ToolError::invalid(
                    "path must stay strictly inside the ledger docs directory without '..' or roots.",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(ToolError::invalid("path must name a Markdown file."));
    }
    let is_markdown = full
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
    if !is_markdown {
        return Err(ToolError::invalid(
            "Ledger documents must have a .md extension.",
        ));
    }
    Ok((parts.join("/"), full))
}

fn verify_no_symlink_ancestors(root: &Path, target: &Path) -> Result<(), ToolError> {
    if let Ok(meta) = fs::symlink_metadata(root) {
        if meta.file_type().is_symlink() {
            return Err(ToolError::Failed(
                "Refused to access symlinked ledger docs directory.".into(),
            ));
        }
    }
    let Ok(relative) = target.strip_prefix(root) else {
        return Err(ToolError::invalid(
            "Ledger document path escapes the ledger docs directory.",
        ));
    };
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        if let Ok(meta) = fs::symlink_metadata(&cursor) {
            if meta.file_type().is_symlink() {
                return Err(ToolError::Failed(
                    "Refused to follow symlink inside ledger docs directory.".into(),
                ));
            }
        }
    }
    Ok(())
}

fn write_doc_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Document path has no parent")
    })?;
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("doc.md");
    let temporary = parent.join(format!(".{stem}.{}.next", uuid::Uuid::new_v4().simple()));
    let mut file = fs::File::create_new(&temporary)?;
    let write_outcome = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_outcome {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn is_gh_auth_error(stderr: &str) -> bool {
    let lowered = stderr.to_ascii_lowercase();
    lowered.contains("not logged into any github hosts")
        || lowered.contains("gh auth login")
        || lowered.contains("authentication failed")
        || lowered.contains("token in") && lowered.contains("invalid")
}

fn validate_positive_number(label: &str, value: u64) -> Result<(), ToolError> {
    if value == 0 {
        return Err(ToolError::invalid(format!("{label} must be at least 1.")));
    }
    Ok(())
}

fn validate_enum<'a>(label: &str, value: &'a str, allowed: &[&str]) -> Result<&'a str, ToolError> {
    let trimmed = value.trim();
    if allowed.contains(&trimmed) {
        Ok(trimmed)
    } else {
        Err(ToolError::invalid(format!(
            "{label} must be one of: {}.",
            allowed.join(", ")
        )))
    }
}

fn bounded_github_limit(limit: Option<usize>) -> Result<usize, ToolError> {
    match limit {
        None => Ok(DEFAULT_GITHUB_LIMIT),
        Some(value) if (1..=MAX_GITHUB_LIMIT).contains(&value) => Ok(value),
        Some(_) => Err(ToolError::invalid(format!(
            "limit must be between 1 and {MAX_GITHUB_LIMIT}"
        ))),
    }
}

enum ToolError {
    Rpc(RpcError),
    Failed(String),
}

impl ToolError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Rpc(RpcError::params(message))
    }
}

impl From<Failure> for ToolError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Invalid(message) => Self::Rpc(RpcError::params(message)),
            Failure::Refused(message) => Self::Failed(message),
        }
    }
}

fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments)
        .map_err(|error| ToolError::Rpc(RpcError::params(format!("Invalid arguments: {error}"))))
}

fn bounded_limit(limit: Option<usize>) -> Result<usize, ToolError> {
    match limit {
        None => Ok(control::DEFAULT_READ_LIMIT),
        Some(limit) if (1..=MAX_READ_LIMIT).contains(&limit) => Ok(limit),
        Some(_) => Err(ToolError::Rpc(RpcError::params(format!(
            "limit must be between 1 and {MAX_READ_LIMIT}"
        )))),
    }
}

fn tool_result(reply: Reply) -> Value {
    match reply {
        Reply::Result(value) => json!({
            "content": [{"type": "text", "text": value.to_string()}],
            "structuredContent": value,
            "isError": false,
        }),
        Reply::ToolFailure(message) => json!({
            "content": [{"type": "text", "text": message}],
            "isError": true,
        }),
    }
}

fn error_response(id: Value, error: RpcError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": error.code, "message": error.message},
    })
}

fn name_schema() -> Value {
    json!({"type": "string", "pattern": "^[A-Za-z][A-Za-z0-9_-]{0,23}$"})
}

fn text_schema(limit: usize) -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": limit})
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn tool(name: &str, description: &str, schema: Value, read_only: bool) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": {"readOnlyHint": read_only, "destructiveHint": false, "openWorldHint": false},
    })
}

fn brief_schema() -> Value {
    let field = text_schema(control::MAX_BRIEF_FIELD_CHARS);
    let fields = [
        "role",
        "goal",
        "context",
        "requirements",
        "done_when",
        "do_not",
        "return",
    ];
    let properties: serde_json::Map<String, Value> = fields
        .iter()
        .map(|field_name| ((*field_name).to_owned(), field.clone()))
        .collect();
    object_schema(Value::Object(properties), &fields)
}

pub(crate) fn tool_definitions() -> Vec<Value> {
    let workspace = json!({"type": "string", "maxLength": control::MAX_PATH_CHARS, "description": "Directory relative to the server workspace root; .. and paths outside the root are refused."});
    let positive_int = json!({"type": "integer", "minimum": 1});
    vec![
        tool(
            "list_threads",
            "List every Agent Command thread with role, status and workspace.",
            object_schema(json!({}), &[]),
            true,
        ),
        tool(
            "search_threads",
            "Find threads whose name, role, mission or transcript contains the query.",
            object_schema(
                json!({"query": text_schema(control::MAX_QUERY_CHARS)}),
                &["query"],
            ),
            true,
        ),
        tool(
            "read_thread",
            "Read a thread summary and transcript entries after sequence number `since`.",
            object_schema(
                json!({
                    "name": name_schema(),
                    "since": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LIMIT},
                }),
                &["name"],
            ),
            true,
        ),
        tool(
            "create_thread",
            "Create an idle specialist thread in its own git worktree or project directory.",
            object_schema(
                json!({
                    "name": name_schema(),
                    "role": text_schema(control::MAX_ROLE_CHARS),
                    "task": text_schema(control::MAX_TEXT_CHARS),
                    "workspace": workspace.clone(),
                }),
                &["name", "role"],
            ),
            false,
        ),
        tool(
            "launch_mission",
            "Start a mission on an idle thread. Refused while the thread is running.",
            object_schema(
                json!({"name": name_schema(), "task": text_schema(control::MAX_TEXT_CHARS)}),
                &["name", "task"],
            ),
            false,
        ),
        tool(
            "message_thread",
            "Send a follow-up: runs now when idle, otherwise queues after the current mission.",
            object_schema(
                json!({"name": name_schema(), "message": text_schema(control::MAX_TEXT_CHARS)}),
                &["name", "message"],
            ),
            false,
        ),
        tool(
            "delegate_task",
            "Create the specialist if needed and run a structured seven-field brief in its workspace.",
            object_schema(
                json!({"name": name_schema(), "brief": brief_schema(), "workspace": workspace.clone()}),
                &["name", "brief"],
            ),
            false,
        ),
        tool(
            "wait_thread",
            "Block until the thread is idle with an empty queue, or until timeout_ms passes.",
            object_schema(
                json!({
                    "name": name_schema(),
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": control::MAX_WAIT.as_millis() as u64},
                }),
                &["name"],
            ),
            true,
        ),
        tool(
            "interrupt_thread",
            "Stop the running mission and drop queued messages.",
            object_schema(json!({"name": name_schema()}), &["name"]),
            false,
        ),
        tool(
            "spawn_worker",
            "Spawn a worker asynchronously in a git worktree (or project dir) and return worker_id, branch, and workspace immediately. A reactive <background-result> arrives on completion.",
            object_schema(
                json!({
                    "name": name_schema(),
                    "role": text_schema(control::MAX_ROLE_CHARS),
                    "task": text_schema(control::MAX_TEXT_CHARS),
                    "model": text_schema(control::MAX_MODEL_CHARS),
                    "mode": {"type": "string", "enum": ["read-only", "supervised", "auto-edits", "worktree-auto", "workspace-auto"]},
                    "workspace": workspace,
                }),
                &["name", "role", "task"],
            ),
            false,
        ),
        tool(
            "message_worker",
            "Send a follow-up message to a worker; runs immediately if idle or queues behind active work.",
            object_schema(
                json!({"worker_id": name_schema(), "message": text_schema(control::MAX_TEXT_CHARS)}),
                &["worker_id", "message"],
            ),
            false,
        ),
        tool(
            "read_worker",
            "Read a worker summary and transcript entries after sequence number `since`.",
            object_schema(
                json!({
                    "worker_id": name_schema(),
                    "since": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LIMIT},
                }),
                &["worker_id"],
            ),
            true,
        ),
        tool(
            "list_workers",
            "List active and finished workers with status, branch, mode, and workspace.",
            object_schema(json!({}), &[]),
            true,
        ),
        tool(
            "interrupt_worker",
            "Kill the worker's running mission immediately and drop queued messages.",
            object_schema(json!({"worker_id": name_schema()}), &["worker_id"]),
            false,
        ),
        tool(
            "ask_user",
            "Ask the user a concise structured question with 1-3 clear choices.",
            object_schema(
                json!({
                    "question": text_schema(control::MAX_TEXT_CHARS),
                    "choices": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ASK_CHOICES,
                        "items": text_schema(240),
                    },
                    "context": text_schema(control::MAX_BRIEF_FIELD_CHARS),
                    "allow_freeform": {"type": "boolean"},
                }),
                &["question", "choices"],
            ),
            false,
        ),
        tool(
            "remember",
            "Persist a durable user preference or learning to SOUL.md (secrets are refused).",
            object_schema(json!({"note": text_schema(500)}), &["note"]),
            false,
        ),
        tool(
            "search_memory",
            "Search durable SOUL.md learnings and worker thread history.",
            object_schema(
                json!({
                    "query": text_schema(control::MAX_QUERY_CHARS),
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MEMORY_LIMIT},
                }),
                &["query"],
            ),
            true,
        ),
        tool(
            "tool_search",
            "Search user-configured external MCP tools by keyword or server name.",
            object_schema(
                json!({
                    "query": {"type": "string", "maxLength": control::MAX_QUERY_CHARS},
                    "server": text_schema(80),
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50},
                }),
                &["query"],
            ),
            true,
        ),
        tool(
            "tool_describe",
            "Fetch full inputSchema and auth state for one external MCP tool.",
            object_schema(
                json!({"server": text_schema(80), "name": text_schema(120)}),
                &["server", "name"],
            ),
            true,
        ),
        tool(
            "tool_call",
            "Invoke an external MCP tool by server and name (returns needs_auth if sign-in is required).",
            object_schema(
                json!({
                    "server": text_schema(80),
                    "name": text_schema(120),
                    "arguments": {"type": "object"},
                }),
                &["server", "name"],
            ),
            false,
        ),
        tool(
            "github_pr_list",
            "List pull requests via gh CLI.",
            object_schema(
                json!({
                    "state": {"type": "string", "enum": ["open", "closed", "merged", "all"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_GITHUB_LIMIT},
                    "author": text_schema(80),
                    "head": text_schema(120),
                    "base": text_schema(120),
                }),
                &[],
            ),
            true,
        ),
        tool(
            "github_pr_view",
            "View full pull request metadata, head SHA, review state, and files via gh CLI.",
            object_schema(json!({"number": positive_int.clone()}), &["number"]),
            true,
        ),
        tool(
            "github_pr_create",
            "Create a pull request via gh CLI.",
            object_schema(
                json!({
                    "title": text_schema(256),
                    "body": {"type": "string", "maxLength": control::MAX_TEXT_CHARS},
                    "base": text_schema(120),
                    "head": text_schema(120),
                    "draft": {"type": "boolean"},
                }),
                &["title"],
            ),
            false,
        ),
        tool(
            "github_pr_checks",
            "Inspect CI status checks for a pull request via gh CLI.",
            object_schema(json!({"number": positive_int.clone()}), &["number"]),
            true,
        ),
        tool(
            "github_issue_list",
            "List GitHub issues via gh CLI.",
            object_schema(
                json!({
                    "state": {"type": "string", "enum": ["open", "closed", "all"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_GITHUB_LIMIT},
                    "label": text_schema(80),
                    "assignee": text_schema(80),
                }),
                &[],
            ),
            true,
        ),
        tool(
            "github_issue_view",
            "View a GitHub issue and its comments via gh CLI.",
            object_schema(json!({"number": positive_int.clone()}), &["number"]),
            true,
        ),
        tool(
            "github_issue_create",
            "Create a GitHub issue via gh CLI.",
            object_schema(
                json!({
                    "title": text_schema(256),
                    "body": {"type": "string", "maxLength": control::MAX_TEXT_CHARS},
                    "labels": {"type": "array", "items": text_schema(80)},
                }),
                &["title"],
            ),
            false,
        ),
        tool(
            "github_pr_merge",
            "Merge a pull request via gh CLI after verifying its live head SHA matches expected_head_sha.",
            object_schema(
                json!({
                    "number": positive_int,
                    "expected_head_sha": {"type": "string", "minLength": 7, "maxLength": 40},
                    "method": {"type": "string", "enum": ["merge", "squash", "rebase"]},
                    "delete_branch": {"type": "boolean"},
                }),
                &["number", "expected_head_sha"],
            ),
            false,
        ),
        tool(
            "env_status",
            "Return live Agent Command environment status: workspace, git state, active provider/profile headroom, worker/inbox/ledger counts, vault metadata count, and available MCP tools.",
            object_schema(json!({}), &[]),
            true,
        ),
        tool(
            "inbox_list",
            "List active Inbox alerts and structured operator Decision cards.",
            object_schema(
                json!({
                    "include_dismissed": {"type": "boolean"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_INBOX_LIMIT},
                }),
                &[],
            ),
            true,
        ),
        tool(
            "inbox_post",
            "Post a concise notice or attention item to the operator Inbox.",
            object_schema(
                json!({
                    "title": text_schema(200),
                    "detail": text_schema(control::MAX_TEXT_CHARS),
                    "level": {"type": "string", "enum": ["info", "attention", "warning", "error"]},
                    "thread": text_schema(80),
                }),
                &["title", "detail"],
            ),
            false,
        ),
        tool(
            "inbox_submit_decision",
            "Submit a structured 2-4 option operator decision card to the Decision Inbox (TBD/TODO placeholders are refused).",
            object_schema(
                json!({
                    "id": {"type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9_-]{0,47}$"},
                    "kind": {"type": "string", "enum": ["decision", "design", "risk", "ops", "release"]},
                    "title": text_schema(200),
                    "question": text_schema(1000),
                    "recommendation": text_schema(1000),
                    "options": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 4,
                        "items": object_schema(
                            json!({
                                "id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{1,40}$"},
                                "label": text_schema(160),
                                "summary": {"type": "string", "maxLength": 500},
                                "recommended": {"type": "boolean"},
                            }),
                            &["id", "label"],
                        ),
                    },
                    "directions": {"type": "string", "maxLength": 2000},
                }),
                &["title", "question", "recommendation", "options"],
            ),
            false,
        ),
        tool(
            "inbox_update_decision",
            "Update an existing Decision Inbox card with optional optimistic concurrency check (expected_version).",
            object_schema(
                json!({
                    "id": {"type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9_-]{0,47}$"},
                    "expected_version": {"type": "integer", "minimum": 1},
                    "status": {"type": "string", "enum": ["open", "deferred", "explain_more", "retired", "resolved"]},
                    "title": text_schema(200),
                    "question": text_schema(1000),
                    "recommendation": text_schema(1000),
                    "options": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 4,
                        "items": object_schema(
                            json!({
                                "id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{1,40}$"},
                                "label": text_schema(160),
                                "summary": {"type": "string", "maxLength": 500},
                                "recommended": {"type": "boolean"},
                            }),
                            &["id", "label"],
                        ),
                    },
                    "directions": {"type": "string", "maxLength": 2000},
                }),
                &["id"],
            ),
            false,
        ),
        tool(
            "inbox_dismiss",
            "Dismiss an Inbox item or retire a decision by key (refuses unresolved gate:* ProdGate approvals).",
            object_schema(json!({"key": text_schema(160)}), &["key"]),
            false,
        ),
        tool(
            "ledger_list",
            "List tasks tracked in the durable Task Ledger with optional status filter.",
            object_schema(
                json!({
                    "filter": {"type": "string", "enum": ["all", "active", "attention", "done"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200},
                }),
                &[],
            ),
            true,
        ),
        tool(
            "ledger_record",
            "Record a new mission, spawn, goal, or review task in the durable Task Ledger.",
            object_schema(
                json!({
                    "kind": {"type": "string", "enum": ["mission", "spawn", "goal", "review"]},
                    "owner": text_schema(80),
                    "title": text_schema(300),
                    "status": {"type": "string", "enum": ["queued", "running", "needs_approval", "done", "error", "snoozed", "cancelled"]},
                    "branch": text_schema(120),
                }),
                &["kind", "owner", "title"],
            ),
            false,
        ),
        tool(
            "ledger_update",
            "Update an existing Task Ledger entry by short id or owner name.",
            object_schema(
                json!({
                    "id_or_owner": text_schema(80),
                    "status": {"type": "string", "enum": ["queued", "running", "needs_approval", "done", "error", "snoozed", "cancelled"]},
                    "branch": text_schema(120),
                    "changed_files": {"type": "integer", "minimum": 0},
                }),
                &["id_or_owner", "status"],
            ),
            false,
        ),
        tool(
            "ledger_read_doc",
            "Read a Markdown document strictly inside the private ledger docs directory.",
            object_schema(json!({"path": text_schema(160)}), &["path"]),
            true,
        ),
        tool(
            "ledger_append_doc",
            "Atomically append a Markdown section to a document inside the private ledger docs directory (secrets are refused).",
            object_schema(
                json!({
                    "path": text_schema(160),
                    "section": {"type": "string", "minLength": 1, "maxLength": MAX_LEDGER_DOC_BYTES},
                }),
                &["path", "section"],
            ),
            false,
        ),
        tool(
            "ui_post_table",
            "Emit a structured Markdown/TUI table widget into the Commander chat transcript.",
            object_schema(
                json!({
                    "title": text_schema(200),
                    "headers": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_TABLE_COLUMNS,
                        "items": text_schema(80),
                    },
                    "rows": {
                        "type": "array",
                        "maxItems": MAX_TABLE_ROWS,
                        "items": {
                            "type": "array",
                            "maxItems": MAX_TABLE_COLUMNS,
                            "items": {"type": "string", "maxLength": 500},
                        },
                    },
                    "alignments": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_TABLE_COLUMNS,
                        "items": {"type": "string", "enum": ["left", "center", "right"]},
                    },
                    "caption": {"type": "string", "maxLength": 300},
                }),
                &["title", "headers", "rows"],
            ),
            false,
        ),
        tool(
            "ui_post_card",
            "Emit a structured summary card widget with key-value fields and optional Markdown body into the Commander chat transcript.",
            object_schema(
                json!({
                    "title": text_schema(200),
                    "level": {"type": "string", "enum": ["info", "success", "warning", "error"]},
                    "badge": text_schema(40),
                    "fields": {
                        "type": "array",
                        "maxItems": MAX_CARD_FIELDS,
                        "items": object_schema(
                            json!({
                                "label": text_schema(80),
                                "value": {"type": "string", "maxLength": 500},
                            }),
                            &["label", "value"],
                        ),
                    },
                    "body_markdown": {"type": "string", "maxLength": control::MAX_TEXT_CHARS},
                }),
                &["title"],
            ),
            false,
        ),
        tool(
            "vault_list",
            "List encrypted vault credential metadata (name, namespace, description, timestamps, sha256_8). Never returns raw secret values.",
            object_schema(
                json!({"namespace": text_schema(vault::MAX_NAMESPACE_LEN)}),
                &[],
            ),
            true,
        ),
        tool(
            "vault_check",
            "Check whether required secret names exist in the encrypted vault without exposing any secret values.",
            object_schema(
                json!({
                    "names": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_VAULT_CHECK_NAMES,
                        "items": text_schema(vault::MAX_SECRET_NAME_LEN),
                    },
                    "namespace": text_schema(vault::MAX_NAMESPACE_LEN),
                }),
                &["names"],
            ),
            true,
        ),
        tool(
            "vault_request",
            "Request that the operator store a missing secret via the masked TUI prompt or `agent-command vault set`. Returns status and metadata only, never the raw value.",
            object_schema(
                json!({
                    "name": text_schema(vault::MAX_SECRET_NAME_LEN),
                    "title": text_schema(200),
                    "namespace": text_schema(vault::MAX_NAMESPACE_LEN),
                    "description": {"type": "string", "maxLength": vault::MAX_DESCRIPTION_LEN},
                }),
                &["name", "title"],
            ),
            false,
        ),
        tool(
            "vault_delete",
            "Delete a named credential entry from the encrypted vault.",
            object_schema(
                json!({
                    "name": text_schema(vault::MAX_SECRET_NAME_LEN),
                    "namespace": text_schema(vault::MAX_NAMESPACE_LEN),
                }),
                &["name"],
            ),
            false,
        ),
        tool(
            "memory_search",
            "Search two-tier durable memory (shared team memory and/or bot-private memory) within a token budget.",
            object_schema(
                json!({
                    "scope": {"type": "string", "enum": ["shared", "self", "both"]},
                    "bot_id": text_schema(MAX_BOT_ID_CHARS),
                    "query": {"type": "string", "maxLength": control::MAX_QUERY_CHARS},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_ACTIVE_ENTRIES},
                    "token_budget": {"type": "integer", "minimum": 32, "maximum": 16_000},
                }),
                &["scope", "query"],
            ),
            true,
        ),
        tool(
            "memory_write",
            "Write or reinforce a durable memory entry in shared or bot-private memory (secrets are refused).",
            object_schema(
                json!({
                    "scope": {"type": "string", "enum": ["shared", "self"]},
                    "bot_id": text_schema(MAX_BOT_ID_CHARS),
                    "kind": {
                        "type": "string",
                        "enum": [
                            "fact",
                            "decision",
                            "convention",
                            "lesson",
                            "warning",
                            "playbook",
                            "tool_recipe",
                            "correction",
                            "preference",
                        ],
                    },
                    "title": text_schema(MAX_TITLE_CHARS),
                    "body": text_schema(MAX_BODY_CHARS),
                    "tags": {
                        "type": "array",
                        "maxItems": MAX_TAGS,
                        "items": text_schema(MAX_TAG_CHARS),
                    },
                    "author": text_schema(48),
                    "salience": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                    "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                    "pinned": {"type": "boolean"},
                    "ttl_secs": {"type": "integer", "minimum": 60},
                }),
                &["scope", "kind", "title", "body"],
            ),
            false,
        ),
        tool(
            "memory_supersede",
            "Supersede an existing active memory entry with an updated replacement while preserving audit lineage.",
            object_schema(
                json!({
                    "scope": {"type": "string", "enum": ["shared", "self"]},
                    "bot_id": text_schema(MAX_BOT_ID_CHARS),
                    "target_id": text_schema(64),
                    "kind": {
                        "type": "string",
                        "enum": [
                            "fact",
                            "decision",
                            "convention",
                            "lesson",
                            "warning",
                            "playbook",
                            "tool_recipe",
                            "correction",
                            "preference",
                        ],
                    },
                    "title": text_schema(MAX_TITLE_CHARS),
                    "body": text_schema(MAX_BODY_CHARS),
                    "tags": {
                        "type": "array",
                        "maxItems": MAX_TAGS,
                        "items": text_schema(MAX_TAG_CHARS),
                    },
                    "author": text_schema(48),
                    "salience": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                    "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                    "pinned": {"type": "boolean"},
                    "ttl_secs": {"type": "integer", "minimum": 60},
                }),
                &["scope", "target_id", "kind", "title", "body"],
            ),
            false,
        ),
        tool(
            "memory_delete",
            "Tombstone a memory entry with an explicit reason (privacy scrubs content immediately).",
            object_schema(
                json!({
                    "scope": {"type": "string", "enum": ["shared", "self"]},
                    "bot_id": text_schema(MAX_BOT_ID_CHARS),
                    "id": text_schema(64),
                    "reason": {"type": "string", "enum": ["incorrect", "obsolete", "privacy", "user_request"]},
                    "actor": text_schema(48),
                }),
                &["scope", "id", "reason"],
            ),
            false,
        ),
        tool(
            "bot_scorecard",
            "Inspect statistical competence scorecards (Wilson LCB, Beta posterior, lifecycle, healthcheck findings) for one or all forged bots.",
            object_schema(
                json!({"bot_id": text_schema(MAX_BOT_ID_CHARS)}),
                &[],
            ),
            true,
        ),
        tool(
            "bot_forge",
            "Forge a narrowly scoped specialist bot with an explicit charter, anti-jobs, verification bar, and initial probation.",
            object_schema(
                json!({
                    "bot_id": name_schema(),
                    "role": text_schema(40),
                    "work_class": {"type": "string", "enum": ["coding", "review", "research", "docs", "ops", "general"]},
                    "charter": charter_schema(),
                    "permission_mode": {"type": "string", "enum": ["read-only", "supervised", "auto-edits", "worktree-auto", "workspace-auto"]},
                    "preferred_model": text_schema(80),
                    "pinned_core": {"type": "boolean"},
                    "initial_memory_seeds": {
                        "type": "array",
                        "maxItems": 8,
                        "items": text_schema(MAX_BODY_CHARS),
                    },
                    "probation_runs": {"type": "integer", "minimum": MIN_PROBATION_RUNS, "maximum": MAX_PROBATION_RUNS},
                    "rationale": text_schema(400),
                }),
                &["bot_id", "role", "work_class", "charter", "rationale"],
            ),
            false,
        ),
        tool(
            "bot_record_feedback",
            "Record user approval/disapproval or objective verification outcome for a forged bot, updating probation or rollback state.",
            object_schema(
                json!({
                    "bot_id": name_schema(),
                    "signal": {
                        "type": "string",
                        "enum": [
                            "user_approved",
                            "user_disapproved",
                            "user_reverted",
                            "first_try_pass",
                            "verification_failure",
                            "review_rejected",
                            "stall",
                            "escalation",
                        ],
                    },
                    "task_id": text_schema(64),
                    "duration_ms": {"type": "integer", "minimum": 0},
                    "note": text_schema(400),
                }),
                &["bot_id", "signal"],
            ),
            false,
        ),
        tool(
            "bot_evolve",
            "Record a structured failure reflection and propose a versioned charter mutation with probation gating and optional bot-memory correction.",
            object_schema(
                json!({
                    "bot_id": name_schema(),
                    "failure_mode": {
                        "type": "string",
                        "enum": [
                            "wrong_output",
                            "scope_creep",
                            "verification_missed",
                            "stall",
                            "user_rejected",
                            "tool_misuse",
                        ],
                    },
                    "task_summary": text_schema(400),
                    "root_cause": text_schema(400),
                    "patched_charter": charter_schema(),
                    "diff_summary": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 12,
                        "items": text_schema(240),
                    },
                    "memory_correction": text_schema(600),
                    "probation_runs": {"type": "integer", "minimum": MIN_PROBATION_RUNS, "maximum": MAX_PROBATION_RUNS},
                }),
                &["bot_id", "failure_mode", "task_summary", "root_cause", "patched_charter", "diff_summary"],
            ),
            false,
        ),
        tool(
            "bot_decommission",
            "Principled decommissioning of a bot: distills durable lessons into shared memory and preserves audit history (pinned_core bots return needs_confirmation until confirmed=true).",
            object_schema(
                json!({
                    "bot_id": name_schema(),
                    "reason": text_schema(400),
                    "confirmed": {"type": "boolean"},
                }),
                &["bot_id", "reason"],
            ),
            false,
        ),
    ]
}

pub(crate) fn serve_stdio(
    server: Arc<Server>,
    input: impl BufRead,
    output: impl Write + Send + 'static,
) -> io::Result<()> {
    let output = Arc::new(Mutex::new(output));
    let mut workers = Vec::new();
    for message in Messages::new(input) {
        let server = Arc::clone(&server);
        let output = Arc::clone(&output);
        let message = message?;
        workers.push(thread::spawn(move || {
            let response = match message {
                Message::Text(text) => server.handle_text(&text),
                Message::Oversized => Some(error_response(
                    Value::Null,
                    RpcError::new(INVALID_REQUEST, "Message exceeds 1 MiB"),
                )),
            };
            if let Some(response) = response {
                let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = writeln!(output, "{response}").and_then(|()| output.flush());
            }
        }));
        workers.retain(|worker| !worker.is_finished());
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

enum Message {
    Text(String),
    Oversized,
}

struct Messages<R> {
    input: R,
}

impl<R: BufRead> Messages<R> {
    fn new(input: R) -> Self {
        Self { input }
    }

    fn skip_line(&mut self) -> io::Result<()> {
        let mut discard = Vec::new();
        self.input.read_until(b'\n', &mut discard).map(|_| ())
    }
}

impl<R: BufRead> Iterator for Messages<R> {
    type Item = io::Result<Message>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut buffer = Vec::new();
            let limit = (MAX_MESSAGE_BYTES + 1) as u64;
            let read = match io::Read::take(&mut self.input, limit).read_until(b'\n', &mut buffer) {
                Ok(read) => read,
                Err(error) => return Some(Err(error)),
            };
            if read == 0 {
                return None;
            }
            if buffer.len() > MAX_MESSAGE_BYTES {
                if !buffer.ends_with(b"\n") {
                    if let Err(error) = self.skip_line() {
                        return Some(Err(error));
                    }
                }
                return Some(Ok(Message::Oversized));
            }
            let text = String::from_utf8_lossy(&buffer).trim().to_owned();
            if !text.is_empty() {
                return Some(Ok(Message::Text(text)));
            }
        }
    }
}

const USAGE: &str =
    "Usage: agent-command mcp [--root PATH] [--scope supervised|read-only]\n       agent-command mcp --connect http://HOST:PORT [--token-env VAR]\n\nSpeaks MCP JSON-RPC 2.0 over stdio, one JSON message per line. With --connect it bridges stdio to a remote daemon's authenticated /mcp endpoint.";

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Remote {
    pub(crate) url: String,
    pub(crate) token_env: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Options {
    pub(crate) root: std::path::PathBuf,
    pub(crate) scope: Scope,
    pub(crate) remote: Option<Remote>,
}

pub(crate) fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut root = None;
    let mut scope = Scope::Supervised;
    let mut url = None;
    let mut token_env = None;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--connect" => url = Some(words.next().ok_or("--connect requires a URL")?.clone()),
            "--token-env" => {
                token_env = Some(
                    words
                        .next()
                        .ok_or("--token-env requires a variable")?
                        .clone(),
                )
            }
            "--root" => root = Some(words.next().ok_or("--root requires a path")?.into()),
            "--scope" => scope = Scope::parse(words.next().ok_or("--scope requires a value")?)?,
            other => return Err(format!("Unknown mcp argument {other}. {USAGE}")),
        }
    }
    if token_env.is_some() && url.is_none() {
        return Err("--token-env needs --connect".into());
    }
    let root = match root {
        Some(root) => root,
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    Ok(Options {
        root,
        scope,
        remote: url.map(|url| Remote { url, token_env }),
    })
}

fn forward(client: &connect::Client, text: &str) -> io::Result<Option<Value>> {
    let message: Value = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            return Ok(Some(error_response(
                Value::Null,
                RpcError::new(PARSE_ERROR, format!("Parse error: {error}")),
            )))
        }
    };
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let response = client.post_json("/mcp", &message)?;
    match response.status {
        200 => serde_json::from_slice(&response.body)
            .map(Some)
            .map_err(io::Error::other),
        202 => Ok(None),
        401 => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Daemon refused the token (401 Unauthorized)",
        )),
        status => Ok(Some(error_response(
            id,
            RpcError::new(INTERNAL_ERROR, format!("Daemon answered HTTP {status}")),
        ))),
    }
}

pub(crate) fn bridge(
    client: &connect::Client,
    input: impl BufRead,
    output: &mut (impl Write + Send),
) -> io::Result<()> {
    let output = Mutex::new(output);
    let failure: Mutex<Option<io::Error>> = Mutex::new(None);
    thread::scope(|scope| {
        for message in Messages::new(input) {
            if failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()
            {
                break;
            }
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    record_bridge_error(&failure, error);
                    break;
                }
            };
            let output_ref = &output;
            let failure_ref = &failure;
            scope.spawn(move || {
                let result = match message {
                    Message::Text(text) => forward(client, &text),
                    Message::Oversized => Ok(Some(error_response(
                        Value::Null,
                        RpcError::new(INVALID_REQUEST, "Message exceeds 1 MiB"),
                    ))),
                };
                match result {
                    Ok(Some(response)) => {
                        let mut writer = output_ref.lock().unwrap_or_else(PoisonError::into_inner);
                        if let Err(error) =
                            writeln!(writer, "{response}").and_then(|()| writer.flush())
                        {
                            record_bridge_error(failure_ref, error);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => record_bridge_error(failure_ref, error),
                }
            });
        }
    });
    match failure.into_inner().unwrap_or_else(PoisonError::into_inner) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn record_bridge_error(slot: &Mutex<Option<io::Error>>, error: io::Error) {
    let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.is_none() {
        *guard = Some(error);
    }
}

fn run_bridge(remote: &Remote) -> io::Result<()> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    let target = connect::Target::parse(&remote.url).map_err(invalid)?;
    let token = connect::resolve_token(
        &target,
        remote.token_env.as_deref(),
        |name| std::env::var(name).ok(),
        || crate::daemon::Token::load_existing(&crate::auth::root()?),
    )?;
    let client = connect::Client::new(target, token);
    client.health()?;
    bridge(&client, io::stdin().lock(), &mut io::stdout())
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let options =
        parse_options(args).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if let Some(remote) = &options.remote {
        return run_bridge(remote);
    }
    let profile_root = crate::auth::root()?;
    let ledger_root = profile_root.join("agent-command-ledger-v1");
    let ui = UiBridge::new().with_ledger_paths(
        ledger_root.join("ledger.json"),
        ledger_root.join("ledger-docs"),
    );
    let vault = VaultStore::open(profile_root.clone());
    let memory = MemoryStore::for_workspace(profile_root.clone(), &options.root);
    let evolution = EvolutionStore::for_workspace(profile_root.clone(), &options.root);
    let control = Control::open(
        crate::specialists::Store::new(profile_root.clone()),
        &options.root,
        Box::new(control::NativeEngine::new(profile_root)),
    )?;
    let server = Arc::new(
        Server::new(control, options.scope)
            .with_ui(ui)
            .with_vault(vault)
            .with_memory(memory)
            .with_evolution(evolution),
    );
    serve_stdio(server, io::stdin().lock(), io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bridge::{
            AskUserAnswer, EnvSnapshot, ExternalServerSpec, ExternalToolExecutor, ExternalToolSpec,
            LoopbackBridge, UiEvent, VaultRequestOutcome,
        },
        control::tests::{control, Scratch},
    };
    use zeroize::Zeroizing;

    fn server(scratch: &Scratch, scope: Scope) -> Server {
        let ledger_root = scratch.0.join("ledger-state");
        let ui = UiBridge::new().with_ledger_paths(
            ledger_root.join("ledger.json"),
            ledger_root.join("ledger-docs"),
        );
        let state_root = scratch.0.join("vault-profiles");
        let memory = MemoryStore::for_workspace(state_root.clone(), &scratch.0);
        let evolution = EvolutionStore::for_workspace(state_root.clone(), &scratch.0);
        Server::new(control(scratch, false), scope)
            .with_soul(SoulStore::new(scratch.0.join("SOUL.md")))
            .with_ui(ui)
            .with_vault(VaultStore::open(state_root))
            .with_memory(memory)
            .with_evolution(evolution)
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn call(server: &Server, tool: &str, arguments: Value) -> Value {
        server
            .handle(request(
                9,
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
            ))
            .unwrap()
    }

    fn structured(response: &Value) -> &Value {
        assert_eq!(response["result"]["isError"], false, "{response}");
        &response["result"]["structuredContent"]
    }

    fn error_code(response: &Value) -> i64 {
        response["error"]["code"].as_i64().unwrap_or_default()
    }

    fn brief() -> Value {
        json!({
            "role": "builder", "goal": "Ship", "context": "Repo", "requirements": "Tests",
            "done_when": "Green", "do_not": "Break main", "return": "PR",
        })
    }

    #[test]
    fn initialize_negotiates_protocol_and_lists_all_tools() {
        let scratch = Scratch::new("mcp-init");
        let server = server(&scratch, Scope::Supervised);
        let response = server
            .handle(request(
                1,
                "initialize",
                json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "openbot", "version": "1"}}),
            ))
            .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(response["result"]["serverInfo"]["name"], "agent-command");
        let unknown = server
            .handle(request(
                2,
                "initialize",
                json!({"protocolVersion": "1999-01-01"}),
            ))
            .unwrap();
        assert_eq!(unknown["result"]["protocolVersion"], LATEST_PROTOCOL);
        assert!(server
            .handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());
        let tools = server.handle(request(3, "tools/list", json!({}))).unwrap();
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for expected in [
            "list_threads",
            "search_threads",
            "read_thread",
            "create_thread",
            "launch_mission",
            "message_thread",
            "delegate_task",
            "wait_thread",
            "interrupt_thread",
            "spawn_worker",
            "message_worker",
            "read_worker",
            "list_workers",
            "interrupt_worker",
            "ask_user",
            "remember",
            "search_memory",
            "tool_search",
            "tool_describe",
            "tool_call",
            "github_pr_list",
            "github_pr_view",
            "github_pr_create",
            "github_pr_checks",
            "github_issue_list",
            "github_issue_view",
            "github_issue_create",
            "github_pr_merge",
            "env_status",
            "inbox_list",
            "inbox_post",
            "inbox_submit_decision",
            "inbox_update_decision",
            "inbox_dismiss",
            "ledger_list",
            "ledger_record",
            "ledger_update",
            "ledger_read_doc",
            "ledger_append_doc",
            "ui_post_table",
            "ui_post_card",
            "vault_list",
            "vault_check",
            "vault_request",
            "vault_delete",
            "memory_search",
            "memory_write",
            "memory_supersede",
            "memory_delete",
            "bot_scorecard",
            "bot_forge",
            "bot_record_feedback",
            "bot_evolve",
            "bot_decommission",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
        for tool in tools["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
    }

    #[test]
    fn every_tool_round_trips_through_tools_call() {
        let scratch = Scratch::new("mcp-tools");
        let server = server(&scratch, Scope::Supervised);
        let created = call(
            &server,
            "create_thread",
            json!({"name": "nova", "role": "reviewer", "task": "Review"}),
        );
        assert_eq!(structured(&created)["thread"]["status"], "IDLE");
        let launched = call(
            &server,
            "launch_mission",
            json!({"name": "nova", "task": "Check"}),
        );
        assert_eq!(structured(&launched)["thread"]["status"], "RUNNING");
        let waited = call(
            &server,
            "wait_thread",
            json!({"name": "nova", "timeout_ms": 5000}),
        );
        assert_eq!(structured(&waited)["finished"], true);
        let messaged = call(
            &server,
            "message_thread",
            json!({"name": "nova", "message": "More"}),
        );
        assert_eq!(structured(&messaged)["thread"]["mission"], "More");
        call(&server, "wait_thread", json!({"name": "nova"}));
        let read = call(&server, "read_thread", json!({"name": "nova", "limit": 2}));
        assert_eq!(structured(&read)["entries"].as_array().unwrap().len(), 2);
        let listed = call(&server, "list_threads", json!({}));
        assert_eq!(structured(&listed)["threads"][0]["name"], "nova");
        let found = call(&server, "search_threads", json!({"query": "MORE"}));
        assert_eq!(structured(&found)["threads"].as_array().unwrap().len(), 1);
        let delegated = call(
            &server,
            "delegate_task",
            json!({"name": "forge", "brief": brief()}),
        );
        assert_eq!(structured(&delegated)["thread"]["role"], "builder");
        call(&server, "wait_thread", json!({"name": "forge"}));
        let interrupted = call(&server, "interrupt_thread", json!({"name": "forge"}));
        assert_eq!(structured(&interrupted)["thread"]["running"], false);
        let missing = call(&server, "read_thread", json!({"name": "ghost"}));
        assert_eq!(missing["result"]["isError"], true);
        let text = call(&server, "list_threads", json!({}))["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(serde_json::from_str::<Value>(&text).is_ok());
    }

    #[test]
    fn commander_worker_ask_user_memory_and_lazy_catalog_tools_round_trip() {
        struct MockExt;
        impl ExternalToolExecutor for MockExt {
            fn execute(&self, server: &str, tool: &str, arguments: Value) -> ExternalCallResult {
                ExternalCallResult::Ok(json!({"server": server, "tool": tool, "args": arguments}))
            }
        }

        let scratch = Scratch::new("mcp-commander-tools");
        let catalog = ExternalToolCatalog::with_executor(Arc::new(MockExt));
        catalog.register_server(ExternalServerSpec {
            name: "porkbun".into(),
            transport: "http".into(),
            connected: false,
            auth_hint: Some("Connect Porkbun to update DNS.".into()),
        });
        catalog.register_tool(ExternalToolSpec {
            server: "porkbun".into(),
            name: "dns_edit".into(),
            description: "Edit DNS records".into(),
            input_schema: json!({"type": "object"}),
            tags: vec!["dns".into()],
        });

        let broker = AskUserBroker::new(true);

        let server = server(&scratch, Scope::Commander)
            .with_catalog(catalog.clone())
            .with_ask_user(broker.clone());

        let spawned = call(
            &server,
            "spawn_worker",
            json!({"name": "bolt", "role": "builder", "task": "Implement bridge", "model": "codex"}),
        );
        let spawned_data = structured(&spawned);
        assert_eq!(spawned_data["worker_id"], "bolt");
        assert_eq!(spawned_data["mode"], "workspace-auto");
        assert!(spawned_data["note"]
            .as_str()
            .unwrap()
            .contains("<background-result>"));

        call(&server, "wait_thread", json!({"name": "bolt"}));
        let workers = call(&server, "list_workers", json!({}));
        assert_eq!(structured(&workers)["workers"][0]["name"], "bolt");
        let read = call(&server, "read_worker", json!({"worker_id": "bolt"}));
        assert!(!structured(&read)["entries"].as_array().unwrap().is_empty());
        let messaged = call(
            &server,
            "message_worker",
            json!({"worker_id": "bolt", "message": "Add one more test"}),
        );
        assert_eq!(
            structured(&messaged)["worker"]["mission"],
            "Add one more test"
        );
        call(&server, "wait_thread", json!({"name": "bolt"}));
        let interrupted = call(&server, "interrupt_worker", json!({"worker_id": "bolt"}));
        assert_eq!(structured(&interrupted)["worker"]["running"], false);

        let responder = broker.clone();
        let responder_thread = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                let pending = responder.pending();
                if let Some(req) = pending.first() {
                    responder.answer(
                        &req.request_id,
                        AskUserAnswer {
                            selected_index: Some(0),
                            answer: "MIT".into(),
                        },
                    );
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let asked = call(
            &server,
            "ask_user",
            json!({"question": "Pick license", "choices": ["MIT", "Apache-2.0"]}),
        );
        responder_thread.join().unwrap();
        assert_eq!(structured(&asked)["status"], "answered");
        assert_eq!(structured(&asked)["answer"], "MIT");

        let bad_ask = call(
            &server,
            "ask_user",
            json!({"question": "Too many", "choices": ["a", "b", "c", "d"]}),
        );
        assert_eq!(error_code(&bad_ask), INVALID_PARAMS);

        let saved = call(
            &server,
            "remember",
            json!({"note": "Prefer squash merges on feature branches"}),
        );
        assert_eq!(structured(&saved)["saved"], true);
        let secret_denied = call(
            &server,
            "remember",
            json!({"note": "Use ghp_1234567890abcdef1234567890abcdef1234"}),
        );
        assert_eq!(secret_denied["result"]["isError"], true);

        let memory = call(&server, "search_memory", json!({"query": "squash"}));
        assert_eq!(
            structured(&memory)["learnings"].as_array().unwrap().len(),
            1
        );

        let hits = call(&server, "tool_search", json!({"query": "dns"}));
        assert_eq!(structured(&hits)["tools"][0]["server"], "porkbun");
        let desc = call(
            &server,
            "tool_describe",
            json!({"server": "porkbun", "name": "dns_edit"}),
        );
        assert_eq!(structured(&desc)["connected"], false);
        let needs_auth = call(
            &server,
            "tool_call",
            json!({"server": "porkbun", "name": "dns_edit", "arguments": {"zone": "x.dev"}}),
        );
        assert_eq!(structured(&needs_auth)["status"], "needs_auth");

        catalog.set_server_connected("porkbun", true);
        let called = call(
            &server,
            "tool_call",
            json!({"server": "porkbun", "name": "dns_edit", "arguments": {"zone": "x.dev"}}),
        );
        assert_eq!(structured(&called)["status"], "ok");
    }

    type GhResponder = Box<dyn Fn(&[String]) -> GhOutput + Send + Sync>;

    struct MockGh {
        recorded: Mutex<Vec<Vec<String>>>,
        responder: GhResponder,
    }

    impl GhExecutor for MockGh {
        fn run(&self, _cwd: &Path, args: &[String]) -> io::Result<GhOutput> {
            self.recorded.lock().unwrap().push(args.to_vec());
            Ok((self.responder)(args))
        }
    }

    #[test]
    fn github_tools_support_prs_issues_checks_and_head_sha_verified_merge() {
        let scratch = Scratch::new("mcp-github");
        let mock = Arc::new(MockGh {
            recorded: Mutex::new(Vec::new()),
            responder: Box::new(|args| {
                let first = args.first().map(String::as_str).unwrap_or_default();
                let second = args.get(1).map(String::as_str).unwrap_or_default();
                match (first, second) {
                    ("pr", "list") => GhOutput {
                        exit_code: 0,
                        stdout: r#"[{"number":42,"title":"Overhaul","state":"OPEN"}]"#.into(),
                        stderr: String::new(),
                    },
                    ("pr", "view") => GhOutput {
                        exit_code: 0,
                        stdout: r#"{"number":42,"headRefOid":"abcdef1234567890abcdef1234567890abcdef12","state":"OPEN","url":"https://github.com/o/r/pull/42"}"#.into(),
                        stderr: String::new(),
                    },
                    ("pr", "create") => GhOutput {
                        exit_code: 0,
                        stdout: "https://github.com/o/r/pull/43\n".into(),
                        stderr: String::new(),
                    },
                    ("pr", "checks") => GhOutput {
                        exit_code: 8,
                        stdout: r#"[{"name":"ci","state":"PENDING","bucket":"pending"}]"#.into(),
                        stderr: String::new(),
                    },
                    ("pr", "merge") => GhOutput {
                        exit_code: 0,
                        stdout: "Merged pull request #42\n".into(),
                        stderr: String::new(),
                    },
                    ("issue", "list") => GhOutput {
                        exit_code: 0,
                        stdout: r#"[{"number":7,"title":"Fix bug","state":"OPEN"}]"#.into(),
                        stderr: String::new(),
                    },
                    ("issue", "view") => GhOutput {
                        exit_code: 0,
                        stdout: r#"{"number":7,"title":"Fix bug","body":"Details"}"#.into(),
                        stderr: String::new(),
                    },
                    ("issue", "create") => GhOutput {
                        exit_code: 0,
                        stdout: "https://github.com/o/r/issues/8\n".into(),
                        stderr: String::new(),
                    },
                    _ => GhOutput {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "unexpected gh command".into(),
                    },
                }
            }),
        });
        let server = server(&scratch, Scope::Commander).with_gh_executor(mock.clone());

        let prs = call(
            &server,
            "github_pr_list",
            json!({"state": "open", "limit": 5}),
        );
        assert_eq!(structured(&prs)["pull_requests"][0]["number"], 42);

        let pr_view = call(&server, "github_pr_view", json!({"number": 42}));
        assert_eq!(structured(&pr_view)["pull_request"]["number"], 42);

        let created_pr = call(
            &server,
            "github_pr_create",
            json!({"title": "New PR", "body": "Body", "draft": true}),
        );
        assert_eq!(
            structured(&created_pr)["url"],
            "https://github.com/o/r/pull/43"
        );

        let checks = call(&server, "github_pr_checks", json!({"number": 42}));
        assert_eq!(structured(&checks)["checks"][0]["state"], "PENDING");

        let issues = call(&server, "github_issue_list", json!({}));
        assert_eq!(structured(&issues)["issues"][0]["number"], 7);

        let issue = call(&server, "github_issue_view", json!({"number": 7}));
        assert_eq!(structured(&issue)["issue"]["title"], "Fix bug");

        let created_issue = call(
            &server,
            "github_issue_create",
            json!({"title": "New issue", "labels": ["bug"]}),
        );
        assert_eq!(
            structured(&created_issue)["url"],
            "https://github.com/o/r/issues/8"
        );

        let stale_merge = call(
            &server,
            "github_pr_merge",
            json!({"number": 42, "expected_head_sha": "deadbeef1234"}),
        );
        assert_eq!(stale_merge["result"]["isError"], true);
        assert!(stale_merge["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("head SHA moved"));

        let ok_merge = call(
            &server,
            "github_pr_merge",
            json!({"number": 42, "expected_head_sha": "abcdef12", "method": "squash", "delete_branch": true}),
        );
        assert_eq!(structured(&ok_merge)["merged"], true);
        let calls = mock.recorded.lock().unwrap();
        let merge_args = calls.last().unwrap();
        assert!(merge_args.contains(&"--match-head-commit".to_owned()));
        assert!(merge_args.contains(&"abcdef1234567890abcdef1234567890abcdef12".to_owned()));
        assert!(merge_args.contains(&"--delete-branch".to_owned()));
    }

    #[test]
    fn loopback_bridge_wraps_mcp_server_end_to_end() {
        let scratch = Scratch::new("mcp-loopback");
        let server = Arc::new(server(&scratch, Scope::Commander));
        let bridge = LoopbackBridge::start(server).unwrap();
        let target = connect::Target::parse(&bridge.base_url()).unwrap();
        let token = crate::daemon::Token::parse(bridge.token()).unwrap();
        let client = connect::Client::new(target, token);
        let response = client
            .post_json(
                "/mcp",
                &request(
                    1,
                    "tools/call",
                    json!({"name": "list_workers", "arguments": {}}),
                ),
            )
            .unwrap();
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["result"]["isError"], false);
        assert!(body["result"]["structuredContent"]["workers"].is_array());
    }

    #[test]
    fn invalid_input_is_rejected_with_invalid_params() {
        let scratch = Scratch::new("mcp-invalid");
        let server = server(&scratch, Scope::Supervised);
        call(
            &server,
            "create_thread",
            json!({"name": "nova", "role": "reviewer"}),
        );
        for (tool, arguments) in [
            ("create_thread", json!({"name": "../evil", "role": "x"})),
            ("create_thread", json!({"name": "ok", "role": "x;rm"})),
            ("create_thread", json!({"name": "ok"})),
            ("launch_mission", json!({"name": "nova", "task": ""})),
            (
                "launch_mission",
                json!({"name": "nova", "task": "x", "dangerously_skip_permissions": true}),
            ),
            (
                "launch_mission",
                json!({"name": "nova", "task": "x", "permission_mode": "bypassPermissions"}),
            ),
            ("read_thread", json!({"name": "nova", "limit": 0})),
            (
                "wait_thread",
                json!({"name": "nova", "timeout_ms": 600_001}),
            ),
            ("search_threads", json!({"query": "x".repeat(201)})),
            (
                "delegate_task",
                json!({"name": "forge", "brief": {"role": "x"}}),
            ),
            ("list_threads", json!({"extra": 1})),
            ("no_such_tool", json!({})),
            (
                "spawn_worker",
                json!({"name": "bad", "role": "builder", "task": "x", "mode": "full-access"}),
            ),
            (
                "github_pr_merge",
                json!({"number": 1, "expected_head_sha": "short"}),
            ),
        ] {
            let response = call(&server, tool, arguments.clone());
            assert_eq!(error_code(&response), INVALID_PARAMS, "{tool} {arguments}");
        }
        let response = server
            .handle(request(
                4,
                "tools/call",
                json!({"name": "list_threads", "arguments": []}),
            ))
            .unwrap();
        assert_eq!(error_code(&response), INVALID_PARAMS);
    }

    #[test]
    fn workspace_traversal_is_refused() {
        let scratch = Scratch::new("mcp-traversal");
        let server = server(&scratch, Scope::Supervised);
        let outside = scratch.0.canonicalize().unwrap();
        for workspace in ["../", "sub/../../..", outside.to_str().unwrap(), "/"] {
            for (tool, arguments) in [
                (
                    "create_thread",
                    json!({"name": "nova", "role": "r", "workspace": workspace}),
                ),
                (
                    "delegate_task",
                    json!({"name": "nova", "brief": brief(), "workspace": workspace}),
                ),
                (
                    "spawn_worker",
                    json!({"name": "nova", "role": "r", "task": "x", "workspace": workspace}),
                ),
            ] {
                let response = call(&server, tool, arguments);
                assert_eq!(error_code(&response), INVALID_PARAMS, "{tool} {workspace}");
            }
        }
        assert!(server.control().list().is_empty());
    }

    #[test]
    fn read_only_scope_hides_and_refuses_changes() {
        let scratch = Scratch::new("mcp-read-only");
        let server = server(&scratch, Scope::ReadOnly);
        let tools = server.handle(request(1, "tools/list", json!({}))).unwrap();
        assert_eq!(
            tools["result"]["tools"].as_array().unwrap().len(),
            READ_TOOLS.len()
        );
        for (tool, args) in [
            ("create_thread", json!({"name": "nova", "role": "r"})),
            (
                "spawn_worker",
                json!({"name": "nova", "role": "r", "task": "x"}),
            ),
            ("remember", json!({"note": "test"})),
            ("inbox_post", json!({"title": "Note", "detail": "Details"})),
            ("inbox_dismiss", json!({"key": "agent:1"})),
            (
                "ledger_record",
                json!({"kind": "mission", "owner": "commander", "title": "Ship"}),
            ),
            (
                "ledger_append_doc",
                json!({"path": "notes.md", "section": "hello"}),
            ),
            ("ui_post_card", json!({"title": "Status"})),
            ("vault_delete", json!({"name": "STRIPE_KEY"})),
            (
                "memory_write",
                json!({"scope": "shared", "kind": "fact", "title": "T", "body": "B"}),
            ),
            (
                "memory_delete",
                json!({"scope": "shared", "id": "m1", "reason": "obsolete"}),
            ),
            (
                "bot_decommission",
                json!({"bot_id": "nova", "reason": "unused"}),
            ),
        ] {
            let refused = call(&server, tool, args);
            assert_eq!(refused["result"]["isError"], true, "{tool}");
        }
        assert_eq!(
            structured(&call(&server, "env_status", json!({})))["scope"],
            "read-only"
        );
        assert!(structured(&call(&server, "inbox_list", json!({})))["items"].is_array());
        assert!(structured(&call(&server, "ledger_list", json!({})))["entries"].is_array());
        assert!(structured(&call(&server, "vault_list", json!({})))["secrets"].is_array());
        assert!(structured(&call(
            &server,
            "memory_search",
            json!({"scope": "shared", "query": "rust"})
        ))["hits"]
            .is_array());
        assert!(structured(&call(&server, "bot_scorecard", json!({})))["scorecards"].is_array());
        assert!(server.control().list().is_empty());
    }

    #[test]
    fn memory_and_bot_evolution_mcp_tools_round_trip_with_secret_and_pinned_core_guards() {
        let scratch = Scratch::new("mcp-memory-evolution");
        let server = server(&scratch, Scope::Commander);

        let secret_mem = call(
            &server,
            "memory_write",
            json!({
                "scope": "shared",
                "kind": "fact",
                "title": "Bad secret",
                "body": "Use ghp_1234567890abcdef1234567890abcdef1234 for CI",
            }),
        );
        assert_eq!(secret_mem["result"]["isError"], true);

        let wrote = call(
            &server,
            "memory_write",
            json!({
                "scope": "shared",
                "kind": "convention",
                "title": "Zero comments in Rust",
                "body": "Keep every Rust module strictly comment-free and formatted.",
                "tags": ["rust", "style"],
            }),
        );
        let mem_id = structured(&wrote)["entry"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(structured(&wrote)["status"], "created");

        let searched = call(
            &server,
            "memory_search",
            json!({"scope": "shared", "query": "comment-free", "limit": 5, "token_budget": 400}),
        );
        assert_eq!(structured(&searched)["count"], 1);
        assert_eq!(structured(&searched)["hits"][0]["entry"]["id"], mem_id);

        let superseded = call(
            &server,
            "memory_supersede",
            json!({
                "scope": "shared",
                "target_id": mem_id,
                "kind": "convention",
                "title": "Zero comments in Rust v2",
                "body": "Keep all Rust modules comment-free and verified with clippy -D warnings.",
            }),
        );
        let new_id = structured(&superseded)["entry"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(new_id, mem_id);

        let deleted = call(
            &server,
            "memory_delete",
            json!({"scope": "shared", "id": new_id, "reason": "privacy"}),
        );
        assert_eq!(structured(&deleted)["deleted"], true);
        let after_delete = call(
            &server,
            "memory_search",
            json!({"scope": "shared", "query": "clippy"}),
        );
        assert_eq!(structured(&after_delete)["count"], 0);

        let charter = json!({
            "job": "Audit Rust pull requests for safety and performance regressions.",
            "anti_jobs": ["Never push commits directly to main."],
            "voice_and_style": "Terse findings with line numbers.",
            "verification_bar": "Run clippy and cite exact line numbers.",
            "allowed_tools": ["read_thread", "github_pr_view"],
            "instructions": "Inspect diffs and verify invariants before approving.",
        });
        let forged = call(
            &server,
            "bot_forge",
            json!({
                "bot_id": "sentinel",
                "role": "reviewer",
                "work_class": "review",
                "charter": charter,
                "pinned_core": true,
                "initial_memory_seeds": ["Always verify atomic writes on Windows and Unix."],
                "probation_runs": 2,
                "rationale": "Dedicated safety reviewer for v0.3.",
            }),
        );
        assert_eq!(structured(&forged)["forged"], true);
        assert_eq!(
            structured(&forged)["bot"]["blueprint"]["lifecycle"],
            "probation"
        );

        let bot_mem = call(
            &server,
            "memory_search",
            json!({"scope": "self", "bot_id": "sentinel", "query": "atomic"}),
        );
        assert_eq!(structured(&bot_mem)["count"], 1);

        let fb1 = call(
            &server,
            "bot_record_feedback",
            json!({"bot_id": "sentinel", "signal": "first_try_pass", "task_id": "T-1", "duration_ms": 1200}),
        );
        assert_eq!(structured(&fb1)["recorded"], true);
        let fb2 = call(
            &server,
            "bot_record_feedback",
            json!({"bot_id": "sentinel", "signal": "user_approved", "task_id": "T-2", "duration_ms": 950}),
        );
        assert_eq!(structured(&fb2)["report"]["lifecycle"], "active");

        let evolved = call(
            &server,
            "bot_evolve",
            json!({
                "bot_id": "sentinel",
                "failure_mode": "verification_missed",
                "task_summary": "Missed symlink guard check",
                "root_cause": "Did not inspect symlink_metadata path validation.",
                "patched_charter": {
                    "job": "Audit Rust pull requests for safety, symlink hygiene, and regressions.",
                    "anti_jobs": ["Never push commits directly to main."],
                    "verification_bar": "Verify symlink_metadata and clippy -D warnings.",
                    "instructions": "Inspect diffs, symlink guards, and tests.",
                },
                "diff_summary": ["+ Added symlink_metadata verification requirement"],
                "memory_correction": "Always verify symlink_metadata before touching private paths.",
                "probation_runs": 2,
            }),
        );
        assert_eq!(structured(&evolved)["evolved"], true);
        assert_eq!(structured(&evolved)["revision"]["version"], 2);

        let cards = call(&server, "bot_scorecard", json!({"bot_id": "sentinel"}));
        assert_eq!(structured(&cards)["count"], 1);
        assert_eq!(structured(&cards)["scorecards"][0]["active_revision"], 2);

        let needs_confirm = call(
            &server,
            "bot_decommission",
            json!({"bot_id": "sentinel", "reason": "Replaced by v2 reviewer"}),
        );
        assert_eq!(structured(&needs_confirm)["outcome"], "needs_confirmation");

        let confirmed = call(
            &server,
            "bot_decommission",
            json!({"bot_id": "sentinel", "reason": "Replaced by v2 reviewer", "confirmed": true}),
        );
        assert_eq!(structured(&confirmed)["outcome"], "decommissioned");
        assert!(!structured(&confirmed)["distilled_memory_ids"]
            .as_array()
            .unwrap()
            .is_empty());

        let distilled = call(
            &server,
            "memory_search",
            json!({"scope": "shared", "query": "symlink_metadata"}),
        );
        assert!(structured(&distilled)["count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn self_control_env_inbox_ledger_ui_and_vault_tools_enforce_guards_and_zero_secret_leak() {
        let scratch = Scratch::new("mcp-self-control");
        let ledger_root = scratch.0.join("ledger-state");
        let ui = UiBridge::interactive(None).with_ledger_paths(
            ledger_root.join("ledger.json"),
            ledger_root.join("ledger-docs"),
        );
        ui.set_env_snapshot(EnvSnapshot {
            workspace: scratch.0.display().to_string(),
            git_branch: Some("feat/v0.3-platform".into()),
            git_dirty_files: 3,
            active_provider: Some("codex".into()),
            active_profile: Some("codex-work".into()),
            permission_mode: Some("supervised".into()),
            profiles: Vec::new(),
            external_servers: vec!["github".into()],
        });

        let vault_store = VaultStore::open(scratch.0.join("vault-profiles"));
        let raw_secret = "ac_test_secret_material_9876543210_never_leak";
        vault_store
            .set(
                "STRIPE_API_KEY",
                Zeroizing::new(raw_secret.to_owned()),
                Some("billing"),
                Some("Billing API key"),
            )
            .unwrap();
        let resolved = vault_store
            .resolve_value("STRIPE_API_KEY", Some("billing"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.as_str(), raw_secret);

        let server = Server::new(control(&scratch, false), Scope::Commander)
            .with_soul(SoulStore::new(scratch.0.join("SOUL.md")))
            .with_ui(ui.clone())
            .with_vault(vault_store);

        let env = call(&server, "env_status", json!({}));
        let env_data = structured(&env);
        assert_eq!(env_data["git_branch"], "feat/v0.3-platform");
        assert_eq!(env_data["vault_secret_count"], 1);
        assert!(env_data["mcp_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "ui_post_table"));

        let posted = call(
            &server,
            "inbox_post",
            json!({
                "title": "Check secret handling",
                "detail": format!("Never leak {raw_secret} into inbox"),
                "level": "attention",
            }),
        );
        let posted_text = posted.to_string();
        assert!(!posted_text.contains(raw_secret));
        assert!(posted_text.contains("[REDACTED]"));

        let placeholder_bad = call(
            &server,
            "inbox_submit_decision",
            json!({
                "title": "Deploy plan",
                "question": "TBD later?",
                "recommendation": "Wait",
                "options": [
                    {"id": "a", "label": "Option A"},
                    {"id": "b", "label": "Option B"},
                ],
            }),
        );
        assert_eq!(error_code(&placeholder_bad), INVALID_PARAMS);

        let submitted = call(
            &server,
            "inbox_submit_decision",
            json!({
                "id": "DEC-MCP-1",
                "kind": "design",
                "title": "Choose transport",
                "question": "Use loopback stdio bridge for Claude and agy?",
                "recommendation": "Yes, keeps bearer tokens off disk.",
                "options": [
                    {"id": "stdio", "label": "Stdio bridge", "recommended": true},
                    {"id": "http", "label": "Direct HTTP"},
                ],
            }),
        );
        assert_eq!(structured(&submitted)["decision"]["record_version"], 1);

        let cas_fail = call(
            &server,
            "inbox_update_decision",
            json!({"id": "DEC-MCP-1", "expected_version": 99, "status": "deferred"}),
        );
        assert_eq!(cas_fail["result"]["isError"], true);

        let updated = call(
            &server,
            "inbox_update_decision",
            json!({"id": "DEC-MCP-1", "expected_version": 1, "status": "deferred"}),
        );
        assert_eq!(structured(&updated)["decision"]["record_version"], 2);
        assert_eq!(structured(&updated)["decision"]["status"], "deferred");

        let gate_refused = call(&server, "inbox_dismiss", json!({"key": "gate:prod-1"}));
        assert_eq!(gate_refused["result"]["isError"], true);

        let inbox = call(&server, "inbox_list", json!({}));
        assert_eq!(structured(&inbox)["items"].as_array().unwrap().len(), 1);
        assert_eq!(structured(&inbox)["decisions"].as_array().unwrap().len(), 1);

        let recorded = call(
            &server,
            "ledger_record",
            json!({"kind": "mission", "owner": "builder", "title": "Ship vault", "branch": "feat/v0.3-platform"}),
        );
        let task_id = structured(&recorded)["id"].as_str().unwrap().to_owned();
        let doc_rel_path = structured(&recorded)["entry"]["doc_rel_path"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(doc_rel_path, format!("ledger-docs/active/{task_id}.md"));
        let initial_doc = call(&server, "ledger_read_doc", json!({"path": doc_rel_path}));
        assert!(structured(&initial_doc)["content"]
            .as_str()
            .unwrap()
            .contains("Ship vault"));
        let appended_entry_doc = call(
            &server,
            "ledger_append_doc",
            json!({"path": doc_rel_path, "section": "## Verification\nShared doc path matches."}),
        );
        assert_eq!(structured(&appended_entry_doc)["appended"], true);
        let reloaded_ledger = Ledger::load(&ledger_root.join("ledger.json"));
        let tui_doc = reloaded_ledger
            .read_document(&ledger_root, &task_id)
            .unwrap();
        assert!(tui_doc.contains("Ship vault"));
        assert!(tui_doc.contains("Shared doc path matches."));
        assert!(!ledger_root.join("ledger-docs").join("ledger-docs").exists());

        let ledger_upd = call(
            &server,
            "ledger_update",
            json!({"id_or_owner": task_id, "status": "done", "changed_files": 7}),
        );
        assert_eq!(structured(&ledger_upd)["updated"], true);
        let listed_ledger = call(&server, "ledger_list", json!({"filter": "done"}));
        assert_eq!(
            structured(&listed_ledger)["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        for bad_doc in [
            "../escape.md",
            "ledger-docs/../escape.md",
            "/abs.md",
            "notes.txt",
            ".hidden.md",
            "a\\b.md",
        ] {
            let err = call(
                &server,
                "ledger_append_doc",
                json!({"path": bad_doc, "section": "hi"}),
            );
            assert_eq!(error_code(&err), INVALID_PARAMS, "{bad_doc}");
        }
        let secret_doc = call(
            &server,
            "ledger_append_doc",
            json!({"path": "tasks/M-001.md", "section": "token ghp_1234567890abcdef1234567890abcdef1234"}),
        );
        assert_eq!(secret_doc["result"]["isError"], true);

        let appended = call(
            &server,
            "ledger_append_doc",
            json!({"path": "tasks/M-001.md", "section": "## Progress\nVault verified."}),
        );
        assert_eq!(structured(&appended)["appended"], true);
        let read_doc = call(
            &server,
            "ledger_read_doc",
            json!({"path": "tasks/M-001.md"}),
        );
        assert!(structured(&read_doc)["content"]
            .as_str()
            .unwrap()
            .contains("Vault verified."));

        let bad_table = call(
            &server,
            "ui_post_table",
            json!({"title": "Mismatch", "headers": ["A", "B"], "rows": [["only-one"]]}),
        );
        assert_eq!(error_code(&bad_table), INVALID_PARAMS);

        let good_table = call(
            &server,
            "ui_post_table",
            json!({
                "title": "Provider Matrix",
                "headers": ["Provider", "Status"],
                "rows": [["Claude", "Ready"], ["Codex", format!("Leak check {raw_secret}")]],
                "alignments": ["left", "right"],
            }),
        );
        assert_eq!(structured(&good_table)["posted"], true);
        let good_card = call(
            &server,
            "ui_post_card",
            json!({
                "title": "Vault Summary",
                "level": "success",
                "badge": "OK",
                "fields": [{"label": "Backend", "value": VaultStore::backend_name()}],
                "body_markdown": format!("Scrubbed {raw_secret}"),
            }),
        );
        assert_eq!(structured(&good_card)["posted"], true);

        let events = ui.drain_events();
        let table_event = events
            .iter()
            .find_map(|event| match event {
                UiEvent::PostTable(card) => Some(card),
                _ => None,
            })
            .unwrap();
        assert!(!table_event.rows[1][1].contains(raw_secret));
        assert!(table_event.rows[1][1].contains("[REDACTED]"));
        let card_event = events
            .iter()
            .find_map(|event| match event {
                UiEvent::PostCard(card) => Some(card),
                _ => None,
            })
            .unwrap();
        assert!(!card_event.body_markdown.contains(raw_secret));

        let listed_vault = call(&server, "vault_list", json!({"namespace": "billing"}));
        assert!(!listed_vault.to_string().contains(raw_secret));
        assert_eq!(
            structured(&listed_vault)["secrets"][0]["name"],
            "STRIPE_API_KEY"
        );

        let checked_vault = call(
            &server,
            "vault_check",
            json!({"namespace": "billing", "names": ["STRIPE_API_KEY", "MISSING_KEY"]}),
        );
        assert!(!checked_vault.to_string().contains(raw_secret));
        assert_eq!(
            structured(&checked_vault)["present"],
            json!(["STRIPE_API_KEY"])
        );
        assert_eq!(
            structured(&checked_vault)["missing"],
            json!(["MISSING_KEY"])
        );

        let resolver_ui = ui.clone();
        let resolver_thread = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                let pending = resolver_ui.pending_vault_requests();
                if let Some(req) = pending.first() {
                    resolver_ui.resolve_vault_request(
                        &req.request_id,
                        VaultRequestOutcome {
                            status: VaultRequestStatus::Saved,
                            name: req.name.clone(),
                            namespace: req.namespace.clone(),
                            sha256_8: Some("1234abcd".into()),
                        },
                    );
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let requested = call(
            &server,
            "vault_request",
            json!({"name": "DEPLOY_TOKEN", "namespace": "ops", "title": "Need deploy token"}),
        );
        resolver_thread.join().unwrap();
        assert_eq!(structured(&requested)["status"], "saved");
        assert_eq!(structured(&requested)["sha256_8"], "1234abcd");

        let deleted = call(
            &server,
            "vault_delete",
            json!({"name": "STRIPE_API_KEY", "namespace": "billing"}),
        );
        assert_eq!(structured(&deleted)["deleted"], true);
    }

    #[test]
    fn malformed_envelopes_get_json_rpc_errors() {
        let scratch = Scratch::new("mcp-envelope");
        let server = server(&scratch, Scope::Supervised);
        assert_eq!(
            error_code(&server.handle_text("{not json").unwrap()),
            PARSE_ERROR
        );
        assert_eq!(
            error_code(&server.handle_text("[]").unwrap()),
            INVALID_REQUEST
        );
        assert_eq!(
            error_code(
                &server
                    .handle(json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}))
                    .unwrap()
            ),
            INVALID_REQUEST
        );
        assert_eq!(
            error_code(
                &server
                    .handle(json!({"jsonrpc": "2.0", "id": {}, "method": "ping"}))
                    .unwrap()
            ),
            INVALID_REQUEST
        );
        assert_eq!(
            error_code(
                &server
                    .handle(request(1, "resources/list", json!({})))
                    .unwrap()
            ),
            METHOD_NOT_FOUND
        );
        assert_eq!(
            server.handle(request(5, "ping", Value::Null)).unwrap()["result"],
            json!({})
        );
    }

    #[test]
    fn stdio_transport_answers_line_by_line() {
        let scratch = Scratch::new("mcp-stdio");
        let server = Arc::new(server(&scratch, Scope::Supervised));
        let oversized = "x".repeat(MAX_MESSAGE_BYTES + 10);
        let input = format!(
            "{}\n\n{}\n{oversized}\n{}\n",
            request(1, "initialize", json!({})),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            request(2, "tools/list", json!({})),
        );
        let output = SharedBuffer::default();
        serve_stdio(server, input.as_bytes(), output.clone()).unwrap();
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        let responses: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 3);
        assert!(responses.iter().any(|r| r["id"] == 1));
        assert!(responses.iter().any(|r| r["id"] == 2));
        assert!(responses.iter().any(|r| error_code(r) == INVALID_REQUEST));
    }

    #[test]
    fn options_default_to_supervised_and_refuse_full_access() {
        let options = parse_options(&[]).unwrap();
        assert_eq!(options.scope, Scope::Supervised);
        let words = |list: &[&str]| list.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert!(parse_options(&words(&["--scope", "full-access"])).is_err());
        assert!(parse_options(&words(&["--dangerously-skip-permissions"])).is_err());
        assert_eq!(
            parse_options(&words(&["--scope", "read-only", "--root", "/tmp"]))
                .unwrap()
                .scope,
            Scope::ReadOnly
        );
    }

    #[test]
    fn connect_bridge_forwards_stdio_concurrently_without_blocking_on_wait() {
        let running = crate::daemon::tests::start("mcp-bridge", false);
        let target = connect::Target::parse(&format!("http://{}", running.address)).unwrap();
        let client = connect::Client::new(target.clone(), running.token.clone());
        let input = format!(
            "{}\n{}\nnot json\n{}\n",
            request(1, "initialize", json!({})),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            request(
                2,
                "tools/call",
                json!({"name": "list_threads", "arguments": {}})
            ),
        );
        let mut output = Vec::new();
        bridge(&client, input.as_bytes(), &mut output).unwrap();
        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 3);
        let init = responses.iter().find(|r| r["id"] == 1).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "agent-command");
        assert!(responses.iter().any(|r| error_code(r) == PARSE_ERROR));
        let listed = responses.iter().find(|r| r["id"] == 2).unwrap();
        assert_eq!(listed["result"]["isError"], false);

        let held = crate::daemon::tests::start("mcp-bridge-concurrent", true);
        let held_target = connect::Target::parse(&format!("http://{}", held.address)).unwrap();
        let held_client = connect::Client::new(held_target, held.token.clone());
        let setup_input = format!(
            "{}\n{}\n",
            request(
                10,
                "tools/call",
                json!({"name": "create_thread", "arguments": {"name": "hold", "role": "builder"}})
            ),
            request(
                11,
                "tools/call",
                json!({"name": "launch_mission", "arguments": {"name": "hold", "task": "hold forever"}})
            ),
        );
        let mut setup_out = Vec::new();
        for line in setup_input.lines() {
            bridge(&held_client, format!("{line}\n").as_bytes(), &mut setup_out).unwrap();
        }

        let concurrent_input = format!(
            "{}\n{}\n",
            request(
                20,
                "tools/call",
                json!({"name": "wait_thread", "arguments": {"name": "hold", "timeout_ms": 1200}})
            ),
            request(21, "ping", json!({})),
        );
        let mut concurrent_out = Vec::new();
        bridge(
            &held_client,
            concurrent_input.as_bytes(),
            &mut concurrent_out,
        )
        .unwrap();
        let concurrent_responses: Vec<Value> = String::from_utf8(concurrent_out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(concurrent_responses.len(), 2);
        assert_eq!(concurrent_responses[0]["id"], 21);
        assert_eq!(concurrent_responses[1]["id"], 20);

        let stranger = connect::Client::new(target, crate::daemon::Token::generate());
        let denied = bridge(&stranger, input.as_bytes(), &mut Vec::new()).unwrap_err();
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
        let words = |list: &[&str]| list.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            parse_options(&words(&["--connect", "http://h:1", "--token-env", "T"]))
                .unwrap()
                .remote,
            Some(Remote {
                url: "http://h:1".into(),
                token_env: Some("T".into())
            })
        );
        assert!(parse_options(&words(&["--token-env", "T"])).is_err());
    }

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
