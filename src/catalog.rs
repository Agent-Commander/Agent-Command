use crate::{auth::Provider, native::Benchmark};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) const SCHEMA: &str = "agent-command-catalog-v2";
pub(crate) const ORIGIN: &str = "https://artificialanalysis.ai";
pub(crate) const KEY_VARIABLE: &str = "ARTIFICIAL_ANALYSIS_API_KEY";
const CACHE_DIRECTORY: &str = "catalog";
const MAX_CACHE_BYTES: u64 = 1 << 20;
const MAX_RESPONSE_BYTES: usize = 4 << 20;
const MAX_ROWS: usize = 2_000;
const MAX_PAGES: u64 = 20;
const MAX_VERSION_LENGTH: usize = 64;
const MAX_SLUG_LENGTH: usize = 96;
const FETCH_SECONDS: &str = "30";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Endpoint {
    #[serde(rename = "models/free")]
    ModelsFree,
    #[serde(rename = "agents/coding")]
    AgentsCoding,
}

impl Endpoint {
    pub(crate) const ALL: [Self; 2] = [Self::AgentsCoding, Self::ModelsFree];

    pub(crate) fn scope(self) -> &'static str {
        match self {
            Self::ModelsFree => "models/free",
            Self::AgentsCoding => "agents/coding",
        }
    }

    pub(crate) fn required(self) -> bool {
        match self {
            Self::ModelsFree => true,
            Self::AgentsCoding => false,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::ModelsFree => "/api/v2/language/models/free",
            Self::AgentsCoding => "/api/v2/language/agents/coding",
        }
    }

    pub(crate) fn source(self) -> String {
        format!("{ORIGIN}{}", self.path())
    }

    fn cache_file(self) -> &'static str {
        match self {
            Self::ModelsFree => "models-free.json",
            Self::AgentsCoding => "agents-coding.json",
        }
    }

    pub(crate) fn speed_unit(self) -> SpeedUnit {
        match self {
            Self::ModelsFree => SpeedUnit::TokensPerSecond,
            Self::AgentsCoding => SpeedUnit::SecondsPerTask,
        }
    }

    fn bundled(self) -> &'static str {
        match self {
            Self::ModelsFree => include_str!("catalog/bundled-models-free.json"),
            Self::AgentsCoding => include_str!("catalog/bundled-agents-coding.json"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Metric {
    CodingAgent,
    Coding,
    Intelligence,
}

impl Metric {
    pub(crate) const PREFERENCE: [Self; 3] = [Self::CodingAgent, Self::Coding, Self::Intelligence];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::CodingAgent => "AA coding agent index",
            Self::Coding => "AA coding index",
            Self::Intelligence => "AA intelligence index",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SpeedUnit {
    SecondsPerTask,
    TokensPerSecond,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Speed {
    pub(crate) unit: SpeedUnit,
    pub(crate) value: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Row {
    pub(crate) provider: String,
    pub(crate) slug: String,
    pub(crate) effort: Option<String>,
    pub(crate) coding_index: Option<f64>,
    pub(crate) intelligence_index: Option<f64>,
    pub(crate) speed: Option<Speed>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub(crate) schema: String,
    pub(crate) scope: Endpoint,
    pub(crate) source: String,
    pub(crate) version: String,
    pub(crate) provenance: String,
    pub(crate) sha256: String,
    pub(crate) rows: Vec<Row>,
}

pub(crate) fn rows_digest(rows: &[Row]) -> String {
    let bytes = serde_json::to_vec(rows).expect("catalog rows serialize");
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Snapshot {
    pub(crate) fn sealed(
        scope: Endpoint,
        version: String,
        provenance: String,
        rows: Vec<Row>,
    ) -> Self {
        Self {
            schema: SCHEMA.into(),
            scope,
            source: scope.source(),
            version,
            provenance,
            sha256: rows_digest(&rows),
            rows,
        }
    }

    pub(crate) fn validate(&self, expected: Endpoint) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!("schema {} is not {SCHEMA}", self.schema));
        }
        if self.scope != expected {
            return Err(format!(
                "scope {} does not match {}",
                self.scope.scope(),
                expected.scope()
            ));
        }
        if self.source != expected.source() {
            return Err("source is not the published endpoint".into());
        }
        if !valid_version(&self.version) {
            return Err("version is missing or malformed".into());
        }
        if self.rows.len() > MAX_ROWS {
            return Err(format!("more than {MAX_ROWS} rows"));
        }
        if rows_digest(&self.rows) != self.sha256 {
            return Err("SHA-256 digest mismatch".into());
        }
        let mut keys = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            validate_row(row, expected)?;
            let key = (&row.provider, &row.slug, &row.effort);
            if keys.contains(&key) {
                return Err(format!("duplicate row {}/{}", row.provider, row.slug));
            }
            keys.push(key);
        }
        Ok(())
    }
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_LENGTH
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_SLUG_LENGTH
        && slug.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte)
        })
}

fn index_in_range(index: Option<f64>) -> bool {
    index.is_none_or(|index| index.is_finite() && (0.0..=100.0).contains(&index))
}

fn validate_row(row: &Row, endpoint: Endpoint) -> Result<(), String> {
    Provider::named(&row.provider).ok_or_else(|| format!("unknown provider {}", row.provider))?;
    if !valid_slug(&row.slug) || !row.effort.as_deref().is_none_or(valid_slug) {
        return Err(format!("malformed identity {}", row.slug));
    }
    if endpoint == Endpoint::ModelsFree && row.effort.is_some() {
        return Err(format!("models/free row {} carries an effort", row.slug));
    }
    if endpoint == Endpoint::AgentsCoding && row.intelligence_index.is_some() {
        return Err(format!(
            "agents/coding row {} carries an intelligence index",
            row.slug
        ));
    }
    if !index_in_range(row.coding_index) {
        return Err(format!("coding index out of range for {}", row.slug));
    }
    if !index_in_range(row.intelligence_index) {
        return Err(format!("intelligence index out of range for {}", row.slug));
    }
    if let Some(speed) = row.speed {
        if speed.unit != endpoint.speed_unit() {
            return Err(format!("speed unit mismatch for {}", row.slug));
        }
        if !speed.value.is_finite() || speed.value <= 0.0 {
            return Err(format!("speed out of range for {}", row.slug));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Origin {
    Cache,
    Bundled(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Source {
    pub(crate) endpoint: Endpoint,
    pub(crate) origin: Origin,
    pub(crate) snapshot: Snapshot,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Catalog {
    pub(crate) sources: Vec<Source>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Metrics {
    pub(crate) coding_agent_index: Option<f64>,
    pub(crate) coding_index: Option<f64>,
    pub(crate) intelligence_index: Option<f64>,
    pub(crate) speed: Option<Speed>,
}

impl Metrics {
    pub(crate) fn value(&self, metric: Metric) -> Option<f64> {
        match metric {
            Metric::CodingAgent => self.coding_agent_index,
            Metric::Coding => self.coding_index,
            Metric::Intelligence => self.intelligence_index,
        }
    }
}

pub(crate) fn cache_directory(state: &Path) -> PathBuf {
    state.join(CACHE_DIRECTORY)
}

pub(crate) fn bundled(endpoint: Endpoint) -> Result<Snapshot, String> {
    let snapshot: Snapshot =
        serde_json::from_str(endpoint.bundled()).map_err(|error| error.to_string())?;
    snapshot.validate(endpoint)?;
    Ok(snapshot)
}

fn read_cache(state: &Path, endpoint: Endpoint) -> Result<Snapshot, String> {
    let path = cache_directory(state).join(endpoint.cache_file());
    let file = File::open(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => "cache missing".to_owned(),
        _ => format!("cache unreadable: {error}"),
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cache unreadable: {error}"))?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err("cache exceeds the size bound".into());
    }
    reopen(&bytes, endpoint)
}

fn reopen(bytes: &[u8], endpoint: Endpoint) -> Result<Snapshot, String> {
    let snapshot: Snapshot =
        serde_json::from_slice(bytes).map_err(|error| format!("cache invalid: {error}"))?;
    snapshot
        .validate(endpoint)
        .map_err(|error| format!("cache invalid: {error}"))?;
    Ok(snapshot)
}

fn load_endpoint(state: Option<&Path>, endpoint: Endpoint) -> Source {
    let cached = state
        .ok_or_else(|| "no state directory".to_owned())
        .and_then(|state| read_cache(state, endpoint));
    match cached {
        Ok(snapshot) => Source {
            endpoint,
            origin: Origin::Cache,
            snapshot,
        },
        Err(reason) => Source {
            endpoint,
            origin: Origin::Bundled(reason),
            snapshot: bundled(endpoint).unwrap_or_else(|error| {
                Snapshot::sealed(endpoint, "bundled-invalid".into(), error, Vec::new())
            }),
        },
    }
}

impl Catalog {
    pub(crate) fn load(state: Option<&Path>) -> Self {
        Self {
            sources: Endpoint::ALL
                .into_iter()
                .map(|endpoint| load_endpoint(state, endpoint))
                .collect(),
        }
    }

    fn exact_row(
        &self,
        endpoint: Endpoint,
        provider: Provider,
        slug: &str,
        effort: Option<&str>,
    ) -> Option<&Row> {
        self.sources
            .iter()
            .filter(|source| source.endpoint == endpoint)
            .flat_map(|source| source.snapshot.rows.iter())
            .find(|row| {
                row.provider == provider.binary()
                    && row.slug == slug
                    && row.effort.as_deref() == effort
            })
    }

    pub(crate) fn metrics(&self, benchmark: &Benchmark) -> Metrics {
        let agent = self.exact_row(
            Endpoint::AgentsCoding,
            benchmark.provider,
            &benchmark.slug,
            benchmark.effort,
        );
        let model = self.exact_row(
            Endpoint::ModelsFree,
            benchmark.provider,
            &benchmark.models_free_slug(),
            None,
        );
        Metrics {
            coding_agent_index: agent.and_then(|row| row.coding_index),
            coding_index: model.and_then(|row| row.coding_index),
            intelligence_index: model.and_then(|row| row.intelligence_index),
            speed: agent
                .and_then(|row| row.speed)
                .or_else(|| model.and_then(|row| row.speed)),
        }
    }

    pub(crate) fn describe(&self) -> Vec<String> {
        self.sources
            .iter()
            .map(|source| {
                let origin = match &source.origin {
                    Origin::Cache => "verified cache".to_owned(),
                    Origin::Bundled(reason) => format!("bundled snapshot ({reason})"),
                };
                let optional = if source.endpoint.required() {
                    ""
                } else {
                    " (optional)"
                };
                format!(
                    "{}{optional} · {origin} · version {} · {} rows · {}",
                    source.endpoint.scope(),
                    source.snapshot.version,
                    source.snapshot.rows.len(),
                    source.snapshot.provenance
                )
            })
            .collect()
    }
}

pub(crate) struct ApiKey(String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(redacted)")
    }
}

impl ApiKey {
    pub(crate) fn new(value: String) -> Result<Self, String> {
        if value.is_empty()
            || value.len() > 256
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(format!("{KEY_VARIABLE} has an unsupported format"));
        }
        Ok(Self(value))
    }

    pub(crate) fn from_environment() -> Option<Result<Self, String>> {
        std::env::var(KEY_VARIABLE).ok().map(Self::new)
    }

    fn redact(&self, text: &str) -> String {
        text.replace(&self.0, "[redacted]")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) trait Fetcher {
    fn fetch(&self, url: &str, key: &ApiKey) -> io::Result<Response>;
}

pub(crate) struct CurlFetcher;

const STATUS_MARKER: &[u8] = b"\nagent-command-http-status:";
const CURL_ENVIRONMENT_DENYLIST: [&str; 2] = [KEY_VARIABLE, "CURL_HOME"];

fn curl_command() -> Command {
    let mut command = Command::new("curl");
    command
        .args([
            "-q",
            "--config",
            "-",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "0",
            "--silent",
            "--show-error",
            "--max-time",
            FETCH_SECONDS,
            "--max-filesize",
            &MAX_RESPONSE_BYTES.to_string(),
            "--write-out",
            "\nagent-command-http-status:%{http_code}",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for variable in CURL_ENVIRONMENT_DENYLIST {
        command.env_remove(variable);
    }
    command
}

fn curl_config(url: &str, key: &ApiKey) -> String {
    format!("url = \"{url}\"\nheader = \"x-api-key: {}\"\n", key.0)
}

fn split_status(mut output: Vec<u8>) -> io::Result<Response> {
    let position = output
        .windows(STATUS_MARKER.len())
        .rposition(|window| window == STATUS_MARKER)
        .ok_or_else(|| io::Error::other("response carried no HTTP status"))?;
    let status = std::str::from_utf8(&output[position + STATUS_MARKER.len()..])
        .ok()
        .and_then(|code| code.trim().parse::<u16>().ok())
        .filter(|code| (100..=599).contains(code))
        .ok_or_else(|| io::Error::other("response carried a malformed HTTP status"))?;
    output.truncate(position);
    Ok(Response {
        status,
        body: output,
    })
}

impl Fetcher for CurlFetcher {
    fn fetch(&self, url: &str, key: &ApiKey) -> io::Result<Response> {
        require_same_origin(url)?;
        let mut child = curl_command().spawn()?;
        let mut input = child.stdin.take().expect("piped curl input");
        input.write_all(curl_config(url, key).as_bytes())?;
        drop(input);
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("piped curl output")
            .take((MAX_RESPONSE_BYTES + STATUS_MARKER.len() + 8) as u64)
            .read_to_end(&mut output)?;
        let status = child.wait()?;
        if !status.success() {
            return Err(io::Error::other(format!("request failed with {status}")));
        }
        split_status(output)
    }
}

fn check_status(status: u16) -> Result<(), String> {
    match status {
        200 => Ok(()),
        401 | 403 => Err(format!(
            "access refused for this key or plan tier (HTTP {status}); previous data kept"
        )),
        429 => Err("rate limited (HTTP 429); previous data kept".into()),
        _ => Err(format!("unexpected HTTP {status}; previous data kept")),
    }
}

fn require_same_origin(url: &str) -> io::Result<()> {
    let allowed = Endpoint::ALL.into_iter().any(|endpoint| {
        url == endpoint.source() || url.starts_with(&format!("{}?page=", endpoint.source()))
    });
    if !allowed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "catalog requests must stay on the published HTTPS origin",
        ));
    }
    Ok(())
}

fn page_url(endpoint: Endpoint, page: u64) -> String {
    match page {
        1 => endpoint.source(),
        _ => format!("{}?page={page}", endpoint.source()),
    }
}

struct Harness {
    provider: Provider,
    creator: &'static str,
    agent: &'static str,
}

const HARNESSES: [Harness; 2] = [
    Harness {
        provider: Provider::Codex,
        creator: "OpenAI",
        agent: "Codex",
    },
    Harness {
        provider: Provider::Claude,
        creator: "Anthropic",
        agent: "Claude Code",
    },
];

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Identity {
    pub(crate) provider: Provider,
    pub(crate) slug: String,
    pub(crate) effort: Option<String>,
}

fn identify_model(raw: &Value) -> Option<Identity> {
    let slug = raw["slug"].as_str().filter(|slug| valid_slug(slug))?;
    let creator = raw["model_creator"]["name"].as_str()?;
    let harness = HARNESSES
        .iter()
        .find(|harness| harness.creator == creator)?;
    Some(Identity {
        provider: harness.provider,
        slug: slug.into(),
        effort: None,
    })
}

fn agent_creator_matches(raw: &Value, expected: &str) -> bool {
    match &raw["agent_creator"]["name"] {
        Value::Null => true,
        Value::String(name) => name == expected,
        _ => false,
    }
}

fn pinned_model_slug(model: &Value) -> Option<&str> {
    let slug = model["slug"].as_str().filter(|slug| valid_slug(slug))?;
    match &model["release_slug"] {
        Value::Null => Some(slug),
        Value::String(release) if release == slug => Some(slug),
        _ => None,
    }
}

fn agent_effort(model: &Value) -> Option<Option<String>> {
    match &model["effort"] {
        Value::Null => Some(None),
        Value::String(effort) if valid_slug(effort) => Some(Some(effort.clone())),
        _ => None,
    }
}

fn identify_agent(raw: &Value) -> Option<Identity> {
    if raw["is_router"] != Value::Bool(false) {
        return None;
    }
    let agent = raw["agent"]["name"].as_str()?;
    let provider = raw["provider"].as_str()?;
    let harness = HARNESSES.iter().find(|harness| {
        harness.agent == agent
            && harness.creator == provider
            && agent_creator_matches(raw, harness.creator)
    })?;
    Some(Identity {
        provider: harness.provider,
        slug: pinned_model_slug(&raw["model"])?.into(),
        effort: agent_effort(&raw["model"])?,
    })
}

struct Page {
    data: Vec<Value>,
    total_pages: u64,
    tier: String,
    version: String,
}

fn version_field(endpoint: Endpoint) -> &'static str {
    match endpoint {
        Endpoint::ModelsFree => "intelligence_index_version",
        Endpoint::AgentsCoding => "coding_agent_index_version",
    }
}

fn parse_version(endpoint: Endpoint, value: &Value) -> Result<String, String> {
    let field = version_field(endpoint);
    let version = match (endpoint, &value[field]) {
        (Endpoint::ModelsFree, Value::Number(number)) => number.to_string(),
        (Endpoint::AgentsCoding, Value::String(text)) => text.clone(),
        _ => return Err(format!("{field} is missing or has the wrong type")),
    };
    if !valid_version(&version) {
        return Err(format!("{field} is malformed"));
    }
    Ok(version)
}

fn parse_pagination(value: &Value, requested: u64, rows: usize) -> Result<u64, String> {
    let pagination = value["pagination"]
        .as_object()
        .ok_or("response has no pagination object")?;
    if pagination.get("page").and_then(Value::as_u64) != Some(requested) {
        return Err(format!("expected page {requested}"));
    }
    if !pagination
        .get("page_size")
        .and_then(Value::as_u64)
        .is_some_and(|size| size >= 1 && rows as u64 <= size)
    {
        return Err("pagination page_size is missing or smaller than the page".into());
    }
    let total = pagination
        .get("total_pages")
        .and_then(Value::as_u64)
        .filter(|total| *total <= MAX_PAGES)
        .ok_or("pagination total is out of bounds")?;
    let has_more = pagination
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or("pagination has_more is missing")?;
    if total == 0 && (requested != 1 || rows != 0) {
        return Err("pagination total is out of bounds".into());
    }
    if has_more != (requested < total) {
        return Err("pagination has_more contradicts total_pages".into());
    }
    Ok(total.max(1))
}

fn parse_page(endpoint: Endpoint, bytes: &[u8], requested: u64) -> Result<Page, String> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("response exceeds the size bound".into());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "response is not JSON")?;
    let data = value["data"]
        .as_array()
        .ok_or("response has no data array")?
        .clone();
    if !data.iter().all(Value::is_object) {
        return Err("response data holds a non-object row".into());
    }
    let tier = value["tier"]
        .as_str()
        .filter(|tier| valid_version(tier))
        .ok_or("response tier is missing or malformed")?
        .to_owned();
    Ok(Page {
        total_pages: parse_pagination(&value, requested, data.len())?,
        version: parse_version(endpoint, &value)?,
        tier,
        data,
    })
}

struct Fetched {
    rows: Vec<Value>,
    version: String,
}

fn fetch_all(endpoint: Endpoint, key: &ApiKey, fetcher: &dyn Fetcher) -> Result<Fetched, String> {
    let mut rows = Vec::new();
    let mut first: Option<Page> = None;
    let mut page = 1;
    loop {
        let url = page_url(endpoint, page);
        require_same_origin(&url).map_err(|error| error.to_string())?;
        let response = fetcher
            .fetch(&url, key)
            .map_err(|error| error.to_string())?;
        check_status(response.status)?;
        let mut parsed = parse_page(endpoint, &response.body, page)?;
        if let Some(first) = &first {
            if parsed.total_pages != first.total_pages {
                return Err("pagination total changed between pages".into());
            }
            if parsed.version != first.version {
                return Err(format!("{} changed between pages", version_field(endpoint)));
            }
            if parsed.tier != first.tier {
                return Err("tier changed between pages".into());
            }
        }
        rows.append(&mut parsed.data);
        if rows.len() > MAX_ROWS {
            return Err(format!("more than {MAX_ROWS} rows"));
        }
        let first = first.get_or_insert(parsed);
        if page == first.total_pages {
            return Ok(Fetched {
                rows,
                version: first.version.clone(),
            });
        }
        page += 1;
    }
}

fn number(value: &Value, field: &str) -> Result<Option<f64>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Number(number) => Ok(number.as_f64()),
        _ => Err(format!("{field} is not numeric")),
    }
}

fn model_speed(raw: &Value) -> Result<Option<f64>, String> {
    match &raw["performance_v3"] {
        Value::Null => Ok(None),
        Value::Object(performance) => number(
            performance
                .get("mean_output_tokens_per_second")
                .unwrap_or(&Value::Null),
            "performance_v3.mean_output_tokens_per_second",
        ),
        _ => Err("performance_v3 is not an object".into()),
    }
}

struct Measured {
    coding_index: Option<f64>,
    intelligence_index: Option<f64>,
    speed: Option<f64>,
}

fn measured_metrics(endpoint: Endpoint, raw: &Value) -> Result<Measured, String> {
    match endpoint {
        Endpoint::ModelsFree => Ok(Measured {
            coding_index: number(
                &raw["evaluations"]["artificial_analysis_coding_index"],
                "evaluations.artificial_analysis_coding_index",
            )?,
            intelligence_index: number(
                &raw["evaluations"]["artificial_analysis_intelligence_index"],
                "evaluations.artificial_analysis_intelligence_index",
            )?,
            speed: model_speed(raw)?,
        }),
        Endpoint::AgentsCoding => Ok(Measured {
            coding_index: number(&raw["coding_agent_index"], "coding_agent_index")?,
            intelligence_index: None,
            speed: number(&raw["time_per_task_seconds"], "time_per_task_seconds")?,
        }),
    }
}

fn identify(endpoint: Endpoint, raw: &Value) -> Option<Identity> {
    match endpoint {
        Endpoint::ModelsFree => identify_model(raw),
        Endpoint::AgentsCoding => identify_agent(raw),
    }
}

fn normalize(endpoint: Endpoint, raw: &Value) -> Result<Option<Row>, String> {
    let Some(identity) = identify(endpoint, raw) else {
        return Ok(None);
    };
    let measured = measured_metrics(endpoint, raw)?;
    Ok(Some(Row {
        provider: identity.provider.binary().into(),
        slug: identity.slug,
        effort: identity.effort,
        coding_index: measured.coding_index,
        intelligence_index: measured.intelligence_index,
        speed: measured.speed.map(|value| Speed {
            unit: endpoint.speed_unit(),
            value,
        }),
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Refreshed {
    pub(crate) published: usize,
    pub(crate) skipped: usize,
}

fn same_identity(left: &Row, right: &Row) -> bool {
    left.provider == right.provider && left.slug == right.slug && left.effort == right.effort
}

fn without_ambiguous_identities(rows: Vec<Row>) -> Vec<Row> {
    rows.iter()
        .filter(|row| {
            rows.iter()
                .filter(|other| same_identity(row, other))
                .count()
                == 1
        })
        .cloned()
        .collect()
}

fn build_snapshot(
    endpoint: Endpoint,
    fetched: Fetched,
    retrieved: &str,
) -> Result<Snapshot, String> {
    let mut rows = Vec::new();
    for raw in &fetched.rows {
        if let Some(row) = normalize(endpoint, raw)? {
            rows.push(row);
        }
    }
    let rows = without_ambiguous_identities(rows);
    if rows.is_empty() {
        return Err(format!(
            "no supported rows among {} published rows; previous data kept",
            fetched.rows.len()
        ));
    }
    let snapshot = Snapshot::sealed(
        endpoint,
        fetched.version,
        format!(
            "Artificial Analysis {} retrieved {retrieved}",
            endpoint.scope()
        ),
        rows,
    );
    snapshot.validate(endpoint)?;
    Ok(snapshot)
}

fn refresh_endpoint(
    state: &Path,
    endpoint: Endpoint,
    key: &ApiKey,
    fetcher: &dyn Fetcher,
    retrieved: &str,
) -> Result<Refreshed, String> {
    let fetched = fetch_all(endpoint, key, fetcher)?;
    let total = fetched.rows.len();
    let snapshot = build_snapshot(endpoint, fetched, retrieved)?;
    publish(state, &snapshot).map_err(|error| error.to_string())?;
    Ok(Refreshed {
        published: snapshot.rows.len(),
        skipped: total - snapshot.rows.len(),
    })
}

pub(crate) fn publish(state: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let directory = cache_directory(state);
    match fs::create_dir(&directory) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let target = directory.join(snapshot.scope.cache_file());
    let temporary = target.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let mut file = File::create_new(&temporary)?;
    serde_json::to_writer(&mut file, snapshot)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, target)
}

pub(crate) fn refresh(
    state: &Path,
    key: &ApiKey,
    fetcher: &dyn Fetcher,
    retrieved: &str,
) -> Vec<(Endpoint, Result<Refreshed, String>)> {
    Endpoint::ALL
        .into_iter()
        .map(|endpoint| {
            let outcome = refresh_endpoint(state, endpoint, key, fetcher, retrieved)
                .map_err(|error| key.redact(&error));
            (endpoint, outcome)
        })
        .collect()
}

pub(crate) fn refresh_failed(outcomes: &[(Endpoint, Result<Refreshed, String>)]) -> bool {
    outcomes
        .iter()
        .any(|(endpoint, outcome)| endpoint.required() && outcome.is_err())
}

fn outcome_line(endpoint: Endpoint, outcome: &Result<Refreshed, String>) -> String {
    match (outcome, endpoint.required()) {
        (Ok(refreshed), _) => format!(
            "{} refreshed · {} rows · {} unsupported rows skipped",
            endpoint.scope(),
            refreshed.published,
            refreshed.skipped
        ),
        (Err(error), true) => format!("{} kept previous data · {error}", endpoint.scope()),
        (Err(error), false) => format!(
            "{} optional endpoint kept previous data · {error}",
            endpoint.scope()
        ),
    }
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    let state = crate::routing::state_directory(&crate::auth::root()?)?;
    match args {
        [action] if action == "show" => {
            for line in Catalog::load(Some(&state)).describe() {
                println!("{line}");
            }
            Ok(())
        }
        [action] if action == "refresh" => {
            let key = ApiKey::from_environment()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Set {KEY_VARIABLE} to refresh. Bundled snapshots stay in use."),
                    )
                })?
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let retrieved = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let outcomes = refresh(&state, &key, &CurlFetcher, &retrieved);
            for (endpoint, outcome) in &outcomes {
                println!("{}", outcome_line(*endpoint, outcome));
            }
            if refresh_failed(&outcomes) {
                std::process::exit(1);
            }
            Ok(())
        }
        _ => {
            println!("agent-command catalog {{show|refresh}}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native;
    use serde_json::json;
    use std::{cell::RefCell, collections::HashMap};

    const SYNTHETIC_KEY: &str = "synthetic-key-0123";
    const MODEL_PAGES: u64 = 4;
    const AGENT_PAGES: u64 = 2;

    fn state(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("ac-catalog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(cache_directory(&path)).unwrap();
        path
    }

    fn agent_row(provider: &str, slug: &str, effort: Option<&str>, index: f64) -> Row {
        Row {
            provider: provider.into(),
            slug: slug.into(),
            effort: effort.map(str::to_owned),
            coding_index: Some(index),
            intelligence_index: None,
            speed: Some(Speed {
                unit: SpeedUnit::SecondsPerTask,
                value: 120.0,
            }),
        }
    }

    fn write(state: &Path, endpoint: Endpoint, snapshot: &Snapshot) {
        fs::write(
            cache_directory(state).join(endpoint.cache_file()),
            serde_json::to_vec(snapshot).unwrap(),
        )
        .unwrap();
    }

    fn lookup(catalog: &Catalog, provider: Provider, model: &str, effort: Option<&str>) -> Metrics {
        native::benchmark(provider, Some(model), effort)
            .map(|benchmark| catalog.metrics(&benchmark))
            .unwrap_or_default()
    }

    fn opus_high(catalog: &Catalog) -> Metrics {
        lookup(catalog, Provider::Claude, "claude-opus-5-5", Some("high"))
    }

    type Mutation = Box<dyn Fn(&mut Snapshot)>;

    fn agents_snapshot() -> Snapshot {
        Snapshot::sealed(
            Endpoint::AgentsCoding,
            "synthetic-1".into(),
            "synthetic test fixture".into(),
            vec![agent_row("claude", "claude-opus-5-5", Some("high"), 61.0)],
        )
    }

    #[test]
    fn bundled_snapshots_are_dated_sourced_and_carry_no_unverified_values() {
        for endpoint in Endpoint::ALL {
            let snapshot = bundled(endpoint).unwrap();
            assert_eq!(snapshot.source, endpoint.source());
            assert!(snapshot.source.starts_with("https://"));
            assert_eq!(snapshot.version, "bundled-2026-11-23");
            assert!(snapshot.provenance.starts_with("Dated 2026-11-23."));
            assert!(snapshot.rows.is_empty());
        }
        let catalog = Catalog::load(None);
        assert_eq!(opus_high(&catalog), Metrics::default());
        assert!(catalog
            .describe()
            .iter()
            .all(|line| line.contains("bundled snapshot (no state directory)")));
        assert!(catalog.describe()[0].starts_with("agents/coding (optional) ·"));
    }

    #[test]
    fn cache_integrity_falls_back_independently_per_endpoint() {
        let state = state("integrity");
        write(&state, Endpoint::AgentsCoding, &agents_snapshot());
        let mut tampered = agents_snapshot();
        tampered.scope = Endpoint::ModelsFree;
        tampered.source = Endpoint::ModelsFree.source();
        tampered.rows[0].effort = None;
        tampered.rows[0].speed = Some(Speed {
            unit: SpeedUnit::TokensPerSecond,
            value: 80.0,
        });
        write(&state, Endpoint::ModelsFree, &tampered);
        let catalog = Catalog::load(Some(&state));
        assert_eq!(catalog.sources[0].origin, Origin::Cache);
        assert_eq!(
            catalog.sources[1].origin,
            Origin::Bundled("cache invalid: SHA-256 digest mismatch".into())
        );
        assert_eq!(opus_high(&catalog).coding_agent_index, Some(61.0));
        let cases: [(&str, Mutation); 9] = [
            ("scope", Box::new(|s| s.scope = Endpoint::ModelsFree)),
            (
                "schema",
                Box::new(|s| s.schema = "agent-command-catalog-v1".into()),
            ),
            ("version", Box::new(|s| s.version = "bad version".into())),
            ("index", Box::new(|s| s.rows[0].coding_index = Some(101.0))),
            (
                "intelligence",
                Box::new(|s| s.rows[0].intelligence_index = Some(50.0)),
            ),
            (
                "unit",
                Box::new(|s| {
                    s.rows[0].speed = Some(Speed {
                        unit: SpeedUnit::TokensPerSecond,
                        value: 1.0,
                    })
                }),
            ),
            ("provider", Box::new(|s| s.rows[0].provider = "api".into())),
            ("slug", Box::new(|s| s.rows[0].slug = "Claude Opus".into())),
            ("duplicate", Box::new(|s| s.rows.push(s.rows[0].clone()))),
        ];
        for (case, mutate) in cases {
            let mut snapshot = agents_snapshot();
            mutate(&mut snapshot);
            snapshot.sha256 = rows_digest(&snapshot.rows);
            write(&state, Endpoint::AgentsCoding, &snapshot);
            let catalog = Catalog::load(Some(&state));
            assert!(
                matches!(&catalog.sources[0].origin, Origin::Bundled(reason) if reason.starts_with("cache invalid")),
                "{case}"
            );
            assert_eq!(opus_high(&catalog), Metrics::default(), "{case}");
        }
        fs::write(
            cache_directory(&state).join(Endpoint::AgentsCoding.cache_file()),
            vec![b' '; MAX_CACHE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            Catalog::load(Some(&state)).sources[0].origin,
            Origin::Bundled("cache exceeds the size bound".into())
        );
        fs::remove_dir_all(cache_directory(&state)).unwrap();
        assert_eq!(
            Catalog::load(Some(&state)).sources[1].origin,
            Origin::Bundled("cache missing".into())
        );
        fs::remove_dir_all(state).unwrap();
    }

    fn decimal_sample(index: u64) -> String {
        let whole = 1 + index * 37 % 480;
        match index % 3 {
            0 => format!("{whole}.{:03}", index * 7_919 % 1_000),
            1 => format!("{whole}.{:09}", index * 104_729_003 % 1_000_000_000),
            _ => format!(
                "{whole}.{:015}",
                index * 7_919_000_000_007 % 1_000_000_000_000_000
            ),
        }
    }

    fn decimal_page(text: &str) -> Vec<u8> {
        format!(
            r#"{{"tier":"free","intelligence_index_version":4,"pagination":{{"page":1,"page_size":1,"total_pages":1,"has_more":false}},"data":[{{"slug":"gpt-6-2-sol-high","model_creator":{{"name":"OpenAI"}},"evaluations":{{"artificial_analysis_intelligence_index":61.5,"artificial_analysis_coding_index":null}},"performance_v3":{{"mean_output_tokens_per_second":{text}}}}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn decimal_speeds_survive_seal_serialize_reopen_and_verify() {
        let mut unverified = Vec::new();
        let mut inexact = Vec::new();
        for index in 0..999 {
            let text = decimal_sample(index);
            let page = parse_page(Endpoint::ModelsFree, &decimal_page(&text), 1).unwrap();
            let snapshot = build_snapshot(
                Endpoint::ModelsFree,
                Fetched {
                    rows: page.data,
                    version: page.version,
                },
                "2026-11-23T00:00:00Z",
            )
            .unwrap();
            if snapshot.rows[0].speed.unwrap().value != text.parse::<f64>().unwrap() {
                inexact.push(text.clone());
            }
            let bytes = serde_json::to_vec(&snapshot).unwrap();
            match reopen(&bytes, Endpoint::ModelsFree) {
                Ok(reopened) if reopened == snapshot => {}
                other => unverified.push((text, other.err())),
            }
        }
        assert!(
            unverified.is_empty(),
            "{} of 999 failed verification after reopening: {unverified:?}",
            unverified.len()
        );
        assert!(
            inexact.is_empty(),
            "{} of 999 parsed inexactly",
            inexact.len()
        );
    }

    struct FixtureFetcher {
        pages: HashMap<String, Value>,
        requested: RefCell<Vec<String>>,
        failure: Option<String>,
        status: u16,
        refused: Option<(Endpoint, u16)>,
    }

    impl Fetcher for FixtureFetcher {
        fn fetch(&self, url: &str, _key: &ApiKey) -> io::Result<Response> {
            self.requested.borrow_mut().push(url.into());
            if let Some(failure) = &self.failure {
                return Err(io::Error::other(failure.clone()));
            }
            let status = match self.refused {
                Some((endpoint, status)) if url.starts_with(&endpoint.source()) => status,
                _ => self.status,
            };
            Ok(Response {
                status,
                body: serde_json::to_vec(&self.pages[url]).unwrap(),
            })
        }
    }

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn pages() -> HashMap<String, Value> {
        HashMap::from([
            (
                page_url(Endpoint::ModelsFree, 1),
                fixture(include_str!("catalog/fixtures/models-free-page-1.json")),
            ),
            (
                page_url(Endpoint::ModelsFree, 2),
                fixture(include_str!("catalog/fixtures/models-free-page-2.json")),
            ),
            (
                page_url(Endpoint::ModelsFree, 3),
                fixture(include_str!("catalog/fixtures/models-free-page-3.json")),
            ),
            (
                page_url(Endpoint::ModelsFree, 4),
                fixture(include_str!("catalog/fixtures/models-free-page-4.json")),
            ),
            (
                page_url(Endpoint::AgentsCoding, 1),
                fixture(include_str!("catalog/fixtures/agents-coding-page-1.json")),
            ),
            (
                page_url(Endpoint::AgentsCoding, 2),
                fixture(include_str!("catalog/fixtures/agents-coding-page-2.json")),
            ),
        ])
    }

    fn fetcher(pages: HashMap<String, Value>) -> FixtureFetcher {
        FixtureFetcher {
            pages,
            requested: RefCell::default(),
            failure: None,
            status: 200,
            refused: None,
        }
    }

    fn outcome_for(
        outcomes: &[(Endpoint, Result<Refreshed, String>)],
        endpoint: Endpoint,
    ) -> Result<Refreshed, String> {
        outcomes
            .iter()
            .find(|(candidate, _)| *candidate == endpoint)
            .unwrap()
            .1
            .clone()
    }

    fn source_for(state: &Path, endpoint: Endpoint) -> Source {
        Catalog::load(Some(state))
            .sources
            .into_iter()
            .find(|source| source.endpoint == endpoint)
            .unwrap()
    }

    fn key() -> ApiKey {
        ApiKey::new(SYNTHETIC_KEY.into()).unwrap()
    }

    fn published(published: usize, skipped: usize) -> Result<Refreshed, String> {
        Ok(Refreshed { published, skipped })
    }

    fn tokens(value: f64) -> Option<Speed> {
        Some(Speed {
            unit: SpeedUnit::TokensPerSecond,
            value,
        })
    }

    fn seconds(value: f64) -> Option<Speed> {
        Some(Speed {
            unit: SpeedUnit::SecondsPerTask,
            value,
        })
    }

    fn refreshed_catalog(name: &str) -> (PathBuf, Catalog) {
        let state = state(name);
        refresh(&state, &key(), &fetcher(pages()), "2026-11-23T00:00:00Z");
        let catalog = Catalog::load(Some(&state));
        (state, catalog)
    }

    #[test]
    fn free_tier_pages_publish_both_endpoints_with_exact_identities() {
        assert_eq!(
            Endpoint::ModelsFree.source(),
            "https://artificialanalysis.ai/api/v2/language/models/free"
        );
        assert_eq!(
            Endpoint::AgentsCoding.source(),
            "https://artificialanalysis.ai/api/v2/language/agents/coding"
        );
        let state = state("documented");
        let fixture = fetcher(pages());
        let outcomes = refresh(&state, &key(), &fixture, "2026-11-23T00:00:00Z");
        assert_eq!(
            outcome_for(&outcomes, Endpoint::AgentsCoding),
            published(4, 4)
        );
        assert_eq!(
            outcome_for(&outcomes, Endpoint::ModelsFree),
            published(9, 3)
        );
        let mut expected: Vec<String> = (1..=AGENT_PAGES)
            .map(|page| page_url(Endpoint::AgentsCoding, page))
            .collect();
        expected.extend((1..=MODEL_PAGES).map(|page| page_url(Endpoint::ModelsFree, page)));
        assert_eq!(*fixture.requested.borrow(), expected);
        assert_eq!(
            source_for(&state, Endpoint::AgentsCoding).snapshot.version,
            "fixture-2"
        );
        assert_eq!(
            source_for(&state, Endpoint::ModelsFree).snapshot.version,
            "4"
        );
        let models = source_for(&state, Endpoint::ModelsFree).snapshot.rows;
        assert_eq!(
            models[0],
            Row {
                provider: "codex".into(),
                slug: "gpt-6-2-sol-high".into(),
                effort: None,
                coding_index: None,
                intelligence_index: Some(61.5),
                speed: tokens(88.25),
            }
        );
        assert!(models.iter().all(|row| row.coding_index.is_none()));
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn exact_identities_never_borrow_another_version_effort_or_creator() {
        let (state, catalog) = refreshed_catalog("identities");
        let codex = |effort| lookup(&catalog, Provider::Codex, "gpt-6.2-sol", effort);
        assert_eq!(
            codex(Some("high")),
            Metrics {
                coding_agent_index: Some(47.0),
                coding_index: None,
                intelligence_index: Some(61.5),
                speed: seconds(310.0),
            }
        );
        assert_eq!(
            codex(None),
            Metrics {
                coding_agent_index: Some(44.0),
                coding_index: None,
                intelligence_index: Some(58.0),
                speed: seconds(330.0),
            }
        );
        assert_eq!(
            codex(Some("medium")),
            Metrics {
                coding_agent_index: Some(39.0),
                ..Metrics::default()
            }
        );
        assert_eq!(
            lookup(&catalog, Provider::Claude, "claude-opus-5-5", None),
            Metrics {
                intelligence_index: Some(60.0),
                ..Metrics::default()
            }
        );
        assert_eq!(
            opus_high(&catalog),
            Metrics {
                coding_agent_index: Some(52.0),
                coding_index: None,
                intelligence_index: Some(63.0),
                speed: seconds(280.0),
            }
        );
        for (provider, model, effort) in [
            (Provider::Codex, "gpt-6.2-sol", Some("low")),
            (Provider::Codex, "gpt-6.2-sol", Some("xhigh")),
            (Provider::Codex, "gpt-6.2-sol", Some("minimal")),
            (Provider::Codex, "gpt-6.2-sol", Some("ultra")),
            (Provider::Codex, "gpt-6.1-sol", Some("high")),
            (Provider::Codex, "gpt-reserve", None),
            (Provider::Codex, "codex-auto-review", None),
            (Provider::Codex, "claude-opus-5-5", Some("high")),
            (Provider::Claude, "claude-opus-5-5", Some("max")),
            (Provider::Claude, "claude-opus-5-5", Some("low")),
            (Provider::Claude, "opus", None),
            (Provider::Claude, "claude-opus", None),
        ] {
            assert_eq!(
                lookup(&catalog, provider, model, effort),
                Metrics::default(),
                "{model} {effort:?}"
            );
        }
        fs::remove_dir_all(state).unwrap();
    }

    type RowEdit = Box<dyn FnOnce(&mut Value)>;

    fn first_agent(edit: impl FnOnce(&mut Value)) -> Value {
        let mut row = pages()[&Endpoint::AgentsCoding.source()]["data"][0].clone();
        edit(&mut row);
        row
    }

    fn codex_identity(slug: &str, effort: Option<&str>) -> Option<Identity> {
        Some(Identity {
            provider: Provider::Codex,
            slug: slug.into(),
            effort: effort.map(str::to_owned),
        })
    }

    #[test]
    fn agent_identity_requires_exact_harness_creator_and_release() {
        assert_eq!(
            identify_agent(&first_agent(|_| {})),
            codex_identity("gpt-6-2-sol", Some("high"))
        );
        assert_eq!(
            identify_agent(&first_agent(|row| row["agent_creator"] = Value::Null)),
            codex_identity("gpt-6-2-sol", Some("high"))
        );
        assert_eq!(
            identify_agent(&first_agent(|row| row["model"]["effort"] = Value::Null)),
            codex_identity("gpt-6-2-sol", None)
        );
        let refused: [(&str, RowEdit); 11] = [
            ("router", Box::new(|row| row["is_router"] = json!(true))),
            (
                "router flag missing",
                Box::new(|row| row["is_router"] = Value::Null),
            ),
            (
                "router flag text",
                Box::new(|row| row["is_router"] = json!("false")),
            ),
            (
                "effort type",
                Box::new(|row| row["model"]["effort"] = json!(3)),
            ),
            (
                "dated release",
                Box::new(|row| row["model"]["release_slug"] = json!("gpt-6-2-sol-fixture")),
            ),
            (
                "api provider",
                Box::new(|row| row["provider"] = json!("Fixture Cloud API")),
            ),
            (
                "other creator",
                Box::new(|row| row["agent_creator"]["name"] = json!("Fixture Labs")),
            ),
            (
                "crossed creator",
                Box::new(|row| row["agent_creator"]["name"] = json!("Anthropic")),
            ),
            (
                "other agent",
                Box::new(|row| row["agent"]["name"] = json!("Fixture Agent")),
            ),
            (
                "crossed agent",
                Box::new(|row| row["agent"]["name"] = json!("Claude Code")),
            ),
            (
                "missing slug",
                Box::new(|row| row["model"]["slug"] = Value::Null),
            ),
        ];
        for (case, edit) in refused {
            assert_eq!(identify_agent(&first_agent(edit)), None, "{case}");
        }
        let model =
            |slug: &str, creator: &str| json!({"slug": slug, "model_creator": {"name": creator}});
        assert_eq!(
            identify_model(&model("claude-opus-5-5-high", "OpenAI")),
            codex_identity("claude-opus-5-5-high", None)
        );
        assert_eq!(
            identify_model(&model("claude-opus-5-5-high", "Anthropic"))
                .unwrap()
                .provider,
            Provider::Claude
        );
        assert_eq!(
            identify_model(&model("gpt-6-2-sol-high", "Fixture Labs")),
            None
        );
        assert_eq!(
            identify_model(
                &json!({"slug": "gpt-6-2-sol-high", "model_creator": {"slug": "openai"}})
            ),
            None
        );
        assert_eq!(
            identify_model(&json!({"model_creator": {"name": "OpenAI"}})),
            None
        );
    }

    #[test]
    fn ambiguous_identities_are_skipped_instead_of_chosen() {
        let mut duplicated = pages();
        let first = duplicated
            .get_mut(&page_url(Endpoint::AgentsCoding, 1))
            .unwrap();
        let mut copy = first["data"][0].clone();
        copy["coding_agent_index"] = json!(12.0);
        first["data"][3] = copy;
        let state = state("ambiguous");
        let outcomes = refresh(&state, &key(), &fetcher(duplicated), "2026-11-23T00:00:00Z");
        assert_eq!(
            outcome_for(&outcomes, Endpoint::AgentsCoding),
            published(3, 5)
        );
        let catalog = Catalog::load(Some(&state));
        assert_eq!(
            lookup(&catalog, Provider::Codex, "gpt-6.2-sol", Some("high")).coding_agent_index,
            None
        );
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn agents_coding_refusal_never_suppresses_models_free() {
        let fresh = state("tier-fresh");
        let (state, _) = refreshed_catalog("tier");
        let agents_before = source_for(&state, Endpoint::AgentsCoding).snapshot;
        let models_before = source_for(&state, Endpoint::ModelsFree).snapshot;
        let free_tier = FixtureFetcher {
            refused: Some((Endpoint::AgentsCoding, 403)),
            ..fetcher(pages())
        };
        let outcomes = refresh(&state, &key(), &free_tier, "2026-11-24T00:00:00Z");
        assert_eq!(
            outcome_for(&outcomes, Endpoint::AgentsCoding).unwrap_err(),
            "access refused for this key or plan tier (HTTP 403); previous data kept"
        );
        assert_eq!(
            outcome_for(&outcomes, Endpoint::ModelsFree),
            published(9, 3)
        );
        assert!(!refresh_failed(&outcomes));
        assert_eq!(
            outcome_line(Endpoint::AgentsCoding, &outcomes[0].1),
            "agents/coding optional endpoint kept previous data · access refused for this key or plan tier (HTTP 403); previous data kept"
        );
        let agents = source_for(&state, Endpoint::AgentsCoding);
        assert_eq!(
            (agents.origin, agents.snapshot),
            (Origin::Cache, agents_before.clone())
        );
        let models = source_for(&state, Endpoint::ModelsFree).snapshot;
        assert_ne!(models.provenance, models_before.provenance);
        assert!(models
            .provenance
            .ends_with("retrieved 2026-11-24T00:00:00Z"));
        let outcomes = refresh(&fresh, &key(), &free_tier, "2026-11-24T00:00:00Z");
        assert!(!refresh_failed(&outcomes));
        let catalog = Catalog::load(Some(&fresh));
        assert_eq!(
            opus_high(&catalog),
            Metrics {
                intelligence_index: Some(63.0),
                speed: tokens(52.5),
                ..Metrics::default()
            }
        );
        for (status, expected) in [
            (
                401,
                "access refused for this key or plan tier (HTTP 401); previous data kept",
            ),
            (429, "rate limited (HTTP 429); previous data kept"),
            (410, "unexpected HTTP 410; previous data kept"),
        ] {
            let before = Catalog::load(Some(&state)).sources;
            let refusal = FixtureFetcher {
                status,
                ..fetcher(pages())
            };
            let outcomes = refresh(&state, &key(), &refusal, "2026-11-25T00:00:00Z");
            for endpoint in Endpoint::ALL {
                assert_eq!(
                    outcome_for(&outcomes, endpoint).unwrap_err(),
                    expected,
                    "{status}"
                );
            }
            assert!(refresh_failed(&outcomes));
            assert_eq!(refusal.requested.borrow().len(), 2, "{status}");
            for (after, before) in Catalog::load(Some(&state)).sources.iter().zip(&before) {
                assert_eq!(after.origin, Origin::Cache, "{status}");
                assert_eq!(after.snapshot, before.snapshot, "{status}");
            }
        }
        fs::remove_dir_all(state).unwrap();
        fs::remove_dir_all(fresh).unwrap();
    }

    fn keep_only_unknown_rows(pages: &mut HashMap<String, Value>) {
        for page in pages.values_mut() {
            let data = page["data"].as_array_mut().unwrap();
            data.retain(|row| {
                let endpoint =
                    if row.get("agent").is_some() || row.get("coding_agent_index").is_some() {
                        Endpoint::AgentsCoding
                    } else {
                        Endpoint::ModelsFree
                    };
                identify(endpoint, row).is_none()
            });
        }
    }

    #[test]
    fn empty_supported_refresh_keeps_a_valid_cache() {
        let (state, _) = refreshed_catalog("empty");
        let before = Catalog::load(Some(&state)).sources;
        let mut unsupported = pages();
        keep_only_unknown_rows(&mut unsupported);
        let outcomes = refresh(
            &state,
            &key(),
            &fetcher(unsupported),
            "2026-11-24T00:00:00Z",
        );
        assert_eq!(
            outcome_for(&outcomes, Endpoint::AgentsCoding).unwrap_err(),
            "no supported rows among 4 published rows; previous data kept"
        );
        assert_eq!(
            outcome_for(&outcomes, Endpoint::ModelsFree).unwrap_err(),
            "no supported rows among 3 published rows; previous data kept"
        );
        let mut empty = pages();
        for endpoint in Endpoint::ALL {
            let first = empty.get_mut(&endpoint.source()).unwrap();
            first["data"] = json!([]);
            first["pagination"] =
                json!({"page": 1, "page_size": 50, "total_pages": 0, "has_more": false});
        }
        let fixture = fetcher(empty);
        let outcomes = refresh(&state, &key(), &fixture, "2026-11-25T00:00:00Z");
        for endpoint in Endpoint::ALL {
            assert_eq!(
                outcome_for(&outcomes, endpoint).unwrap_err(),
                "no supported rows among 0 published rows; previous data kept"
            );
        }
        assert_eq!(fixture.requested.borrow().len(), 2);
        for (after, before) in Catalog::load(Some(&state)).sources.iter().zip(&before) {
            assert_eq!(after.origin, Origin::Cache);
            assert_eq!(after.snapshot, before.snapshot);
        }
        fs::remove_dir_all(state).unwrap();
    }

    type PageEdit = (
        &'static str,
        Endpoint,
        u64,
        Box<dyn Fn(&mut Value)>,
        &'static str,
    );

    fn malformed_cases() -> Vec<PageEdit> {
        use Endpoint::{AgentsCoding as Agents, ModelsFree as Models};
        vec![
            (
                "no pagination",
                Models,
                1,
                Box::new(|page| page["pagination"] = Value::Null),
                "response has no pagination object",
            ),
            (
                "page",
                Agents,
                2,
                Box::new(|page| page["pagination"]["page"] = json!(3)),
                "expected page 2",
            ),
            (
                "zero page",
                Models,
                1,
                Box::new(|page| page["pagination"]["page"] = json!(0)),
                "expected page 1",
            ),
            (
                "has_more",
                Agents,
                1,
                Box::new(|page| page["pagination"]["has_more"] = json!(false)),
                "pagination has_more contradicts total_pages",
            ),
            (
                "has_more missing",
                Models,
                4,
                Box::new(|page| page["pagination"]["has_more"] = Value::Null),
                "pagination has_more is missing",
            ),
            (
                "page_size",
                Agents,
                1,
                Box::new(|page| page["pagination"]["page_size"] = json!(2)),
                "pagination page_size is missing or smaller than the page",
            ),
            (
                "bound",
                Models,
                1,
                Box::new(|page| page["pagination"]["total_pages"] = json!(MAX_PAGES + 1)),
                "pagination total is out of bounds",
            ),
            (
                "total",
                Agents,
                2,
                Box::new(|page| {
                    page["pagination"]["total_pages"] = json!(3);
                    page["pagination"]["has_more"] = json!(true);
                }),
                "pagination total changed between pages",
            ),
            (
                "model version",
                Models,
                3,
                Box::new(|page| page["intelligence_index_version"] = json!(5)),
                "intelligence_index_version changed between pages",
            ),
            (
                "model version type",
                Models,
                1,
                Box::new(|page| page["intelligence_index_version"] = json!("4")),
                "intelligence_index_version is missing or has the wrong type",
            ),
            (
                "agent version",
                Agents,
                2,
                Box::new(|page| page["coding_agent_index_version"] = json!("fixture-3")),
                "coding_agent_index_version changed between pages",
            ),
            (
                "agent version type",
                Agents,
                1,
                Box::new(|page| page["coding_agent_index_version"] = json!(2)),
                "coding_agent_index_version is missing or has the wrong type",
            ),
            (
                "tier",
                Agents,
                2,
                Box::new(|page| page["tier"] = json!("commercial")),
                "tier changed between pages",
            ),
            (
                "tier missing",
                Models,
                1,
                Box::new(|page| page["tier"] = Value::Null),
                "response tier is missing or malformed",
            ),
            (
                "row shape",
                Agents,
                1,
                Box::new(|page| page["data"][1] = json!("row")),
                "response data holds a non-object row",
            ),
            (
                "agent metric",
                Agents,
                1,
                Box::new(|page| page["data"][0]["coding_agent_index"] = json!("47")),
                "coding_agent_index is not numeric",
            ),
            (
                "agent speed",
                Agents,
                2,
                Box::new(|page| page["data"][1]["time_per_task_seconds"] = json!(0)),
                "speed out of range for gpt-6-2-sol",
            ),
            (
                "intelligence range",
                Models,
                1,
                Box::new(|page| {
                    page["data"][0]["evaluations"]["artificial_analysis_intelligence_index"] =
                        json!(250)
                }),
                "intelligence index out of range for gpt-6-2-sol-high",
            ),
            (
                "intelligence type",
                Models,
                2,
                Box::new(|page| {
                    page["data"][0]["evaluations"]["artificial_analysis_intelligence_index"] =
                        json!("58")
                }),
                "evaluations.artificial_analysis_intelligence_index is not numeric",
            ),
            (
                "coding range",
                Models,
                1,
                Box::new(|page| {
                    page["data"][0]["evaluations"]["artificial_analysis_coding_index"] = json!(-1)
                }),
                "coding index out of range for gpt-6-2-sol-high",
            ),
            (
                "model speed",
                Models,
                1,
                Box::new(|page| page["data"][0]["performance_v3"] = json!(88)),
                "performance_v3 is not an object",
            ),
        ]
    }

    #[test]
    fn malformed_pagination_or_metrics_reject_only_that_endpoint() {
        let (state, _) = refreshed_catalog("malformed");
        for (case, endpoint, page, edit, expected) in malformed_cases() {
            let before = source_for(&state, endpoint).snapshot;
            let mut edited = pages();
            edit(edited.get_mut(&page_url(endpoint, page)).unwrap());
            let outcomes = refresh(&state, &key(), &fetcher(edited), "2026-11-24T00:00:00Z");
            assert_eq!(
                outcome_for(&outcomes, endpoint).unwrap_err(),
                expected,
                "{case}"
            );
            let other = Endpoint::ALL
                .into_iter()
                .find(|candidate| *candidate != endpoint)
                .unwrap();
            assert!(outcome_for(&outcomes, other).is_ok(), "{case}");
            let after = source_for(&state, endpoint);
            assert_eq!(after.origin, Origin::Cache, "{case}");
            assert_eq!(after.snapshot, before, "{case}");
        }
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn legacy_and_retired_fields_are_never_read_as_measurements() {
        let mut legacy = pages()[&Endpoint::ModelsFree.source()]["data"][0].clone();
        legacy["performance_v3"] = Value::Null;
        legacy["performance"] = json!({"median_output_tokens_per_second": 120.0});
        legacy["median_output_tokens_per_second"] = json!(130.0);
        legacy["evaluations"]["artificial_analysis_agentic_index"] = json!(70.0);
        let row = normalize(Endpoint::ModelsFree, &legacy).unwrap().unwrap();
        assert_eq!(
            (row.coding_index, row.intelligence_index, row.speed),
            (None, Some(61.5), None)
        );
        legacy["performance_v3"] = json!({"mean_output_tokens_per_second": null});
        assert_eq!(
            normalize(Endpoint::ModelsFree, &legacy)
                .unwrap()
                .unwrap()
                .speed,
            None
        );
        legacy["evaluations"] = Value::Null;
        let row = normalize(Endpoint::ModelsFree, &legacy).unwrap().unwrap();
        assert_eq!((row.coding_index, row.intelligence_index), (None, None));
        let mut agent = first_agent(|_| {});
        agent["coding_index"] = json!(80.0);
        agent["median_seconds_per_task"] = json!(1.0);
        agent["coding_agent_index"] = Value::Null;
        agent["time_per_task_seconds"] = Value::Null;
        let row = normalize(Endpoint::AgentsCoding, &agent).unwrap().unwrap();
        assert_eq!(
            (row.coding_index, row.intelligence_index, row.speed),
            (None, None, None)
        );
    }

    #[test]
    fn curl_reads_no_implicit_config_and_inherits_no_key() {
        let command = curl_command();
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments[0], "-q");
        assert_eq!(&arguments[1..3], ["--config", "-"]);
        assert!(!arguments
            .iter()
            .any(|argument| argument.contains("api-key")));
        assert!(!arguments.iter().any(|argument| argument == "--fail"));
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert!(removed.contains(&KEY_VARIABLE.to_owned()));
        assert!(removed.contains(&"CURL_HOME".to_owned()));
        assert_eq!(
            curl_config(&Endpoint::ModelsFree.source(), &key()),
            format!(
                "url = \"https://artificialanalysis.ai/api/v2/language/models/free\"\nheader = \"x-api-key: {SYNTHETIC_KEY}\"\n"
            )
        );
        let mut output = br#"{"data":[]}"#.to_vec();
        output.extend_from_slice(b"\nagent-command-http-status:403");
        assert_eq!(
            split_status(output).unwrap(),
            Response {
                status: 403,
                body: br#"{"data":[]}"#.to_vec()
            }
        );
        assert!(split_status(b"{}".to_vec()).is_err());
        assert!(split_status(b"{}\nagent-command-http-status:000".to_vec()).is_err());
    }

    #[test]
    fn refresh_redacts_the_key_and_stays_on_the_published_origin() {
        let state = state("redact");
        let leaking = FixtureFetcher {
            failure: Some(format!("denied for {SYNTHETIC_KEY}")),
            ..fetcher(pages())
        };
        let outcomes = refresh(&state, &key(), &leaking, "2026-11-25T00:00:00Z");
        for endpoint in Endpoint::ALL {
            assert_eq!(
                outcome_for(&outcomes, endpoint).unwrap_err(),
                "denied for [redacted]"
            );
        }
        refresh(&state, &key(), &fetcher(pages()), "2026-11-25T00:00:00Z");
        for entry in fs::read_dir(cache_directory(&state)).unwrap() {
            let text = fs::read_to_string(entry.unwrap().path()).unwrap();
            assert!(!text.contains(SYNTHETIC_KEY));
        }
        assert_eq!(format!("{:?}", key()), "ApiKey(redacted)");
        assert!(ApiKey::new("key with\nnewline".into()).is_err());
        assert!(ApiKey::new("key\"quote".into()).is_err());
        assert!(
            require_same_origin("http://artificialanalysis.ai/api/v2/language/models/free")
                .is_err()
        );
        assert!(require_same_origin(
            "https://artificialanalysis.ai.example/api/v2/language/models/free"
        )
        .is_err());
        assert!(
            require_same_origin("https://artificialanalysis.ai/api/v2/data/llms/models").is_err()
        );
        assert!(require_same_origin(&page_url(Endpoint::AgentsCoding, 2)).is_ok());
        fs::remove_dir_all(state).unwrap();
    }
}
