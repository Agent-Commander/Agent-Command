use crate::{
    auth,
    control::{self, Control, Scope},
    http::{self, HttpError, Request},
    mcp::Server,
    specialists::Store,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fmt, fs,
    io::{self, BufReader, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) const STATE_DIRECTORY: &str = "agent-command-daemon-v1";
pub(crate) const TOKEN_FILE: &str = "token";
pub(crate) const DEFAULT_PORT: u16 = 47_100;
pub(crate) const TOKEN_HEX_CHARS: usize = 64;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_POLL: Duration = Duration::from_millis(500);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(15);
const STREAM_BATCH: usize = 200;
const ACCEPT_POLL: Duration = Duration::from_millis(50);
const SERVICE_LABEL: &str = "com.agent-command.daemon";
const SYSTEMD_UNIT: &str = "agent-command.service";
const WINDOWS_TASK: &str = "AgentCommandDaemon";

#[derive(Clone)]
pub(crate) struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token(redacted)")
    }
}

impl Token {
    pub(crate) fn generate() -> Self {
        let mut hasher = Sha256::new();
        for _ in 0..4 {
            hasher.update(uuid::Uuid::new_v4().as_bytes());
        }
        Self(hex(&hasher.finalize()))
    }

    pub(crate) fn parse(text: &str) -> io::Result<Self> {
        let text = text.trim();
        if text.len() != TOKEN_HEX_CHARS || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Daemon token must be 64 hexadecimal characters",
            ));
        }
        Ok(Self(text.to_ascii_lowercase()))
    }

    pub(crate) fn secret(&self) -> &str {
        &self.0
    }

    pub(crate) fn matches(&self, presented: &str) -> bool {
        constant_time_equal(
            &Sha256::digest(self.0.as_bytes()),
            &Sha256::digest(presented.trim().as_bytes()),
        )
    }

    pub(crate) fn path(profile_root: &Path) -> PathBuf {
        profile_root.join(STATE_DIRECTORY).join(TOKEN_FILE)
    }

    pub(crate) fn load_existing(profile_root: &Path) -> io::Result<Option<Self>> {
        let Some(directory) = auth::existing_private_state(profile_root, STATE_DIRECTORY)? else {
            return Ok(None);
        };
        match read_private(&directory.join(TOKEN_FILE)) {
            Ok(text) => Self::parse(&text).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn load_or_create(profile_root: &Path) -> io::Result<(Self, bool)> {
        if let Some(token) = Self::load_existing(profile_root)? {
            return Ok((token, false));
        }
        let directory = auth::private_state_directory(profile_root, STATE_DIRECTORY)?;
        let token = Self::generate();
        write_private(&directory.join(TOKEN_FILE), token.0.as_bytes())?;
        Ok((token, true))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(unix)]
fn read_private(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Expected a mode 0600 token file at {}", path.display()),
        ));
    }
    fs::read_to_string(path)
}

#[cfg(not(unix))]
fn read_private(path: &Path) -> io::Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Expected a token file at {}", path.display()),
        ));
    }
    fs::read_to_string(path)
}

pub(crate) fn validate_bind(address: SocketAddr, allow_remote: bool) -> Result<(), String> {
    let ip = address.ip();
    if ip.is_unspecified() {
        return Err(format!(
            "Refusing to bind {ip}: name the loopback, Tailscale or LAN address to listen on."
        ));
    }
    if !ip.is_loopback() && !allow_remote {
        return Err(format!(
            "Binding {ip} exposes the daemon beyond this machine. Pass --allow-remote to confirm; the bearer token is always required."
        ));
    }
    Ok(())
}

pub(crate) fn default_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT)
}

pub(crate) struct Daemon {
    server: Server,
    token: Token,
}

impl Daemon {
    pub(crate) fn new(server: Server, token: Token) -> Self {
        Self { server, token }
    }

    fn authorized(&self, request: &Request) -> bool {
        request
            .header("authorization")
            .and_then(|value| {
                let (scheme, credential) = value.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then_some(credential)
            })
            .is_some_and(|credential| self.token.matches(credential))
    }

    fn control(&self) -> &Control {
        self.server.control()
    }

    fn health(&self) -> Value {
        json!({
            "status": "ok",
            "name": "agent-command",
            "version": env!("CARGO_PKG_VERSION"),
            "scope": self.server.scope().label(),
            "threads": self.control().count(),
        })
    }
}

pub(crate) fn serve(
    listener: TcpListener,
    daemon: Arc<Daemon>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let daemon = Arc::clone(&daemon);
                let shutdown = Arc::clone(&shutdown);
                thread::spawn(move || {
                    if let Err(error) = connection(stream, &daemon, &shutdown) {
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            eprintln!("Connection error: {error}");
                        }
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn connection(stream: TcpStream, daemon: &Daemon, shutdown: &AtomicBool) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let request = match http::read_request(&mut reader) {
        Ok(request) => request,
        Err(HttpError::Closed) => return Ok(()),
        Err(error) => {
            return respond_json(
                &mut writer,
                error.status(),
                &json!({"error": error.to_string()}),
            )
        }
    };
    if !daemon.authorized(&request) {
        return http::write_response(
            &mut writer,
            401,
            "application/json",
            &[("WWW-Authenticate", "Bearer realm=\"agent-command\"")],
            json!({"error": "unauthorized"}).to_string().as_bytes(),
        );
    }
    route(&request, daemon, &mut writer, shutdown)
}

fn respond_json(writer: &mut impl Write, status: u16, body: &Value) -> io::Result<()> {
    http::write_response(
        writer,
        status,
        "application/json",
        &[],
        body.to_string().as_bytes(),
    )
}

fn route(
    request: &Request,
    daemon: &Daemon,
    writer: &mut TcpStream,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    let control = daemon.control();
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => respond_json(writer, 200, &daemon.health()),
        ("GET", "/threads") => respond_json(writer, 200, &json!({"threads": control.list()})),
        ("GET", path) if path.starts_with("/threads/") => {
            let name = &path["/threads/".len()..];
            let since = since(request)?;
            match control.read(name, since, crate::control::DEFAULT_READ_LIMIT) {
                Ok(transcript) => respond_json(writer, 200, &json!(transcript)),
                Err(failure) => respond_json(writer, 404, &json!({"error": failure.message()})),
            }
        }
        ("GET", "/events") => stream_events(request, control, writer, shutdown),
        ("POST", "/mcp") => mcp(request, daemon, writer),
        (_, "/health" | "/threads" | "/events" | "/mcp") => {
            respond_json(writer, 405, &json!({"error": "method not allowed"}))
        }
        _ => respond_json(writer, 404, &json!({"error": "not found"})),
    }
}

fn since(request: &Request) -> io::Result<u64> {
    request.query_value("since").map_or(Ok(0), |text| {
        text.parse::<u64>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "since must be a number"))
    })
}

fn mcp(request: &Request, daemon: &Daemon, writer: &mut TcpStream) -> io::Result<()> {
    let json_body = request
        .header("content-type")
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"));
    if !json_body {
        return respond_json(
            writer,
            415,
            &json!({"error": "Content-Type must be application/json"}),
        );
    }
    let text = String::from_utf8_lossy(&request.body);
    match daemon.server.handle_text(&text) {
        Some(response) => respond_json(writer, 200, &response),
        None => http::write_response(writer, 202, "application/json", &[], b""),
    }
}

fn stream_events(
    request: &Request,
    control: &Control,
    writer: &mut TcpStream,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    let Some(name) = request.query_value("thread") else {
        return respond_json(
            writer,
            400,
            &json!({"error": "thread query parameter required"}),
        );
    };
    let mut seen = match since(request) {
        Ok(seen) => seen,
        Err(error) => return respond_json(writer, 400, &json!({"error": error.to_string()})),
    };
    if let Err(failure) = control.read(name, seen, 1) {
        return respond_json(writer, 404, &json!({"error": failure.message()}));
    }
    http::write_stream_head(writer)?;
    let mut quiet_since = Instant::now();
    while !shutdown.load(Ordering::SeqCst) {
        let Ok(transcript) = control.read(name, seen, STREAM_BATCH) else {
            return writer.write_all(b"event: closed\ndata: {}\n\n");
        };
        for line in &transcript.entries {
            write!(
                writer,
                "id: {}\nevent: entry\ndata: {}\n\n",
                line.seq,
                json!(line)
            )?;
            seen = line.seq;
        }
        if !transcript.entries.is_empty() {
            write!(
                writer,
                "event: status\ndata: {}\n\n",
                json!(transcript.thread)
            )?;
            writer.flush()?;
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= KEEPALIVE_EVERY {
            writer.write_all(b": keepalive\n\n")?;
            writer.flush()?;
            quiet_since = Instant::now();
        }
        control.changed_since(name, seen, STREAM_POLL);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServiceOs {
    Linux,
    Macos,
    Windows,
}

impl ServiceOs {
    pub(crate) fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        match text {
            "linux" => Ok(Self::Linux),
            "macos" => Ok(Self::Macos),
            "windows" => Ok(Self::Windows),
            other => Err(format!(
                "Unknown service OS {other}. Use linux, macos or windows."
            )),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ServiceFile {
    pub(crate) location: Option<PathBuf>,
    pub(crate) contents: String,
    pub(crate) activate: Vec<String>,
}

pub(crate) struct ServiceSpec<'a> {
    pub(crate) executable: &'a Path,
    pub(crate) bind: SocketAddr,
    pub(crate) allow_remote: bool,
    pub(crate) scope: Scope,
    pub(crate) root: &'a Path,
    pub(crate) home: &'a Path,
}

impl ServiceSpec<'_> {
    fn arguments(&self) -> Result<Vec<String>, String> {
        let mut arguments = vec![
            path_text(self.executable)?,
            "daemon".into(),
            "--bind".into(),
            self.bind.to_string(),
            "--root".into(),
            path_text(self.root)?,
            "--scope".into(),
            self.scope.label().into(),
        ];
        if self.allow_remote {
            arguments.push("--allow-remote".into());
        }
        Ok(arguments)
    }
}

pub(crate) fn strip_windows_verbatim(text: &str) -> String {
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(stripped) = text.strip_prefix(r"\\?\") {
        stripped.to_owned()
    } else {
        text.to_owned()
    }
}

fn path_text(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))?;
    let cleaned = strip_windows_verbatim(text);
    if cleaned.chars().any(|c| c.is_control() || c == '"') {
        return Err(format!(
            "{} contains quotes or control characters",
            path.display()
        ));
    }
    Ok(cleaned)
}

fn systemd_quote(argument: &str) -> String {
    format!(
        "\"{}\"",
        argument
            .replace('\\', "\\\\")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
}

pub(crate) fn render_service(os: ServiceOs, spec: &ServiceSpec<'_>) -> Result<ServiceFile, String> {
    let arguments = spec.arguments()?;
    Ok(match os {
        ServiceOs::Linux => render_systemd(&arguments, spec.home),
        ServiceOs::Macos => render_launchd(&arguments, spec.home),
        ServiceOs::Windows => render_windows(&arguments),
    })
}

fn render_systemd(arguments: &[String], home: &Path) -> ServiceFile {
    let command: Vec<String> = arguments
        .iter()
        .map(|argument| systemd_quote(argument))
        .collect();
    let contents = format!(
        "[Unit]\nDescription=Agent Command headless daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={}\nRestart=on-failure\nRestartSec=5\nNoNewPrivileges=true\nUMask=0077\n\n[Install]\nWantedBy=default.target\n",
        command.join(" ")
    );
    ServiceFile {
        location: Some(home.join(".config/systemd/user").join(SYSTEMD_UNIT)),
        contents,
        activate: vec![
            "systemctl --user daemon-reload".into(),
            format!("systemctl --user enable --now {SYSTEMD_UNIT}"),
            "loginctl enable-linger \"$USER\"".into(),
        ],
    }
}

fn render_launchd(arguments: &[String], home: &Path) -> ServiceFile {
    let program: String = arguments
        .iter()
        .map(|argument| format!("    <string>{}</string>\n", xml_escape(argument)))
        .collect();
    let contents = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{SERVICE_LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n{program}  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ProcessType</key>\n  <string>Background</string>\n  <key>Umask</key>\n  <integer>63</integer>\n</dict>\n</plist>\n"
    );
    let location = home
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist"));
    ServiceFile {
        activate: vec![
            format!(
                "launchctl bootstrap gui/$(id -u) \"{}\"",
                location.display()
            ),
            format!("launchctl kickstart -k gui/$(id -u)/{SERVICE_LABEL}"),
        ],
        location: Some(location),
        contents,
    }
}

fn render_windows(arguments: &[String]) -> ServiceFile {
    let action: Vec<String> = arguments
        .iter()
        .map(|argument| format!("\\\"{argument}\\\""))
        .collect();
    let command = format!(
        "schtasks /Create /TN \"{WINDOWS_TASK}\" /SC ONLOGON /RL LIMITED /F /TR \"{}\"",
        action.join(" ")
    );
    ServiceFile {
        location: None,
        contents: format!("{command}\n"),
        activate: vec![command, format!("schtasks /Run /TN \"{WINDOWS_TASK}\"")],
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ServiceAction {
    Print(ServiceOs),
    Install,
}

#[derive(Debug)]
pub(crate) struct Options {
    pub(crate) bind: SocketAddr,
    pub(crate) allow_remote: bool,
    pub(crate) root: Option<PathBuf>,
    pub(crate) scope: Scope,
    pub(crate) service: Option<ServiceAction>,
}

const USAGE: &str = "Usage: agent-command daemon [--bind 127.0.0.1:47100] [--allow-remote] [--root PATH] [--scope supervised|read-only]\n       agent-command daemon --print-service [linux|macos|windows]\n       agent-command daemon --install-service\n\nEvery request needs Authorization: Bearer <token>. The token is generated once, stored mode 0600 under the private state directory, and printed only when first created.\nEndpoints: GET /health, GET /threads, GET /threads/NAME?since=N, GET /events?thread=NAME&since=N (SSE), POST /mcp (MCP JSON-RPC).";

pub(crate) fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        bind: default_bind(),
        allow_remote: false,
        root: None,
        scope: Scope::Supervised,
        service: None,
    };
    let mut words = args.iter().peekable();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--bind" => {
                let text = words.next().ok_or("--bind requires HOST:PORT")?;
                options.bind = text.parse().map_err(|_| {
                    format!("--bind expects an IP:PORT such as 127.0.0.1:{DEFAULT_PORT}")
                })?;
            }
            "--allow-remote" => options.allow_remote = true,
            "--root" => options.root = Some(words.next().ok_or("--root requires a path")?.into()),
            "--scope" => {
                options.scope = Scope::parse(words.next().ok_or("--scope requires a value")?)?
            }
            "--print-service" => {
                let os = match words.peek() {
                    Some(next) if !next.starts_with("--") => {
                        ServiceOs::parse(words.next().expect("peeked"))?
                    }
                    _ => ServiceOs::current(),
                };
                options.service = Some(ServiceAction::Print(os));
            }
            "--install-service" => options.service = Some(ServiceAction::Install),
            other => return Err(format!("Unknown daemon argument {other}.\n{USAGE}")),
        }
    }
    validate_bind(options.bind, options.allow_remote)?;
    Ok(options)
}

fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn service(action: &ServiceAction, options: &Options, root: &Path) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let home = std::env::home_dir().ok_or_else(|| io::Error::other("No home directory"))?;
    let spec = ServiceSpec {
        executable: &executable,
        bind: options.bind,
        allow_remote: options.allow_remote,
        scope: options.scope,
        root,
        home: &home,
    };
    let os = match action {
        ServiceAction::Print(os) => *os,
        ServiceAction::Install => ServiceOs::current(),
    };
    let file = render_service(os, &spec).map_err(invalid_input)?;
    match (action, &file.location) {
        (ServiceAction::Install, Some(location)) => {
            if location.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} already exists; remove it first to reinstall.",
                        location.display()
                    ),
                ));
            }
            if let Some(parent) = location.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(location, &file.contents)?;
            println!("Wrote {}", location.display());
            println!("Nothing was started. To activate, run:");
        }
        (ServiceAction::Install, None) => {
            println!("Windows uses a per-user scheduled task. Nothing was run. To activate, run:");
        }
        (ServiceAction::Print(_), location) => {
            if let Some(location) = location {
                println!("# {}", location.display());
            }
            print!("{}", file.contents);
            println!("# Activate with:");
        }
    }
    for command in &file.activate {
        println!("  {command}");
    }
    Ok(())
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let options = parse_options(args).map_err(invalid_input)?;
    let root = match &options.root {
        Some(root) => root.canonicalize()?,
        None => std::env::current_dir()?.canonicalize()?,
    };
    if let Some(action) = &options.service {
        return service(action, &options, &root);
    }
    let profile_root = auth::root()?;
    let (token, created) = Token::load_or_create(&profile_root)?;
    let control = Control::open(
        Store::new(profile_root.clone()),
        &root,
        Box::new(control::NativeEngine::new(profile_root.clone())),
    )?;
    let listener = TcpListener::bind(options.bind)?;
    let address = listener.local_addr()?;
    eprintln!(
        "Agent Command daemon listening on http://{address} · scope {} · root {}",
        options.scope.label(),
        root.display()
    );
    if created {
        eprintln!(
            "Pairing token (shown once, stored at {}):\n{}",
            Token::path(&profile_root).display(),
            token.secret()
        );
    } else {
        eprintln!("Token: {}", Token::path(&profile_root).display());
    }
    let daemon = Arc::new(Daemon::new(Server::new(control, options.scope), token));
    serve(listener, daemon, Arc::new(AtomicBool::new(false)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        connect,
        control::tests::{control, Scratch},
    };

    pub(crate) struct Running {
        pub(crate) address: SocketAddr,
        pub(crate) token: Token,
        shutdown: Arc<AtomicBool>,
        _scratch: Scratch,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn start(name: &str, hold: bool) -> Running {
        let scratch = Scratch::new(name);
        let token = Token::generate();
        let daemon = Arc::new(Daemon::new(
            Server::new(control(&scratch, hold), Scope::Supervised),
            token.clone(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        thread::spawn(move || serve(listener, daemon, stop).unwrap());
        Running {
            address,
            token,
            shutdown,
            _scratch: scratch,
        }
    }

    fn raw(address: SocketAddr, request: &str) -> http::Response {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        http::read_response(&mut BufReader::new(stream)).unwrap()
    }

    fn rpc(running: &Running, id: u64, tool: &str, arguments: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": arguments}});
        let response = connect::Client::new(
            connect::Target::parse(&format!("http://{}", running.address)).unwrap(),
            running.token.clone(),
        )
        .post_json("/mcp", &body)
        .unwrap();
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["result"]["isError"], false, "{value}");
        value["result"]["structuredContent"].clone()
    }

    #[test]
    fn missing_or_wrong_tokens_get_401_everywhere() {
        let running = start("daemon-auth", false);
        for request in [
            "GET /health HTTP/1.1\r\n\r\n".to_owned(),
            "GET /threads HTTP/1.1\r\nAuthorization: Bearer nope\r\n\r\n".to_owned(),
            format!(
                "GET /health HTTP/1.1\r\nAuthorization: Basic {}\r\n\r\n",
                running.token.secret()
            ),
            format!(
                "GET /health HTTP/1.1\r\nAuthorization: Bearer {}0\r\n\r\n",
                running.token.secret()
            ),
            "GET /nowhere HTTP/1.1\r\n\r\n".to_owned(),
            "POST /mcp HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_owned(),
        ] {
            let response = raw(running.address, &request);
            assert_eq!(response.status, 401, "{request}");
            assert!(!String::from_utf8_lossy(&response.body).contains(running.token.secret()));
            assert!(response
                .headers
                .iter()
                .any(|(name, value)| name == "WWW-Authenticate" && value.starts_with("Bearer")));
        }
    }

    #[test]
    fn valid_token_round_trips_health_list_launch_read_and_interrupt() {
        let running = start("daemon-roundtrip", false);
        let client = connect::Client::new(
            connect::Target::parse(&format!("http://{}", running.address)).unwrap(),
            running.token.clone(),
        );
        let health = client.health().unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["scope"], "supervised");
        rpc(
            &running,
            1,
            "create_thread",
            json!({"name": "nova", "role": "reviewer"}),
        );
        rpc(
            &running,
            2,
            "launch_mission",
            json!({"name": "nova", "task": "hold the line"}),
        );
        let listed: Value = serde_json::from_slice(&client.get("/threads").unwrap().body).unwrap();
        assert_eq!(listed["threads"][0]["status"], "RUNNING");
        let read: Value =
            serde_json::from_slice(&client.get("/threads/nova?since=0").unwrap().body).unwrap();
        assert_eq!(read["entries"][0]["text"], "TASK  hold the line");
        let interrupted = rpc(&running, 3, "interrupt_thread", json!({"name": "nova"}));
        assert_eq!(interrupted["thread"]["name"], "nova");
        let waited = rpc(
            &running,
            4,
            "wait_thread",
            json!({"name": "nova", "timeout_ms": 5000}),
        );
        assert_eq!(waited["finished"], true);
        assert_eq!(waited["thread"]["status"], "ERROR");
        assert_eq!(client.get("/threads/ghost").unwrap().status, 404);
        assert_eq!(client.get("/nowhere").unwrap().status, 404);
        assert_eq!(client.get("/mcp").unwrap().status, 405);
        let notification = client
            .post_json(
                "/mcp",
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )
            .unwrap();
        assert_eq!(notification.status, 202);
    }

    #[test]
    fn mcp_endpoint_requires_json_and_rejects_traversal() {
        let running = start("daemon-mcp", false);
        let request = format!(
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}",
            running.token.secret()
        );
        assert_eq!(raw(running.address, &request).status, 415);
        let client = connect::Client::new(
            connect::Target::parse(&format!("http://{}", running.address)).unwrap(),
            running.token.clone(),
        );
        let response = client
            .post_json(
                "/mcp",
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "create_thread", "arguments": {"name": "nova", "role": "r", "workspace": "../.."}}}),
            )
            .unwrap();
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["error"]["code"], -32602);
    }

    #[test]
    fn event_stream_replays_entries_after_since() {
        let running = start("daemon-events", false);
        rpc(
            &running,
            1,
            "create_thread",
            json!({"name": "echo", "role": "writer"}),
        );
        rpc(
            &running,
            2,
            "launch_mission",
            json!({"name": "echo", "task": "Write"}),
        );
        rpc(&running, 3, "wait_thread", json!({"name": "echo"}));
        let client = connect::Client::new(
            connect::Target::parse(&format!("http://{}", running.address)).unwrap(),
            running.token.clone(),
        );
        let mut seen = Vec::new();
        let last = client
            .stream("echo", 1, |line| {
                seen.push(line.text.clone());
                seen.len() < 2
            })
            .unwrap();
        assert_eq!(last, 3);
        assert_eq!(seen[0], "echo  echo handled: Write");
        assert!(seen[1].starts_with("echo  DONE"));
    }

    #[test]
    fn tokens_are_high_entropy_private_and_compared_safely() {
        let first = Token::generate();
        let second = Token::generate();
        assert_eq!(first.secret().len(), TOKEN_HEX_CHARS);
        assert_ne!(first.secret(), second.secret());
        assert!(first.matches(first.secret()));
        assert!(!first.matches(second.secret()));
        assert!(!first.matches(""));
        assert!(!format!("{first:?}").contains(first.secret()));
        assert!(constant_time_equal(b"abc", b"abc"));
        assert!(!constant_time_equal(b"abc", b"abd"));
        assert!(!constant_time_equal(b"abc", b"abcd"));
        assert!(Token::parse("xyz").is_err());
        let scratch = Scratch::new("token");
        let profiles = scratch.0.join("profiles");
        assert!(Token::load_existing(&profiles).unwrap().is_none());
        let (created, fresh) = Token::load_or_create(&profiles).unwrap();
        assert!(fresh);
        let (loaded, fresh) = Token::load_or_create(&profiles).unwrap();
        assert!(!fresh);
        assert_eq!(created.secret(), loaded.secret());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = Token::path(&profiles);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(Token::load_existing(&profiles).is_err());
        }
    }

    #[test]
    fn binds_default_to_loopback_and_remote_needs_consent() {
        assert_eq!(default_bind().to_string(), "127.0.0.1:47100");
        let parse = |words: &[&str]| {
            parse_options(&words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>())
        };
        assert!(parse(&[]).unwrap().bind.ip().is_loopback());
        assert!(parse(&["--bind", "0.0.0.0:47100"]).is_err());
        assert!(parse(&["--bind", "0.0.0.0:47100", "--allow-remote"]).is_err());
        assert!(parse(&["--bind", "[::]:47100", "--allow-remote"]).is_err());
        assert!(parse(&["--bind", "100.64.0.7:47100"]).is_err());
        assert!(parse(&["--bind", "100.64.0.7:47100", "--allow-remote"]).is_ok());
        assert!(parse(&["--bind", "[::1]:9"]).is_ok());
        assert!(parse(&["--bind", "localhost"]).is_err());
        assert!(parse(&["--scope", "full-access"]).is_err());
        assert_eq!(
            parse(&["--print-service", "macos"]).unwrap().service,
            Some(ServiceAction::Print(ServiceOs::Macos))
        );
        assert_eq!(
            parse(&["--print-service", "--bind", "127.0.0.1:1"])
                .unwrap()
                .service,
            Some(ServiceAction::Print(ServiceOs::current()))
        );
        assert!(parse(&["--print-service", "beos"]).is_err());
    }

    fn spec<'a>(executable: &'a Path, root: &'a Path, home: &'a Path) -> ServiceSpec<'a> {
        ServiceSpec {
            executable,
            bind: default_bind(),
            allow_remote: false,
            scope: Scope::Supervised,
            root,
            home,
        }
    }

    #[test]
    fn renders_a_user_level_systemd_unit() {
        let mut unit = spec(
            Path::new("/opt/ac/agent-command"),
            Path::new("/srv/50%_work"),
            Path::new("/home/me"),
        );
        unit.scope = Scope::ReadOnly;
        let file = render_service(ServiceOs::Linux, &unit).unwrap();
        assert_eq!(
            file.location,
            Some(PathBuf::from(
                "/home/me/.config/systemd/user/agent-command.service"
            ))
        );
        assert!(file.contents.contains(
            "ExecStart=\"/opt/ac/agent-command\" \"daemon\" \"--bind\" \"127.0.0.1:47100\" \"--root\" \"/srv/50%%_work\" \"--scope\" \"read-only\"\n"
        ));
        assert!(file.contents.contains("WantedBy=default.target"));
        assert!(file.contents.contains("NoNewPrivileges=true"));
        assert!(!file.contents.contains("User="));
        assert!(file
            .activate
            .iter()
            .all(|command| !command.starts_with("sudo")));
        assert!(file.activate[1].contains("systemctl --user enable --now"));
    }

    #[test]
    fn renders_a_launchd_agent_plist() {
        let file = render_service(
            ServiceOs::Macos,
            &spec(
                Path::new("/Applications/a&b/agent-command"),
                Path::new("/Users/me/src"),
                Path::new("/Users/me"),
            ),
        )
        .unwrap();
        assert_eq!(
            file.location,
            Some(PathBuf::from(
                "/Users/me/Library/LaunchAgents/com.agent-command.daemon.plist"
            ))
        );
        assert!(file
            .contents
            .contains("<string>/Applications/a&amp;b/agent-command</string>"));
        assert!(file.contents.contains("<string>daemon</string>\n    <string>--bind</string>\n    <string>127.0.0.1:47100</string>"));
        assert!(file
            .contents
            .contains("<string>--scope</string>\n    <string>supervised</string>"));
        assert!(file.contents.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(file.activate[0].starts_with("launchctl bootstrap gui/$(id -u)"));
    }

    #[test]
    fn renders_a_windows_logon_task_without_elevation_and_strips_verbatim_prefix() {
        let mut remote = spec(
            Path::new(r"\\?\C:\Program Files\Agent Command\agent-command.exe"),
            Path::new(r"\\?\C:\work"),
            Path::new(r"C:\Users\me"),
        );
        remote.bind = "100.64.0.7:47100".parse().unwrap();
        remote.allow_remote = true;
        remote.scope = Scope::ReadOnly;
        let file = render_service(ServiceOs::Windows, &remote).unwrap();
        assert_eq!(file.location, None);
        assert_eq!(
            file.contents,
            "schtasks /Create /TN \"AgentCommandDaemon\" /SC ONLOGON /RL LIMITED /F /TR \"\\\"C:\\Program Files\\Agent Command\\agent-command.exe\\\" \\\"daemon\\\" \\\"--bind\\\" \\\"100.64.0.7:47100\\\" \\\"--root\\\" \\\"C:\\work\\\" \\\"--scope\\\" \\\"read-only\\\" \\\"--allow-remote\\\"\"\n"
        );
        assert!(!file.contents.contains(r"\\?\"));
        assert!(!file.contents.contains("HIGHEST"));
        assert_eq!(
            strip_windows_verbatim(r"\\?\UNC\fileserver\share\repo"),
            r"\\fileserver\share\repo"
        );
        assert!(render_service(
            ServiceOs::Windows,
            &spec(
                Path::new("C:\\x\"y.exe"),
                Path::new("C:\\w"),
                Path::new("C:\\")
            )
        )
        .is_err());
    }
}
