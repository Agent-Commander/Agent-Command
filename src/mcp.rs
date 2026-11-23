use crate::{
    connect,
    control::{self, Brief, Control, Failure, Scope},
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Write},
    sync::{Arc, Mutex, PoisonError},
    thread,
    time::Duration,
};

pub(crate) const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: [&str; 3] = [LATEST_PROTOCOL, "2025-03-26", "2024-11-05"];
pub(crate) const MAX_MESSAGE_BYTES: usize = 1 << 20;
const MAX_READ_LIMIT: usize = 200;
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const READ_TOOLS: [&str; 4] = [
    "list_threads",
    "search_threads",
    "read_thread",
    "wait_thread",
];

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
}

impl Server {
    pub(crate) fn new(control: Control, scope: Scope) -> Self {
        Self { control, scope }
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
                "Agent Command control plane. Threads are isolated specialists. Scope: {}. Full access and permission skipping are never available.",
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
                "{} needs supervised scope; this server is read-only.",
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
            other => Err(ToolError::Rpc(RpcError::params(format!(
                "Unknown tool: {other}"
            )))),
        }
    }
}

enum ToolError {
    Rpc(RpcError),
    Failed(String),
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
            "Create an idle specialist thread in its own git worktree or private directory.",
            object_schema(
                json!({
                    "name": name_schema(),
                    "role": text_schema(control::MAX_ROLE_CHARS),
                    "task": text_schema(control::MAX_TEXT_CHARS),
                    "workspace": workspace,
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
            "Create the specialist if needed and run a structured seven-field brief in its isolated workspace.",
            object_schema(
                json!({"name": name_schema(), "brief": brief_schema(), "workspace": workspace}),
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
const INTERNAL_ERROR: i64 = -32603;

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
    output: &mut impl Write,
) -> io::Result<()> {
    for message in Messages::new(input) {
        let response = match message? {
            Message::Text(text) => forward(client, &text)?,
            Message::Oversized => Some(error_response(
                Value::Null,
                RpcError::new(INVALID_REQUEST, "Message exceeds 1 MiB"),
            )),
        };
        if let Some(response) = response {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
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
    use crate::control::tests::{control, Scratch};

    fn server(scratch: &Scratch, scope: Scope) -> Server {
        Server::new(control(scratch, false), scope)
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
        assert_eq!(
            names,
            [
                "list_threads",
                "search_threads",
                "read_thread",
                "create_thread",
                "launch_mission",
                "message_thread",
                "delegate_task",
                "wait_thread",
                "interrupt_thread"
            ]
        );
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
        let refused = call(
            &server,
            "create_thread",
            json!({"name": "nova", "role": "r"}),
        );
        assert_eq!(refused["result"]["isError"], true);
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
    fn connect_bridge_forwards_stdio_to_the_daemon() {
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
        assert_eq!(
            responses[0]["result"]["serverInfo"]["name"],
            "agent-command"
        );
        assert_eq!(error_code(&responses[1]), PARSE_ERROR);
        assert_eq!(responses[2]["result"]["isError"], false);
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
