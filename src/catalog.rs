use crate::{auth::Provider, routing};
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

pub(crate) const SCHEMA: &str = "agent-command-catalog-v1";
pub(crate) const ORIGIN: &str = "https://artificialanalysis.ai";
pub(crate) const KEY_VARIABLE: &str = "ARTIFICIAL_ANALYSIS_API_KEY";
const CACHE_DIRECTORY: &str = "catalog";
const MAX_CACHE_BYTES: u64 = 1 << 20;
const MAX_RESPONSE_BYTES: usize = 4 << 20;
const MAX_ROWS: usize = 2_000;
const MAX_PAGES: u64 = 20;
const MAX_VERSION_LENGTH: usize = 64;
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

    fn path(self) -> &'static str {
        match self {
            Self::ModelsFree => "/api/v2/data/llms/models",
            Self::AgentsCoding => "/api/v2/data/agents/coding",
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
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) coding_index: Option<f64>,
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
            let key = (&row.provider, &row.model, &row.effort);
            if keys.contains(&key) {
                return Err(format!("duplicate row {}/{}", row.provider, row.model));
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

fn validate_row(row: &Row, endpoint: Endpoint) -> Result<(), String> {
    let provider = Provider::named(&row.provider)
        .ok_or_else(|| format!("unknown provider {}", row.provider))?;
    if routing::supported(provider, &row.model, &row.effort).is_none() {
        return Err(format!(
            "unsupported configuration {}/{} {}",
            row.provider, row.model, row.effort
        ));
    }
    if row
        .coding_index
        .is_some_and(|index| !index.is_finite() || !(0.0..=100.0).contains(&index))
    {
        return Err(format!("coding index out of range for {}", row.model));
    }
    if let Some(speed) = row.speed {
        if speed.unit != endpoint.speed_unit() {
            return Err(format!("speed unit mismatch for {}", row.model));
        }
        if !speed.value.is_finite() || speed.value <= 0.0 {
            return Err(format!("speed out of range for {}", row.model));
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Score {
    pub(crate) endpoint: Endpoint,
    pub(crate) value: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Metrics {
    pub(crate) index: Option<Score>,
    pub(crate) speed: Option<Speed>,
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
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).map_err(|error| format!("cache invalid: {error}"))?;
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

    fn exact_rows<'a>(
        &'a self,
        provider: Provider,
        model: &'a str,
        effort: &'a str,
    ) -> impl Iterator<Item = (Endpoint, &'a Row)> + 'a {
        Endpoint::ALL.into_iter().flat_map(move |endpoint| {
            self.sources
                .iter()
                .filter(move |source| source.endpoint == endpoint)
                .flat_map(|source| source.snapshot.rows.iter())
                .filter(move |row| {
                    row.provider == provider.binary() && row.model == model && row.effort == effort
                })
                .map(move |row| (endpoint, row))
        })
    }

    pub(crate) fn metrics(&self, provider: Provider, model: &str, effort: &str) -> Metrics {
        Metrics {
            index: self
                .exact_rows(provider, model, effort)
                .find_map(|(endpoint, row)| {
                    row.coding_index.map(|value| Score { endpoint, value })
                }),
            speed: self
                .exact_rows(provider, model, effort)
                .find_map(|(_, row)| row.speed),
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
                format!(
                    "{} · {origin} · version {} · {} rows · {}",
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

pub(crate) trait Fetcher {
    fn fetch(&self, url: &str, key: &ApiKey) -> io::Result<Vec<u8>>;
}

pub(crate) struct CurlFetcher;

impl Fetcher for CurlFetcher {
    fn fetch(&self, url: &str, key: &ApiKey) -> io::Result<Vec<u8>> {
        require_same_origin(url)?;
        let mut child = Command::new("curl")
            .args([
                "--config",
                "-",
                "--proto",
                "=https",
                "--max-redirs",
                "0",
                "--fail",
                "--silent",
                "--show-error",
                "--max-time",
                FETCH_SECONDS,
                "--max-filesize",
                &MAX_RESPONSE_BYTES.to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut input = child.stdin.take().expect("piped curl input");
        writeln!(input, "url = \"{url}\"\nheader = \"x-api-key: {}\"", key.0)?;
        drop(input);
        let mut body = Vec::new();
        child
            .stdout
            .take()
            .expect("piped curl output")
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)?;
        let status = child.wait()?;
        if !status.success() {
            return Err(io::Error::other(format!("request failed with {status}")));
        }
        Ok(body)
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

struct Page {
    data: Vec<Value>,
    total_pages: u64,
    version: Option<String>,
}

fn parse_page(bytes: &[u8], requested: u64) -> Result<Page, String> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("response exceeds the size bound".into());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "response is not JSON")?;
    if value["status"] != 200 {
        return Err("response status is not 200".into());
    }
    let data = value["data"]
        .as_array()
        .ok_or("response has no data array")?
        .clone();
    let version = match &value["version"] {
        Value::Null => None,
        Value::String(version) if valid_version(version) => Some(version.clone()),
        _ => return Err("response version is malformed".into()),
    };
    let total_pages = match &value["pagination"] {
        Value::Null if requested == 1 => 1,
        Value::Null => return Err("pagination disappeared".into()),
        pagination => {
            if pagination["page"].as_u64() != Some(requested) {
                return Err(format!("expected page {requested}"));
            }
            pagination["total_pages"]
                .as_u64()
                .filter(|total| (1..=MAX_PAGES).contains(total))
                .ok_or("pagination total is out of bounds")?
        }
    };
    Ok(Page {
        data,
        total_pages,
        version,
    })
}

fn fetch_all(
    endpoint: Endpoint,
    key: &ApiKey,
    fetcher: &dyn Fetcher,
) -> Result<(Vec<Value>, Option<String>), String> {
    let mut rows = Vec::new();
    let mut expected_total = None;
    let mut expected_version = None;
    let mut page = 1;
    loop {
        let url = page_url(endpoint, page);
        require_same_origin(&url).map_err(|error| error.to_string())?;
        let bytes = fetcher
            .fetch(&url, key)
            .map_err(|error| error.to_string())?;
        let parsed = parse_page(&bytes, page)?;
        if *expected_total.get_or_insert(parsed.total_pages) != parsed.total_pages {
            return Err("pagination total changed between pages".into());
        }
        if page == 1 {
            expected_version = parsed.version.clone();
        } else if parsed.version != expected_version {
            return Err("version changed between pages".into());
        }
        rows.extend(parsed.data);
        if rows.len() > MAX_ROWS {
            return Err(format!("more than {MAX_ROWS} rows"));
        }
        if page == parsed.total_pages {
            return Ok((rows, expected_version));
        }
        page += 1;
    }
}

fn number(value: &Value) -> Result<Option<f64>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Number(number) => Ok(number.as_f64()),
        _ => Err("metric is not numeric".into()),
    }
}

fn normalize(endpoint: Endpoint, raw: &Value) -> Result<Option<Row>, String> {
    let (owner, slug, index, speed) = match endpoint {
        Endpoint::ModelsFree => (
            &raw["model_creator"]["slug"],
            &raw["slug"],
            &raw["evaluations"]["artificial_analysis_coding_index"],
            &raw["median_output_tokens_per_second"],
        ),
        Endpoint::AgentsCoding => (
            &raw["agent"]["slug"],
            &raw["model"]["slug"],
            &raw["coding_index"],
            &raw["median_seconds_per_task"],
        ),
    };
    let (Some(owner), Some(slug)) = (owner.as_str(), slug.as_str()) else {
        return Err("row identity is missing".into());
    };
    let Some(configuration) = routing::by_index_slug(endpoint, owner, slug) else {
        return Ok(None);
    };
    Ok(Some(Row {
        provider: configuration.provider.binary().into(),
        model: configuration.model.into(),
        effort: configuration.effort.into(),
        coding_index: number(index)?,
        speed: number(speed)?.map(|value| Speed {
            unit: endpoint.speed_unit(),
            value,
        }),
    }))
}

fn build_snapshot(
    endpoint: Endpoint,
    raw: &[Value],
    version: Option<String>,
    retrieved: &str,
) -> Result<Snapshot, String> {
    let mut rows = Vec::new();
    for row in raw {
        if let Some(row) = normalize(endpoint, row)? {
            rows.push(row);
        }
    }
    let version = version.unwrap_or_else(|| retrieved.to_owned());
    let snapshot = Snapshot::sealed(
        endpoint,
        version,
        format!(
            "Artificial Analysis {} retrieved {retrieved}",
            endpoint.scope()
        ),
        rows,
    );
    snapshot.validate(endpoint)?;
    Ok(snapshot)
}

fn publish(state: &Path, snapshot: &Snapshot) -> io::Result<()> {
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
) -> Vec<(Endpoint, Result<usize, String>)> {
    Endpoint::ALL
        .into_iter()
        .map(|endpoint| {
            let outcome = fetch_all(endpoint, key, fetcher)
                .and_then(|(raw, version)| build_snapshot(endpoint, &raw, version, retrieved))
                .and_then(|snapshot| {
                    publish(state, &snapshot)
                        .map(|()| snapshot.rows.len())
                        .map_err(|error| error.to_string())
                })
                .map_err(|error| key.redact(&error));
            (endpoint, outcome)
        })
        .collect()
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
            let mut failed = false;
            for (endpoint, outcome) in refresh(&state, &key, &CurlFetcher, &retrieved) {
                match outcome {
                    Ok(rows) => println!("{} refreshed · {rows} rows", endpoint.scope()),
                    Err(error) => {
                        failed = true;
                        println!("{} kept previous data · {error}", endpoint.scope());
                    }
                }
            }
            if failed {
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
    use serde_json::json;
    use std::{cell::RefCell, collections::HashMap};

    const SYNTHETIC_KEY: &str = "synthetic-key-0123";

    fn state(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("ac-catalog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(cache_directory(&path)).unwrap();
        path
    }

    fn synthetic_row(model: &str, effort: &str, index: f64, speed: Speed) -> Row {
        Row {
            provider: if model == "gpt-5-codex" {
                "codex"
            } else {
                "claude"
            }
            .into(),
            model: model.into(),
            effort: effort.into(),
            coding_index: Some(index),
            speed: Some(speed),
        }
    }

    fn write(state: &Path, endpoint: Endpoint, snapshot: &Snapshot) {
        fs::write(
            cache_directory(state).join(endpoint.cache_file()),
            serde_json::to_vec(snapshot).unwrap(),
        )
        .unwrap();
    }

    type Mutation = Box<dyn Fn(&mut Snapshot)>;

    fn agents_snapshot() -> Snapshot {
        Snapshot::sealed(
            Endpoint::AgentsCoding,
            "synthetic-1".into(),
            "synthetic test fixture".into(),
            vec![synthetic_row(
                "opus",
                "default",
                61.0,
                Speed {
                    unit: SpeedUnit::SecondsPerTask,
                    value: 120.0,
                },
            )],
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
        assert_eq!(
            catalog.metrics(Provider::Claude, "opus", "default"),
            Metrics::default()
        );
        assert!(catalog
            .describe()
            .iter()
            .all(|line| line.contains("bundled snapshot (no state directory)")));
    }

    #[test]
    fn cache_integrity_falls_back_independently_per_endpoint() {
        let state = state("integrity");
        write(&state, Endpoint::AgentsCoding, &agents_snapshot());
        let mut tampered = agents_snapshot();
        tampered.scope = Endpoint::ModelsFree;
        tampered.source = Endpoint::ModelsFree.source();
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
        assert_eq!(
            catalog
                .metrics(Provider::Claude, "opus", "default")
                .index
                .unwrap(),
            Score {
                endpoint: Endpoint::AgentsCoding,
                value: 61.0
            }
        );
        let cases: [(&str, Mutation); 6] = [
            ("scope", Box::new(|s| s.scope = Endpoint::ModelsFree)),
            (
                "schema",
                Box::new(|s| s.schema = "agent-command-catalog-v0".into()),
            ),
            ("version", Box::new(|s| s.version = "bad version".into())),
            ("index", Box::new(|s| s.rows[0].coding_index = Some(101.0))),
            (
                "unit",
                Box::new(|s| {
                    s.rows[0].speed = Some(Speed {
                        unit: SpeedUnit::TokensPerSecond,
                        value: 1.0,
                    })
                }),
            ),
            (
                "unsupported",
                Box::new(|s| s.rows[0].effort = "turbo".into()),
            ),
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
            assert_eq!(
                catalog.metrics(Provider::Claude, "opus", "default"),
                Metrics::default(),
                "{case}"
            );
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

    struct FixtureFetcher {
        pages: HashMap<String, Value>,
        requested: RefCell<Vec<String>>,
        failure: Option<String>,
    }

    impl Fetcher for FixtureFetcher {
        fn fetch(&self, url: &str, _key: &ApiKey) -> io::Result<Vec<u8>> {
            self.requested.borrow_mut().push(url.into());
            if let Some(failure) = &self.failure {
                return Err(io::Error::other(failure.clone()));
            }
            Ok(serde_json::to_vec(&self.pages[url]).unwrap())
        }
    }

    fn pages() -> HashMap<String, Value> {
        let agents = Endpoint::AgentsCoding.source();
        let models = Endpoint::ModelsFree.source();
        HashMap::from([
            (
                agents.clone(),
                json!({"status": 200, "version": "synthetic-v2", "pagination": {"page": 1, "total_pages": 2},
                       "data": [{"agent": {"slug": "codex"}, "model": {"slug": "gpt-5-codex-high"},
                                 "coding_index": 58.5, "median_seconds_per_task": 240.0}]}),
            ),
            (
                format!("{agents}?page=2"),
                json!({"status": 200, "version": "synthetic-v2", "pagination": {"page": 2, "total_pages": 2},
                       "data": [{"agent": {"slug": "unlisted"}, "model": {"slug": "other"}}]}),
            ),
            (
                models,
                json!({"status": 200, "data": [
                    {"slug": "claude-sonnet", "model_creator": {"slug": "anthropic"},
                     "evaluations": {"artificial_analysis_coding_index": 49.0},
                     "median_output_tokens_per_second": 75.0}]}),
            ),
        ])
    }

    fn fetcher(pages: HashMap<String, Value>) -> FixtureFetcher {
        FixtureFetcher {
            pages,
            requested: RefCell::default(),
            failure: None,
        }
    }

    #[test]
    fn refresh_publishes_only_fully_validated_same_origin_pages() {
        let state = state("refresh");
        let key = ApiKey::new(SYNTHETIC_KEY.into()).unwrap();
        let fixture = fetcher(pages());
        let outcomes = refresh(&state, &key, &fixture, "2026-11-23T00:00:00Z");
        assert_eq!(
            outcomes,
            [
                (Endpoint::AgentsCoding, Ok(1)),
                (Endpoint::ModelsFree, Ok(1))
            ]
        );
        assert!(fixture
            .requested
            .borrow()
            .iter()
            .all(|url| url.starts_with("https://artificialanalysis.ai/api/v2/")));
        let catalog = Catalog::load(Some(&state));
        assert!(catalog.sources.iter().all(|s| s.origin == Origin::Cache));
        assert_eq!(catalog.sources[0].snapshot.version, "synthetic-v2");
        assert_eq!(catalog.sources[1].snapshot.version, "2026-11-23T00:00:00Z");
        let codex = catalog.metrics(Provider::Codex, "gpt-5-codex", "high");
        assert_eq!(codex.index.unwrap().value, 58.5);
        assert_eq!(
            codex.speed.unwrap(),
            Speed {
                unit: SpeedUnit::SecondsPerTask,
                value: 240.0
            }
        );

        let mut broken = pages();
        broken
            .get_mut(&format!("{}?page=2", Endpoint::AgentsCoding.source()))
            .unwrap()["pagination"]["page"] = json!(3);
        let mut changed = pages();
        changed
            .get_mut(&format!("{}?page=2", Endpoint::AgentsCoding.source()))
            .unwrap()["version"] = json!("synthetic-v3");
        let mut out_of_range = pages();
        out_of_range
            .get_mut(&Endpoint::ModelsFree.source())
            .unwrap()["data"][0]["evaluations"]["artificial_analysis_coding_index"] = json!(250);
        let mut runaway = pages();
        runaway.get_mut(&Endpoint::AgentsCoding.source()).unwrap()["pagination"]["total_pages"] =
            json!(MAX_PAGES + 1);
        for (attempt, (case, pages, endpoint, expected)) in [
            ("page", broken, Endpoint::AgentsCoding, "expected page 2"),
            (
                "version",
                changed,
                Endpoint::AgentsCoding,
                "version changed between pages",
            ),
            (
                "range",
                out_of_range,
                Endpoint::ModelsFree,
                "coding index out of range for sonnet",
            ),
            (
                "bound",
                runaway,
                Endpoint::AgentsCoding,
                "pagination total is out of bounds",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let retrieved = format!("2026-11-24T00:00:0{attempt}Z");
            let outcomes = refresh(&state, &key, &fetcher(pages), &retrieved);
            let (_, outcome) = outcomes.iter().find(|(e, _)| *e == endpoint).unwrap();
            assert_eq!(outcome.as_ref().unwrap_err(), expected, "{case}");
            let kept = Catalog::load(Some(&state));
            let source = kept
                .sources
                .iter()
                .find(|s| s.endpoint == endpoint)
                .unwrap();
            assert_eq!(source.origin, Origin::Cache, "{case}");
            assert_ne!(source.snapshot.version, retrieved, "{case}");
        }

        let leaking = FixtureFetcher {
            failure: Some(format!("denied for {SYNTHETIC_KEY}")),
            ..fetcher(pages())
        };
        for (_, outcome) in refresh(&state, &key, &leaking, "2026-11-25T00:00:00Z") {
            assert_eq!(outcome.unwrap_err(), "denied for [redacted]");
        }
        for entry in fs::read_dir(cache_directory(&state)).unwrap() {
            let text = fs::read_to_string(entry.unwrap().path()).unwrap();
            assert!(!text.contains(SYNTHETIC_KEY));
        }
        assert_eq!(format!("{key:?}"), "ApiKey(redacted)");
        assert!(ApiKey::new("key with\nnewline".into()).is_err());
        assert!(
            require_same_origin("http://artificialanalysis.ai/api/v2/data/llms/models").is_err()
        );
        assert!(require_same_origin(
            "https://artificialanalysis.ai.example/api/v2/data/llms/models"
        )
        .is_err());
        assert!(require_same_origin(&page_url(Endpoint::ModelsFree, 2)).is_ok());
        fs::remove_dir_all(state).unwrap();
    }
}
