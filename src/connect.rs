use crate::{
    auth,
    control::Line,
    daemon::{Token, DEFAULT_PORT},
    http::{self, Response},
};
use serde_json::Value;
use std::{
    io::{self, BufRead, BufReader, Write},
    net::{IpAddr, TcpStream, ToSocketAddrs},
    thread,
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const POLL_EVERY: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const DEFAULT_TOKEN_ENV: &str = "AGENT_COMMAND_TOKEN";
const MAX_HOST_CHARS: usize = 253;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Target {
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Target {
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let rest = url.strip_prefix("http://").ok_or(
            "Daemon URLs use http://HOST:PORT; traffic should ride Tailscale or a trusted LAN.",
        )?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
            return Err(
                "Daemon URL must be http://HOST[:PORT] with no path or credentials.".into(),
            );
        }
        let (host, port) = split_authority(authority)?;
        let valid_host = !host.is_empty()
            && host.len() <= MAX_HOST_CHARS
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c));
        if !valid_host {
            return Err(format!("Invalid daemon host {host}"));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    pub(crate) fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn display(&self) -> String {
        format!("http://{}", self.host_header())
    }
}

fn split_authority(authority: &str) -> Result<(&str, u16), String> {
    let parse_port = |text: &str| {
        text.parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("Invalid port {text}"))
    };
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or("Unclosed IPv6 bracket")?;
        return match tail.strip_prefix(':') {
            Some(port) => Ok((host, parse_port(port)?)),
            None if tail.is_empty() => Ok((host, DEFAULT_PORT)),
            None => Err("Invalid IPv6 authority".into()),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host, parse_port(port)?)),
        Some(_) => Err("Wrap IPv6 hosts in brackets".into()),
        None => Ok((authority, DEFAULT_PORT)),
    }
}

pub(crate) struct Client {
    target: Target,
    token: Token,
}

impl Client {
    pub(crate) fn new(target: Target, token: Token) -> Self {
        Self { target, token }
    }

    fn open(&self, timeout: Duration) -> io::Result<TcpStream> {
        let addresses = (self.target.host.as_str(), self.target.port).to_socket_addrs()?;
        let mut last = io::Error::new(io::ErrorKind::NotFound, "Daemon host did not resolve");
        for address in addresses {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(timeout))?;
                    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
                    return Ok(stream);
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<TcpStream> {
        let mut stream = self.open(timeout)?;
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n",
            self.target.host_header(),
            self.token.secret()
        );
        if let Some(body) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        if let Some(body) = body {
            stream.write_all(body)?;
        }
        stream.flush()?;
        Ok(stream)
    }

    fn exchange(&self, method: &str, path: &str, body: Option<&[u8]>) -> io::Result<Response> {
        let stream = self.send(method, path, body, REQUEST_TIMEOUT)?;
        http::read_response(&mut BufReader::new(stream)).map_err(io::Error::other)
    }

    pub(crate) fn get(&self, path: &str) -> io::Result<Response> {
        self.exchange("GET", path, None)
    }

    pub(crate) fn post_json(&self, path: &str, body: &Value) -> io::Result<Response> {
        self.exchange("POST", path, Some(body.to_string().as_bytes()))
    }

    pub(crate) fn get_json(&self, path: &str) -> io::Result<Value> {
        let response = self.get(path)?;
        match response.status {
            200 if response.is_json() => {
                serde_json::from_slice(&response.body).map_err(io::Error::other)
            }
            401 => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Daemon refused the token (401 Unauthorized)",
            )),
            status => Err(io::Error::other(format!(
                "Daemon answered {status} {}: {}",
                http::reason(status),
                String::from_utf8_lossy(&response.body)
            ))),
        }
    }

    pub(crate) fn health(&self) -> io::Result<Value> {
        let health = self.get_json("/health")?;
        if health["status"] != "ok" || health["name"] != "agent-command" {
            return Err(io::Error::other("Endpoint is not an Agent Command daemon"));
        }
        Ok(health)
    }

    pub(crate) fn stream(
        &self,
        thread: &str,
        since: u64,
        mut on_line: impl FnMut(&Line) -> bool,
    ) -> io::Result<u64> {
        let path = format!("/events?thread={thread}&since={since}");
        let stream = self.send("GET", &path, None, STREAM_IDLE_TIMEOUT)?;
        let mut reader = BufReader::new(stream);
        let (status, _) = http::read_status_and_headers(&mut reader).map_err(io::Error::other)?;
        if status == 401 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Daemon refused the token (401 Unauthorized)",
            ));
        }
        if status != 200 {
            return Err(io::Error::other(format!("Event stream answered {status}")));
        }
        let mut last = since;
        let mut event = String::new();
        let mut text = String::new();
        loop {
            text.clear();
            if reader.read_line(&mut text)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Event stream closed",
                ));
            }
            let line = text.trim_end_matches(['\r', '\n']);
            if let Some(name) = line.strip_prefix("event: ") {
                event = name.to_owned();
            } else if let Some(data) = line.strip_prefix("data: ") {
                if event == "entry" {
                    let entry = parse_line(data)?;
                    last = entry.seq;
                    if !on_line(&entry) {
                        return Ok(last);
                    }
                } else if event == "closed" {
                    return Ok(last);
                }
            }
        }
    }
}

fn parse_line(data: &str) -> io::Result<Line> {
    let value: Value = serde_json::from_str(data).map_err(io::Error::other)?;
    Ok(Line {
        seq: value["seq"].as_u64().unwrap_or_default(),
        at: value["at"].as_str().unwrap_or_default().to_owned(),
        text: value["text"].as_str().unwrap_or_default().to_owned(),
    })
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Options {
    pub(crate) url: String,
    pub(crate) token_env: Option<String>,
    pub(crate) thread: Option<String>,
    pub(crate) once: bool,
}

const USAGE: &str = "Usage: agent-command connect http://HOST:PORT [--token-env VAR] [--thread NAME] [--once]\n\nVerifies the daemon's health and token, then shows the thread status view (or streams one thread with --thread), reconnecting with backoff when the network drops.\nThe token comes from --token-env VAR, else AGENT_COMMAND_TOKEN, else the local token file for loopback daemons.";

pub(crate) fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut url = None;
    let mut token_env = None;
    let mut thread = None;
    let mut once = false;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--token-env" => {
                token_env = Some(
                    words
                        .next()
                        .ok_or("--token-env requires a variable name")?
                        .clone(),
                )
            }
            "--thread" => thread = Some(words.next().ok_or("--thread requires a name")?.clone()),
            "--once" => once = true,
            flag if flag.starts_with("--") => {
                return Err(format!("Unknown connect argument {flag}.\n{USAGE}"))
            }
            value if url.is_none() => url = Some(value.to_owned()),
            value => return Err(format!("Unexpected argument {value}.\n{USAGE}")),
        }
    }
    Ok(Options {
        url: url.ok_or(USAGE)?,
        token_env,
        thread,
        once,
    })
}

pub(crate) fn resolve_token(
    target: &Target,
    token_env: Option<&str>,
    read_env: impl Fn(&str) -> Option<String>,
    local: impl FnOnce() -> io::Result<Option<Token>>,
) -> io::Result<Token> {
    if let Some(variable) = token_env {
        let value = read_env(variable)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("{variable} is not set"))
            })?;
        return Token::parse(&value);
    }
    if let Some(value) = read_env(DEFAULT_TOKEN_ENV).filter(|value| !value.trim().is_empty()) {
        return Token::parse(&value);
    }
    if target.is_loopback() {
        if let Some(token) = local()? {
            return Ok(token);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("No daemon token. Set {DEFAULT_TOKEN_ENV} or pass --token-env VAR."),
    ))
}

pub(crate) fn next_backoff(current: Duration) -> Duration {
    (current * 2).clamp(Duration::from_secs(1), MAX_BACKOFF)
}

fn print_threads(out: &mut impl Write, threads: &Value) -> io::Result<()> {
    let rows = threads["threads"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return writeln!(out, "No threads.");
    }
    for row in rows {
        writeln!(
            out,
            "{}\t{}\t{}\t{}",
            row["name"].as_str().unwrap_or_default(),
            row["role"].as_str().unwrap_or_default(),
            row["status"].as_str().unwrap_or_default(),
            row["mission"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
        )?;
    }
    Ok(())
}

fn watch_threads(client: &Client, out: &mut impl Write) -> io::Result<()> {
    let mut shown = Value::Null;
    loop {
        let threads = client.get_json("/threads")?;
        if threads != shown {
            writeln!(out, "--")?;
            print_threads(out, &threads)?;
            out.flush()?;
            shown = threads;
        }
        thread::sleep(POLL_EVERY);
    }
}

fn reconnecting(mut attempt: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut backoff = Duration::ZERO;
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Err(error),
            Err(error) => {
                backoff = next_backoff(backoff);
                eprintln!(
                    "Connection lost: {error}. Reconnecting in {}s.",
                    backoff.as_secs()
                );
                thread::sleep(backoff);
            }
        }
    }
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    let options = parse_options(args).map_err(invalid)?;
    let target = Target::parse(&options.url).map_err(invalid)?;
    let token = resolve_token(
        &target,
        options.token_env.as_deref(),
        |name| std::env::var(name).ok(),
        || Token::load_existing(&auth::root()?),
    )?;
    let client = Client::new(target.clone(), token);
    let health = client.health()?;
    let mut out = io::stdout();
    writeln!(
        out,
        "Connected to agent-command {} at {} · scope {} · {} threads",
        health["version"].as_str().unwrap_or("?"),
        target.display(),
        health["scope"].as_str().unwrap_or("?"),
        health["threads"]
    )?;
    if options.once {
        return print_threads(&mut out, &client.get_json("/threads")?);
    }
    match options.thread {
        Some(name) => {
            let mut since = 0;
            reconnecting(|| {
                since = client.stream(&name, since, |line| {
                    let _ = writeln!(io::stdout(), "{}  {}", line.at, line.text);
                    true
                })?;
                Ok(())
            })
        }
        None => reconnecting(|| watch_threads(&client, &mut io::stdout())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::start;

    #[test]
    fn parses_daemon_urls_strictly() {
        assert_eq!(
            Target::parse("http://100.64.0.7:47100/").unwrap(),
            Target {
                host: "100.64.0.7".into(),
                port: 47100
            }
        );
        assert_eq!(
            Target::parse("http://box.tail1234.ts.net").unwrap().port,
            DEFAULT_PORT
        );
        let v6 = Target::parse("http://[::1]:9").unwrap();
        assert_eq!(v6.host, "::1");
        assert!(v6.is_loopback());
        assert_eq!(v6.host_header(), "[::1]:9");
        assert!(Target::parse("http://localhost:1").unwrap().is_loopback());
        assert!(!Target::parse("http://100.64.0.7:1").unwrap().is_loopback());
        for bad in [
            "https://x:1",
            "x:1",
            "http://",
            "http://a:0",
            "http://a:99999",
            "http://u:p@a:1",
            "http://a:1/path",
            "http://::1:9",
            "http://a b:1",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn token_comes_from_named_env_default_env_or_local_file() {
        let secret = Token::generate();
        let text = secret.secret().to_owned();
        let remote = Target::parse("http://100.64.0.7:1").unwrap();
        let local = Target::parse("http://127.0.0.1:1").unwrap();
        let named = resolve_token(
            &remote,
            Some("MY_TOKEN"),
            |name| (name == "MY_TOKEN").then(|| text.clone()),
            || Ok(None),
        )
        .unwrap();
        assert!(named.matches(&text));
        assert!(resolve_token(
            &remote,
            Some("MY_TOKEN"),
            |_| None,
            || Ok(Some(Token::generate()))
        )
        .is_err());
        let default = resolve_token(
            &remote,
            None,
            |name| (name == DEFAULT_TOKEN_ENV).then(|| text.clone()),
            || Ok(None),
        )
        .unwrap();
        assert!(default.matches(&text));
        let file = resolve_token(&local, None, |_| None, || Ok(Some(secret.clone()))).unwrap();
        assert!(file.matches(&text));
        assert!(resolve_token(&remote, None, |_| None, || Ok(Some(secret.clone()))).is_err());
        assert!(
            resolve_token(&remote, Some("BAD"), |_| Some("short".into()), || Ok(None)).is_err()
        );
    }

    #[test]
    fn parses_connect_options_and_backs_off_to_a_cap() {
        let words = |list: &[&str]| list.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            parse_options(&words(&[
                "http://h:1",
                "--token-env",
                "T",
                "--thread",
                "nova",
                "--once"
            ]))
            .unwrap(),
            Options {
                url: "http://h:1".into(),
                token_env: Some("T".into()),
                thread: Some("nova".into()),
                once: true
            }
        );
        assert!(parse_options(&[]).is_err());
        assert!(parse_options(&words(&["http://h:1", "extra"])).is_err());
        assert!(parse_options(&words(&["http://h:1", "--token"])).is_err());
        assert_eq!(next_backoff(Duration::ZERO), Duration::from_secs(1));
        assert_eq!(next_backoff(Duration::from_secs(4)), Duration::from_secs(8));
        assert_eq!(next_backoff(Duration::from_secs(20)), MAX_BACKOFF);
    }

    #[test]
    fn health_check_distinguishes_good_and_bad_tokens() {
        let running = start("connect-health", false);
        let target = Target::parse(&format!("http://{}", running.address)).unwrap();
        let good = Client::new(target.clone(), running.token.clone());
        assert_eq!(good.health().unwrap()["status"], "ok");
        let mut out = Vec::new();
        print_threads(&mut out, &good.get_json("/threads").unwrap()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No threads.\n");
        let bad = Client::new(target, Token::generate());
        assert_eq!(
            bad.health().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let mut attempts = 0;
        let refused = reconnecting(|| {
            attempts += 1;
            bad.health().map(|_| ())
        });
        assert!(refused.is_err());
        assert_eq!(attempts, 1);
    }
}
