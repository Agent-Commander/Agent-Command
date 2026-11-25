use crate::{
    bridge::{AskUserBroker, AskUserRequest, ExternalCallResult, ExternalToolCatalog},
    connect,
    control::{self, Brief, Control, Failure, Scope, SpawnWorkerRequest},
    permissions::PermissionMode,
    soul::SoulStore,
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Write},
    path::Path,
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
const ASK_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

const READ_TOOLS: [&str; 14] = [
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
    gh: Arc<dyn GhExecutor>,
}

impl Server {
    pub(crate) fn new(control: Control, scope: Scope) -> Self {
        Self {
            control,
            scope,
            soul: SoulStore::from_env(),
            catalog: ExternalToolCatalog::new(),
            ask_user: AskUserBroker::default(),
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
    let control = Control::open(
        crate::specialists::Store::new(profile_root.clone()),
        &options.root,
        Box::new(control::NativeEngine::new(profile_root)),
    )?;
    let server = Arc::new(Server::new(control, options.scope));
    serve_stdio(server, io::stdin().lock(), io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bridge::{
            AskUserAnswer, ExternalServerSpec, ExternalToolExecutor, ExternalToolSpec,
            LoopbackBridge,
        },
        control::tests::{control, Scratch},
    };

    fn server(scratch: &Scratch, scope: Scope) -> Server {
        Server::new(control(scratch, false), scope)
            .with_soul(SoulStore::new(scratch.0.join("SOUL.md")))
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
        let responder = broker.clone();
        let responder_thread = thread::spawn(move || {
            for _ in 0..100 {
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

        let server = server(&scratch, Scope::Commander)
            .with_catalog(catalog.clone())
            .with_ask_user(broker);

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
        ] {
            let refused = call(&server, tool, args);
            assert_eq!(refused["result"]["isError"], true, "{tool}");
        }
        assert!(server.control().list().is_empty());
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
