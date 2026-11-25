#[cfg(unix)]
#[path = "auth/unix.rs"]
mod privacy;
#[cfg(windows)]
#[path = "auth/windows.rs"]
mod privacy;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{Mutex, PoisonError},
};

static ADOPTED: Mutex<[Option<PathBuf>; 3]> = Mutex::new([None, None, None]);

const CODEX_PROFILE_ARGUMENTS: [&str; 4] = [
    "-c",
    "cli_auth_credentials_store='file'",
    "-c",
    "forced_login_method='chatgpt'",
];
const WINDOWS_EXECUTABLE_EXTENSIONS: [&str; 4] = [".exe", ".com", ".cmd", ".bat"];
const STAGING_PREFIX: &str = ".connecting-";
const FALLBACK_PROFILE_NAME: &str = "main";
const SYSTEM_DEFAULT_NAME: &str = "default";
const CODEX_AUTH_FILE: &str = "auth.json";
const ISOLATED_ENV_FILE: &str = "profile-env.json";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Found {
    OnPath,
    At(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub native_resume: bool,
    pub persistent_stdio: bool,
    pub interactive_permissions: bool,
    pub streaming_thinking: bool,
    pub cost_usd: bool,
    pub mcp_injection: bool,
    pub native_worktree: bool,
}

impl Provider {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Gemini];

    pub fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.binary().eq_ignore_ascii_case(name))
    }

    pub fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Gemini => "Gemini CLI",
        }
    }

    pub fn capabilities(self) -> ProviderCapabilities {
        match self {
            Self::Claude => ProviderCapabilities {
                native_resume: true,
                persistent_stdio: true,
                interactive_permissions: true,
                streaming_thinking: true,
                cost_usd: true,
                mcp_injection: true,
                native_worktree: true,
            },
            Self::Codex => ProviderCapabilities {
                native_resume: true,
                persistent_stdio: false,
                interactive_permissions: false,
                streaming_thinking: true,
                cost_usd: false,
                mcp_injection: true,
                native_worktree: true,
            },
            Self::Gemini => ProviderCapabilities {
                native_resume: true,
                persistent_stdio: false,
                interactive_permissions: false,
                streaming_thinking: false,
                cost_usd: false,
                mcp_injection: true,
                native_worktree: true,
            },
        }
    }

    pub(crate) fn slot(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
            Self::Gemini => 2,
        }
    }

    pub fn home_variable(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
            Self::Gemini => "GEMINI_CLI_HOME",
        }
    }

    fn action(self, action: &str) -> Vec<&str> {
        match (self, action) {
            (Self::Claude, "login") => vec!["auth", "login", "--claudeai"],
            (Self::Claude, "status") => vec!["auth", "status", "--json"],
            (Self::Claude, _) => vec!["auth", action],
            (Self::Codex, "status") => vec!["login", "status"],
            (Self::Codex, _) => vec![action],
            (Self::Gemini, _) => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileSource {
    SystemDefault,
    Isolated {
        root: PathBuf,
        name: String,
        #[serde(default)]
        env_overrides: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthDetection {
    pub provider: Provider,
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub account: Option<String>,
    pub tier: Option<String>,
    pub api_provider: Option<String>,
    pub config_dir: Option<PathBuf>,
    pub warning: Option<String>,
}

impl AuthDetection {
    fn signed_out(provider: Provider, config_dir: Option<PathBuf>) -> Self {
        Self {
            provider,
            logged_in: false,
            auth_method: None,
            account: None,
            tier: None,
            api_provider: None,
            config_dir,
            warning: None,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

const MAX_NAME_BYTES: usize = 64;
const NAME_PUNCTUATION: &[u8] = b"._@+-";
const WINDOWS_RESERVED_NAMES: [&str; 24] = [
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
    "com8", "com9", "lpt0", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || NAME_PUNCTUATION.contains(&byte)
}

fn windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    WINDOWS_RESERVED_NAMES.contains(&stem)
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=MAX_NAME_BYTES).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && !name.ends_with('.')
        && !name.contains("..")
        && bytes.iter().copied().all(name_byte)
        && !windows_reserved(name)
}

pub(crate) fn normalized_name(account: &str) -> Option<String> {
    let lowered: String = account
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            c if c.is_ascii() && name_byte(c as u8) => c,
            _ => '-',
        })
        .collect();
    let mut name = lowered.trim_matches(|c| c == '.' || c == '-').to_owned();
    while name.contains("..") {
        name = name.replace("..", ".");
    }
    name.truncate(MAX_NAME_BYTES);
    let name = name.trim_end_matches('.').to_owned();
    valid_name(&name).then_some(name)
}

fn home() -> io::Result<PathBuf> {
    env::home_dir().ok_or_else(|| invalid("No home directory. Set an absolute SUB_AUTH_DIR."))
}

pub fn root() -> io::Result<PathBuf> {
    let path = match env::var_os("SUB_AUTH_DIR") {
        Some(value) => {
            let path = PathBuf::from(value);
            if let Ok(rest) = path.strip_prefix("~") {
                home()?.join(rest)
            } else {
                path
            }
        }
        None => home()?.join(".local/share/sub-auth"),
    };
    if !path.is_absolute() {
        return Err(invalid("SUB_AUTH_DIR must be an absolute path"));
    }
    Ok(path)
}

pub(crate) fn private_state_directory(root: &Path, name: &str) -> io::Result<PathBuf> {
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    privacy::create(root)?;
    let directory = root.join(name);
    privacy::create(&directory)?;
    Ok(directory)
}

pub(crate) fn existing_private_state(root: &Path, name: &str) -> io::Result<Option<PathBuf>> {
    let directory = root.join(name);
    Ok(optional_private(&directory)?.then_some(directory))
}

fn optional_private(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            privacy::verify(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub provider: Provider,
    name: String,
    root: PathBuf,
    source: ProfileSource,
    #[cfg(test)]
    executable: Option<PathBuf>,
    #[cfg(test)]
    system_home: Option<PathBuf>,
}

impl fmt::Display for Profile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider.binary(), self.name)
    }
}

fn nested_session_guard(key: &str) -> bool {
    matches!(
        key.to_ascii_uppercase().as_str(),
        "CLAUDECODE" | "CLAUDE_SESSION_ID" | "CLAUDE_CODE_ENTRYPOINT" | "CODEX_THREAD_ID"
    )
}

fn inherited_provider_variable(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "OPENAI_",
        "ANTHROPIC_",
        "CLAUDE_CODE_",
        "CODEX_",
        "CHATGPT_",
        "GEMINI_",
        "GOOGLE_GENAI_",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
        || matches!(
            upper.as_str(),
            "CLAUDECODE" | "CLAUDE_CONFIG_DIR" | "CLAUDE_SESSION_ID" | "GOOGLE_API_KEY"
        )
}

fn codex_override_disables_chatgpt_force(env_overrides: &BTreeMap<String, String>) -> bool {
    env_overrides.keys().any(|key| {
        matches!(
            key.to_ascii_uppercase().as_str(),
            "OPENAI_API_KEY" | "CODEX_API_KEY" | "OPENAI_BASE_URL" | "CODEX_FORCED_LOGIN_METHOD"
        )
    })
}

fn is_non_secret_override_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    !upper.contains("KEY")
        && !upper.contains("TOKEN")
        && !upper.contains("SECRET")
        && !upper.contains("PASSWORD")
}

fn load_isolated_env(directory: &Path) -> BTreeMap<String, String> {
    let Ok(bytes) = fs::read(directory.join(ISOLATED_ENV_FILE)) else {
        return BTreeMap::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_isolated_env(directory: &Path, overrides: &BTreeMap<String, String>) -> io::Result<()> {
    let safe: BTreeMap<&String, &String> = overrides
        .iter()
        .filter(|(key, _)| is_non_secret_override_key(key))
        .collect();
    if safe.is_empty() {
        return Ok(());
    }
    let path = directory.join(ISOLATED_ENV_FILE);
    let temp = directory.join(format!("{ISOLATED_ENV_FILE}.{}.next", uuid::Uuid::new_v4()));
    let mut file = fs::File::create_new(&temp)?;
    serde_json::to_writer(&mut file, &safe)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)
}

impl Profile {
    pub fn system_default(state_root: PathBuf, provider: Provider) -> Self {
        Self {
            provider,
            name: SYSTEM_DEFAULT_NAME.into(),
            root: state_root,
            source: ProfileSource::SystemDefault,
            #[cfg(test)]
            executable: None,
            #[cfg(test)]
            system_home: None,
        }
    }

    pub fn new(root: PathBuf, provider: Provider, name: &str) -> Result<Self, &'static str> {
        Self::isolated(root, provider, name, BTreeMap::new())
    }

    pub fn isolated(
        root: PathBuf,
        provider: Provider,
        name: &str,
        env_overrides: BTreeMap<String, String>,
    ) -> Result<Self, &'static str> {
        validate_name(name)?;
        Ok(Self {
            provider,
            name: name.into(),
            root: root.clone(),
            source: ProfileSource::Isolated {
                root,
                name: name.into(),
                env_overrides,
            },
            #[cfg(test)]
            executable: None,
            #[cfg(test)]
            system_home: None,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn source(&self) -> &ProfileSource {
        &self.source
    }

    pub fn is_system_default(&self) -> bool {
        matches!(self.source, ProfileSource::SystemDefault)
    }

    pub fn env_overrides(&self) -> Option<&BTreeMap<String, String>> {
        match &self.source {
            ProfileSource::SystemDefault => None,
            ProfileSource::Isolated { env_overrides, .. } => Some(env_overrides),
        }
    }

    pub fn prepare_isolated_with_env(&self) -> io::Result<PathBuf> {
        let directory = self.directory(true)?;
        if let ProfileSource::Isolated { env_overrides, .. } = &self.source {
            save_isolated_env(&directory, env_overrides)?;
        }
        Ok(directory)
    }

    pub fn detect_passive(&self) -> AuthDetection {
        let env_map = current_env_map(self.env_overrides());
        match &self.source {
            ProfileSource::SystemDefault => {
                let home_dir = self.effective_home();
                detect_provider_passive(self.provider, home_dir.as_deref(), None, &env_map)
            }
            ProfileSource::Isolated { .. } => {
                let dir = self.root.join(self.provider.binary()).join(&self.name);
                detect_provider_passive(self.provider, Some(&dir), Some(&dir), &env_map)
            }
        }
    }

    pub fn signed_in(&self) -> bool {
        self.authenticated_command().is_ok()
    }

    fn effective_home(&self) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(home) = &self.system_home {
            return Some(home.clone());
        }
        env::home_dir()
    }

    #[cfg(test)]
    pub(crate) fn prepare(&self) -> io::Result<()> {
        self.directory(true).map(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn without_provider(mut self) -> Self {
        self.executable = Some(PathBuf::from("agent-command-test-absent-provider"));
        self
    }

    #[cfg(all(test, unix))]
    pub(crate) fn fixture(root: &Path, provider: Provider, executable: PathBuf) -> Self {
        Self::named_fixture(root, provider, "fixture", executable)
    }

    #[cfg(test)]
    pub(crate) fn named_fixture(
        root: &Path,
        provider: Provider,
        name: &str,
        executable: PathBuf,
    ) -> Self {
        let profile = Self {
            provider,
            name: name.into(),
            root: root.into(),
            source: ProfileSource::Isolated {
                root: root.into(),
                name: name.into(),
                env_overrides: BTreeMap::new(),
            },
            executable: Some(executable),
            system_home: None,
        };
        profile.directory(true).unwrap();
        profile
    }

    #[cfg(all(test, unix))]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn usage_directory(&self) -> io::Result<PathBuf> {
        if !self.is_system_default() {
            self.directory(false)?;
        } else {
            if let Some(parent) = self.root.parent() {
                fs::create_dir_all(parent)?;
            }
            privacy::create(&self.root)?;
        }
        let usage = self.root.join("agent-command-usage-v1");
        let provider = usage.join(self.provider.binary());
        let profile = provider.join(&self.name);
        for path in [&usage, &provider, &profile] {
            privacy::create(path)?;
        }
        Ok(profile)
    }

    pub(crate) fn checkpoint_directory(&self) -> io::Result<PathBuf> {
        if !self.is_system_default() {
            self.directory(false)?;
        } else {
            if let Some(parent) = self.root.parent() {
                fs::create_dir_all(parent)?;
            }
            privacy::create(&self.root)?;
        }
        let directory = self.root.join(crate::checkpoint::DIRECTORY);
        privacy::create(&directory)?;
        Ok(directory)
    }

    pub(crate) fn isolated_command(&self) -> io::Result<(Command, PathBuf)> {
        match &self.source {
            ProfileSource::SystemDefault => {
                let dir = self.effective_home().unwrap_or_else(|| self.root.clone());
                Ok((self.command(&dir), dir))
            }
            ProfileSource::Isolated { .. } => {
                let directory = self.directory(false)?;
                Ok((self.command(&directory), directory))
            }
        }
    }

    fn directory(&self, create: bool) -> io::Result<PathBuf> {
        if self.is_system_default() {
            return self
                .effective_home()
                .ok_or_else(|| invalid("No home directory for system default profile"));
        }
        let parent = self.root.join(self.provider.binary());
        let directory = parent.join(&self.name);
        if create {
            if let Some(parent) = self.root.parent() {
                fs::create_dir_all(parent)?;
            }
            for path in [&self.root, &parent, &directory] {
                privacy::create(path)?;
            }
        } else {
            for path in [&self.root, &parent, &directory] {
                privacy::verify(path)?;
            }
        }
        Ok(directory)
    }

    pub(crate) fn command(&self, directory: &Path) -> Command {
        let mut command = Command::new(program(self.provider));
        #[cfg(test)]
        if let Some(executable) = &self.executable {
            command = Command::new(executable);
        }
        match &self.source {
            ProfileSource::SystemDefault => {
                for (key, _) in env::vars_os() {
                    if key.to_str().is_some_and(nested_session_guard) {
                        command.env_remove(key);
                    }
                }
            }
            ProfileSource::Isolated { env_overrides, .. } => {
                for (key, _) in env::vars_os() {
                    if key.to_str().is_some_and(inherited_provider_variable) {
                        command.env_remove(key);
                    }
                }
                command.env(self.provider.home_variable(), directory);
                for (key, value) in env_overrides {
                    command.env(key, value);
                }
                if self.provider == Provider::Codex
                    && !codex_override_disables_chatgpt_force(env_overrides)
                {
                    command.args(CODEX_PROFILE_ARGUMENTS);
                }
            }
        }
        command
    }

    fn status(&self, directory: &Path) -> io::Result<Option<std::process::Output>> {
        let action_args = self.provider.action("status");
        if action_args.is_empty() {
            return Ok(None);
        }
        let output = self
            .command(directory)
            .current_dir(directory)
            .args(action_args)
            .stdin(Stdio::null())
            .output()?;
        Ok(output.status.success().then_some(output))
    }

    fn claude_status(&self, directory: &Path) -> io::Result<Option<Value>> {
        let Some(output) = self.status(directory)? else {
            return Ok(None);
        };
        serde_json::from_slice(&output.stdout)
            .map(Some)
            .map_err(|_| invalid("Claude returned an unreadable authentication status"))
    }

    fn active(&self, directory: &Path) -> io::Result<bool> {
        match self.provider {
            Provider::Claude => {
                if self.is_system_default() {
                    let passive = self.detect_passive();
                    if passive.logged_in {
                        return Ok(true);
                    }
                    if !claude_has_cloud_override_env(&current_env_map(None)) {
                        return Ok(false);
                    }
                }
                if let Some(overrides) = self.env_overrides() {
                    if claude_has_cloud_override_env(overrides) {
                        return Ok(true);
                    }
                }
                Ok(self
                    .claude_status(directory)?
                    .is_some_and(|data| claude_status_logged_in(&data)))
            }
            Provider::Codex => {
                if self.is_system_default() {
                    let passive = self.detect_passive();
                    if passive.logged_in {
                        return Ok(true);
                    }
                }
                if let Some(overrides) = self.env_overrides() {
                    if overrides.contains_key("OPENAI_API_KEY")
                        || overrides.contains_key("CODEX_API_KEY")
                    {
                        return Ok(true);
                    }
                }
                Ok(self
                    .status(directory)?
                    .is_some_and(|output| codex_status_logged_in(&output.stdout, &output.stderr)))
            }
            Provider::Gemini => Ok(self.detect_passive().logged_in),
        }
    }

    fn account(&self, directory: &Path) -> Option<String> {
        match self.provider {
            Provider::Claude => claude_account(&self.claude_status(directory).ok()??),
            Provider::Codex => codex_account(&fs::read(directory.join(CODEX_AUTH_FILE)).ok()?),
            Provider::Gemini => self.detect_passive().account,
        }
    }

    pub fn authenticated_command(&self) -> io::Result<Command> {
        let directory = self.directory(false)?;
        if !self.active(&directory)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Sign in first with agent-command auth login {} {}",
                    self.provider.binary(),
                    self.name
                ),
            ));
        }
        Ok(self.command(&directory))
    }
}

pub fn claude_status_logged_in(data: &Value) -> bool {
    if data["loggedIn"] != true {
        return false;
    }
    if let Some(expires) = data.get("loginExpiresAt") {
        if let Some(ms) = expires.as_i64() {
            let now_ms = chrono::Utc::now().timestamp_millis();
            if ms > 0 && ms <= now_ms {
                return false;
            }
        } else if let Some(text) = expires.as_str() {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
                if dt.timestamp() <= chrono::Utc::now().timestamp() {
                    return false;
                }
            }
        }
    }
    true
}

pub fn codex_status_logged_in(stdout: &[u8], stderr: &[u8]) -> bool {
    [stdout, stderr].iter().any(|stream| {
        String::from_utf8_lossy(stream).lines().any(|line| {
            let trimmed = line.trim().to_ascii_lowercase();
            trimmed.starts_with("logged in using ")
        })
    })
}

fn current_env_map(overrides: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = env::vars().collect();
    if let Some(extra) = overrides {
        for (k, v) in extra {
            map.insert(k.clone(), v.clone());
        }
    }
    map
}

fn env_non_empty<'a>(env_map: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    env_map
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn env_is_truthy(env_map: &BTreeMap<String, String>, key: &str) -> bool {
    env_non_empty(env_map, key).is_some_and(|value| {
        value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    })
}

fn claude_has_cloud_override_env(env_map: &BTreeMap<String, String>) -> bool {
    env_non_empty(env_map, "ANTHROPIC_API_KEY").is_some()
        || env_non_empty(env_map, "ANTHROPIC_AUTH_TOKEN").is_some()
        || env_is_truthy(env_map, "CLAUDE_CODE_USE_BEDROCK")
        || env_is_truthy(env_map, "CLAUDE_CODE_USE_VERTEX")
        || env_is_truthy(env_map, "CLAUDE_CODE_USE_FOUNDRY")
}

pub fn detect_system_defaults() -> Vec<AuthDetection> {
    let home_dir = env::home_dir();
    let env_map = current_env_map(None);
    Provider::ALL
        .into_iter()
        .map(|provider| detect_provider_passive(provider, home_dir.as_deref(), None, &env_map))
        .collect()
}

pub fn all_profiles(root: &Path) -> io::Result<Vec<Profile>> {
    let mut combined = Vec::new();
    for detection in detect_system_defaults() {
        if detection.logged_in {
            combined.push(Profile::system_default(
                root.to_path_buf(),
                detection.provider,
            ));
        }
    }
    combined.extend(profiles(root)?);
    Ok(combined)
}

pub fn detect_provider_passive(
    provider: Provider,
    home_dir: Option<&Path>,
    isolated_dir: Option<&Path>,
    env_map: &BTreeMap<String, String>,
) -> AuthDetection {
    match provider {
        Provider::Claude => detect_claude_passive(home_dir, isolated_dir, env_map),
        Provider::Codex => detect_codex_passive(home_dir, isolated_dir, env_map),
        Provider::Gemini => detect_gemini_passive(home_dir, isolated_dir, None, env_map),
    }
}

pub fn detect_claude_passive(
    home_dir: Option<&Path>,
    isolated_dir: Option<&Path>,
    env_map: &BTreeMap<String, String>,
) -> AuthDetection {
    let config_dir = isolated_dir
        .map(Path::to_path_buf)
        .or_else(|| env_non_empty(env_map, "CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| home_dir.map(|h| h.join(".claude")));

    if env_is_truthy(env_map, "CLAUDE_CODE_USE_BEDROCK") {
        return AuthDetection {
            provider: Provider::Claude,
            logged_in: true,
            auth_method: Some("bedrock".into()),
            account: env_non_empty(env_map, "AWS_PROFILE").map(str::to_owned),
            tier: Some("bedrock".into()),
            api_provider: Some("bedrock".into()),
            config_dir,
            warning: None,
        };
    }
    if env_is_truthy(env_map, "CLAUDE_CODE_USE_VERTEX") {
        return AuthDetection {
            provider: Provider::Claude,
            logged_in: true,
            auth_method: Some("vertex".into()),
            account: env_non_empty(env_map, "ANTHROPIC_VERTEX_PROJECT_ID").map(str::to_owned),
            tier: Some("vertex".into()),
            api_provider: Some("vertex".into()),
            config_dir,
            warning: None,
        };
    }
    if env_is_truthy(env_map, "CLAUDE_CODE_USE_FOUNDRY") {
        return AuthDetection {
            provider: Provider::Claude,
            logged_in: true,
            auth_method: Some("foundry".into()),
            account: env_non_empty(env_map, "ANTHROPIC_FOUNDRY_RESOURCE").map(str::to_owned),
            tier: Some("foundry".into()),
            api_provider: Some("foundry".into()),
            config_dir,
            warning: None,
        };
    }
    if env_non_empty(env_map, "ANTHROPIC_API_KEY").is_some()
        || env_non_empty(env_map, "ANTHROPIC_AUTH_TOKEN").is_some()
    {
        return AuthDetection {
            provider: Provider::Claude,
            logged_in: true,
            auth_method: Some("api_key".into()),
            account: env_non_empty(env_map, "ANTHROPIC_BASE_URL").map(str::to_owned),
            tier: Some("api".into()),
            api_provider: Some("firstParty".into()),
            config_dir,
            warning: None,
        };
    }

    let mut account: Option<String> = None;
    let mut tier: Option<String> = None;
    let mut logged_in = false;
    let mut auth_method: Option<String> = None;

    if let Some(dir) = &config_dir {
        if let Some(creds) = read_json(&dir.join(".credentials.json")) {
            let oauth = &creds["claudeAiOauth"];
            let has_token = oauth["accessToken"]
                .as_str()
                .is_some_and(|t| !t.trim().is_empty())
                || oauth["refreshToken"]
                    .as_str()
                    .is_some_and(|t| !t.trim().is_empty());
            if has_token {
                logged_in = true;
                auth_method = Some("oauth_token".into());
                tier = extract_claude_tier(oauth);
            }
        }
    }

    let mut state_candidates = Vec::new();
    if let Some(dir) = &config_dir {
        state_candidates.push(dir.join(".claude.json"));
    }
    if isolated_dir.is_none() {
        if let Some(home) = home_dir {
            state_candidates.push(home.join(".claude.json"));
        }
    }
    for path in state_candidates {
        let Some(state) = read_json(&path) else {
            continue;
        };
        let oauth_account = &state["oauthAccount"];
        if oauth_account.is_object() {
            if account.is_none() {
                account = first_text(
                    oauth_account,
                    &["emailAddress", "email", "organizationName", "displayName"],
                );
            }
            if tier.is_none() {
                tier = extract_claude_tier(oauth_account);
            }
            if account.is_some()
                || oauth_account["accountUuid"]
                    .as_str()
                    .is_some_and(|u| !u.trim().is_empty())
            {
                logged_in = true;
                if auth_method.is_none() {
                    auth_method = Some("oauth_token".into());
                }
            }
        }
        if !logged_in
            && state["primaryApiKey"]
                .as_str()
                .is_some_and(|key| !key.trim().is_empty())
        {
            logged_in = true;
            auth_method = Some("api_key".into());
            tier = Some("api".into());
        }
        if logged_in {
            break;
        }
    }

    AuthDetection {
        provider: Provider::Claude,
        logged_in,
        auth_method,
        account,
        tier,
        api_provider: logged_in.then(|| "firstParty".into()),
        config_dir,
        warning: None,
    }
}

fn extract_claude_tier(value: &Value) -> Option<String> {
    let raw = first_text(
        value,
        &[
            "subscriptionType",
            "rateLimitTier",
            "billingType",
            "organizationRole",
        ],
    )?;
    let lower = raw.to_ascii_lowercase();
    if lower.contains("max") {
        Some("max".into())
    } else if lower.contains("enterprise") {
        Some("enterprise".into())
    } else if lower.contains("team") {
        Some("team".into())
    } else if lower.contains("pro") {
        Some("pro".into())
    } else {
        Some(raw)
    }
}

pub fn detect_codex_passive(
    home_dir: Option<&Path>,
    isolated_dir: Option<&Path>,
    env_map: &BTreeMap<String, String>,
) -> AuthDetection {
    let codex_home = isolated_dir
        .map(Path::to_path_buf)
        .or_else(|| env_non_empty(env_map, "CODEX_HOME").map(PathBuf::from))
        .or_else(|| home_dir.map(|h| h.join(".codex")));

    if let Some(dir) = &codex_home {
        if let Ok(bytes) = fs::read(dir.join(CODEX_AUTH_FILE)) {
            if let Ok(auth) = serde_json::from_slice::<Value>(&bytes) {
                let has_oauth = auth["tokens"]["access_token"]
                    .as_str()
                    .is_some_and(|v| !v.trim().is_empty())
                    || auth["tokens"]["refresh_token"]
                        .as_str()
                        .is_some_and(|v| !v.trim().is_empty())
                    || auth["tokens"]["id_token"]
                        .as_str()
                        .is_some_and(|v| !v.trim().is_empty());
                if has_oauth {
                    return AuthDetection {
                        provider: Provider::Codex,
                        logged_in: true,
                        auth_method: Some("chatgpt".into()),
                        account: codex_account(&bytes),
                        tier: codex_plan_tier(&bytes),
                        api_provider: Some("openai".into()),
                        config_dir: codex_home,
                        warning: None,
                    };
                }
                if auth["OPENAI_API_KEY"]
                    .as_str()
                    .is_some_and(|v| !v.trim().is_empty())
                {
                    return AuthDetection {
                        provider: Provider::Codex,
                        logged_in: true,
                        auth_method: Some("api_key".into()),
                        account: None,
                        tier: Some("api".into()),
                        api_provider: Some("openai".into()),
                        config_dir: codex_home,
                        warning: None,
                    };
                }
            }
        }
    }

    if env_non_empty(env_map, "OPENAI_API_KEY").is_some()
        || env_non_empty(env_map, "CODEX_API_KEY").is_some()
    {
        return AuthDetection {
            provider: Provider::Codex,
            logged_in: true,
            auth_method: Some("api_key".into()),
            account: None,
            tier: Some("api".into()),
            api_provider: Some("openai".into()),
            config_dir: codex_home,
            warning: None,
        };
    }

    AuthDetection::signed_out(Provider::Codex, codex_home)
}

pub fn detect_gemini_passive(
    home_dir: Option<&Path>,
    isolated_dir: Option<&Path>,
    workspace: Option<&Path>,
    env_map: &BTreeMap<String, String>,
) -> AuthDetection {
    let base_home = isolated_dir
        .map(Path::to_path_buf)
        .or_else(|| env_non_empty(env_map, "GEMINI_CLI_HOME").map(PathBuf::from))
        .or_else(|| home_dir.map(Path::to_path_buf));
    let config_dir = base_home.as_ref().map(|base| base.join(".gemini"));

    let mut effective_env = load_gemini_dotenv(workspace, config_dir.as_deref());
    for (key, value) in env_map {
        effective_env.insert(key.clone(), value.clone());
    }

    let settings = config_dir
        .as_ref()
        .and_then(|dir| read_json(&dir.join("settings.json")));
    let selected_type = settings
        .as_ref()
        .and_then(|s| s["security"]["auth"]["selectedType"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let enforced_type = settings
        .as_ref()
        .and_then(|s| s["security"]["auth"]["enforcedType"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    if let (Some(enforced), Some(selected)) = (&enforced_type, &selected_type) {
        if !enforced.eq_ignore_ascii_case(selected) {
            return AuthDetection {
                provider: Provider::Gemini,
                logged_in: false,
                auth_method: Some(selected.clone()),
                account: None,
                tier: None,
                api_provider: Some("google".into()),
                config_dir,
                warning: Some(format!(
                    "Gemini policy enforces auth type `{enforced}`, but settings.json selects `{selected}`."
                )),
            };
        }
    }

    let auth_type = selected_type
        .or(enforced_type)
        .or_else(|| gemini_auth_type_from_env(&effective_env));

    let Some(auth_type) = auth_type else {
        return AuthDetection::signed_out(Provider::Gemini, config_dir);
    };

    let active_email = config_dir
        .as_ref()
        .and_then(|dir| read_json(&dir.join("google_accounts.json")))
        .and_then(|accounts| {
            accounts["active"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        });

    let (logged_in, tier) = match auth_type.as_str() {
        "oauth-personal" | "login-with-google" => {
            let valid_creds = config_dir
                .as_ref()
                .and_then(|dir| read_json(&dir.join("oauth_creds.json")))
                .is_some_and(|creds| {
                    creds["access_token"]
                        .as_str()
                        .is_some_and(|v| !v.trim().is_empty())
                        || creds["refresh_token"]
                            .as_str()
                            .is_some_and(|v| !v.trim().is_empty())
                        || creds["id_token"]
                            .as_str()
                            .is_some_and(|v| !v.trim().is_empty())
                });
            let tier = if env_non_empty(&effective_env, "GOOGLE_CLOUD_PROJECT").is_some()
                || env_non_empty(&effective_env, "GOOGLE_CLOUD_PROJECT_ID").is_some()
            {
                Some("workspace".into())
            } else {
                Some("google".into())
            };
            (valid_creds, tier)
        }
        "gemini-api-key" => (
            env_non_empty(&effective_env, "GEMINI_API_KEY").is_some(),
            Some("api".into()),
        ),
        "vertex-ai" => {
            let express = env_non_empty(&effective_env, "GOOGLE_API_KEY").is_some();
            let project_and_loc = env_non_empty(&effective_env, "GOOGLE_CLOUD_PROJECT").is_some()
                && env_non_empty(&effective_env, "GOOGLE_CLOUD_LOCATION").is_some();
            let has_adc = env_non_empty(&effective_env, "GOOGLE_APPLICATION_CREDENTIALS")
                .is_some_and(|p| Path::new(p).is_file())
                || default_gcloud_adc_exists(home_dir, &effective_env);
            (
                express || (project_and_loc && has_adc),
                Some("vertex".into()),
            )
        }
        "gateway" => (
            env_non_empty(&effective_env, "GOOGLE_GEMINI_BASE_URL").is_some(),
            Some("gateway".into()),
        ),
        "compute-default-credentials" => (
            env_is_truthy(&effective_env, "CLOUD_SHELL")
                || env_is_truthy(&effective_env, "GEMINI_CLI_USE_COMPUTE_ADC"),
            Some("adc".into()),
        ),
        _ => (false, None),
    };

    AuthDetection {
        provider: Provider::Gemini,
        logged_in,
        auth_method: Some(auth_type),
        account: active_email
            .or_else(|| env_non_empty(&effective_env, "GOOGLE_CLOUD_PROJECT").map(str::to_owned)),
        tier: logged_in.then_some(tier).flatten(),
        api_provider: Some("google".into()),
        config_dir,
        warning: None,
    }
}

pub fn gemini_auth_type_from_env(env_map: &BTreeMap<String, String>) -> Option<String> {
    if env_is_truthy(env_map, "GOOGLE_GENAI_USE_GCA") {
        return Some("oauth-personal".into());
    }
    if env_is_truthy(env_map, "GOOGLE_GENAI_USE_VERTEXAI") {
        return Some("vertex-ai".into());
    }
    if env_non_empty(env_map, "GOOGLE_GEMINI_BASE_URL").is_some() {
        return Some("gateway".into());
    }
    if env_non_empty(env_map, "GEMINI_API_KEY").is_some() {
        return Some("gemini-api-key".into());
    }
    if env_is_truthy(env_map, "CLOUD_SHELL") || env_is_truthy(env_map, "GEMINI_CLI_USE_COMPUTE_ADC")
    {
        return Some("compute-default-credentials".into());
    }
    None
}

fn default_gcloud_adc_exists(home_dir: Option<&Path>, env_map: &BTreeMap<String, String>) -> bool {
    if let Some(appdata) = env_non_empty(env_map, "APPDATA") {
        let win_adc = Path::new(appdata)
            .join("gcloud")
            .join("application_default_credentials.json");
        if win_adc.is_file() {
            return true;
        }
    }
    if let Some(home) = home_dir {
        let unix_adc = home
            .join(".config")
            .join("gcloud")
            .join("application_default_credentials.json");
        if unix_adc.is_file() {
            return true;
        }
    }
    false
}

fn load_gemini_dotenv(
    workspace: Option<&Path>,
    config_dir: Option<&Path>,
) -> BTreeMap<String, String> {
    if let Some(start) = workspace {
        for ancestor in start.ancestors() {
            let candidate = ancestor.join(".gemini").join(".env");
            if candidate.is_file() {
                return parse_dotenv(&candidate);
            }
        }
    }
    if let Some(dir) = config_dir {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            return parse_dotenv(&candidate);
        }
    }
    BTreeMap::new()
}

fn parse_dotenv(path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut map = BTreeMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let val = raw_value.trim();
        let unquoted = if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            &val[1..val.len() - 1]
        } else {
            val
        };
        map.insert(key.to_owned(), unquoted.to_owned());
    }
    map
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn profiles(root: &Path) -> io::Result<Vec<Profile>> {
    let mut profiles = Vec::new();
    if !optional_private(root)? {
        return Ok(profiles);
    }
    for provider in Provider::ALL {
        let parent = root.join(provider.binary());
        if !optional_private(&parent)? {
            continue;
        }
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| valid_name(name)) else {
                continue;
            };
            if entry.file_type()?.is_dir() && privacy::verify(&entry.path()).is_ok() {
                let env_overrides = load_isolated_env(&entry.path());
                profiles.push(Profile {
                    root: root.into(),
                    provider,
                    name: name.into(),
                    source: ProfileSource::Isolated {
                        root: root.into(),
                        name: name.into(),
                        env_overrides,
                    },
                    #[cfg(test)]
                    executable: None,
                    #[cfg(test)]
                    system_home: None,
                });
            }
        }
    }
    profiles.sort_by(|left, right| {
        left.provider
            .binary()
            .cmp(right.provider.binary())
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(profiles)
}

pub(crate) const NAME_RULE: &str = "Use a profile name of 1–64 lowercase letters, digits, _ or -, starting with a letter or digit. Windows device names are reserved.";

pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(NAME_RULE)
    }
}

pub(crate) fn adopt(provider: Provider, executable: &Path) {
    ADOPTED.lock().unwrap_or_else(PoisonError::into_inner)[provider.slot()] =
        Some(executable.to_path_buf());
}

fn adopted(provider: Provider) -> Option<PathBuf> {
    ADOPTED.lock().unwrap_or_else(PoisonError::into_inner)[provider.slot()].clone()
}

fn program(provider: Provider) -> OsString {
    program_for(provider, adopted(provider).or_else(|| runnable(provider)))
}

fn program_for(provider: Provider, resolved: Option<PathBuf>) -> OsString {
    resolved.map_or_else(|| provider.binary().into(), PathBuf::into_os_string)
}

fn runnable(provider: Provider) -> Option<PathBuf> {
    resolve(provider.binary()).or_else(|| documented(provider))
}

fn documented(provider: Provider) -> Option<PathBuf> {
    crate::install::documented_executable(provider, crate::install::Platform::current(), |name| {
        env::var_os(name)
    })
    .filter(|path| path.is_file())
}

pub(crate) fn found(provider: Provider) -> Option<Found> {
    if let Some(executable) = adopted(provider).filter(|path| path.is_file()) {
        return Some(Found::At(executable));
    }
    resolve(provider.binary()).map(|_| Found::OnPath)
}

pub(crate) fn program_on_path(binary: &str) -> bool {
    resolve(binary).is_some()
}

pub fn resolve(binary: &str) -> Option<PathBuf> {
    resolve_in(binary, &env::var_os("PATH")?, cfg!(windows))
}

fn resolve_in(binary: &str, path: &OsStr, windows: bool) -> Option<PathBuf> {
    let names = candidate_names(binary, windows);
    env::split_paths(path)
        .flat_map(|directory| {
            names
                .iter()
                .map(|name| directory.join(name))
                .collect::<Vec<_>>()
        })
        .find(|candidate| candidate.is_file())
}

fn candidate_names(binary: &str, windows: bool) -> Vec<String> {
    if windows {
        let mut names = Vec::with_capacity(WINDOWS_EXECUTABLE_EXTENSIONS.len() + 1);
        if Path::new(binary).extension().is_some() {
            names.push(binary.to_owned());
        }
        names.extend(
            WINDOWS_EXECUTABLE_EXTENSIONS
                .iter()
                .map(|extension| format!("{binary}{extension}")),
        );
        names
    } else {
        vec![binary.to_owned()]
    }
}

fn session(profile: &Profile, action: &str, stdio: fn() -> Stdio) -> io::Result<ExitStatus> {
    let directory = profile.directory(action == "login")?;
    profile
        .command(&directory)
        .current_dir(&directory)
        .args(profile.provider.action(action))
        .stdin(stdio())
        .stdout(stdio())
        .stderr(stdio())
        .status()
}

pub(crate) fn sign_in(profile: &Profile) -> io::Result<ExitStatus> {
    session(profile, "login", Stdio::inherit)
}

pub(crate) fn sign_out(profile: &Profile) -> io::Result<ExitStatus> {
    session(profile, "logout", Stdio::inherit)
}

pub(crate) fn sign_out_quietly(profile: &Profile) -> io::Result<ExitStatus> {
    session(profile, "logout", Stdio::null)
}

pub(crate) fn disconnect(profile: &Profile) -> io::Result<()> {
    if profile.is_system_default() {
        return Err(invalid(
            "System default logins are managed by their own CLI and cannot be deleted by disconnect.",
        ));
    }
    let directory = profile.removable_directory()?;
    let _ = sign_out_quietly(profile);
    fs::remove_dir_all(&directory)
}

impl Profile {
    fn removable_directory(&self) -> io::Result<PathBuf> {
        validate_name(&self.name).map_err(invalid)?;
        let parent = self.root.join(self.provider.binary());
        let directory = parent.join(&self.name);
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("No {self} profile to disconnect"),
                ))
            }
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            return Err(invalid("A profile must be a real directory, not a link"));
        }
        let inside = fs::canonicalize(&directory)?.parent() == Some(&fs::canonicalize(&parent)?);
        if inside {
            Ok(directory)
        } else {
            Err(invalid("A profile must stay inside the profile root"))
        }
    }
}

pub(crate) fn succeeded(profile: &Profile, status: ExitStatus) -> io::Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            profile.provider.binary()
        )))
    }
}

fn first_text(data: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| data[*key].as_str())
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(crate) fn claude_account(status: &Value) -> Option<String> {
    first_text(status, &["email", "orgName"])
}

pub(crate) fn codex_account(auth_json: &[u8]) -> Option<String> {
    let claims = codex_id_claims(auth_json)?;
    first_text(&claims, &["email", "name"])
}

pub fn codex_plan_tier(auth_json: &[u8]) -> Option<String> {
    let claims = codex_id_claims(auth_json)?;
    claims["https://api.openai.com/auth"]["chatgpt_plan_type"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn codex_id_claims(auth_json: &[u8]) -> Option<Value> {
    let auth: Value = serde_json::from_slice(auth_json).ok()?;
    let payload = auth["tokens"]["id_token"].as_str()?.split('.').nth(1)?;
    serde_json::from_slice(&base64url_decode(payload)?).ok()
}

fn base64url_value(byte: u8) -> Option<u32> {
    let value = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'-' | b'+' => 62,
        b'_' | b'/' => 63,
        _ => return None,
    };
    Some(u32::from(value))
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.trim_end_matches('=').bytes() {
        buffer = (buffer << 6) | base64url_value(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(decoded)
}

pub(crate) fn chosen_name(
    root: &Path,
    provider: Provider,
    requested: Option<&str>,
    account: Option<&str>,
) -> Result<String, &'static str> {
    if let Some(requested) = requested {
        validate_name(requested)?;
        return Ok(requested.into());
    }
    if let Some(name) = account.and_then(normalized_name) {
        return Ok(name);
    }
    let taken = |name: &str| root.join(provider.binary()).join(name).exists();
    Ok(std::iter::once(FALLBACK_PROFILE_NAME.to_owned())
        .chain((2..).map(|number| format!("account-{number}")))
        .find(|name| !taken(name))
        .expect("an unused fallback name exists"))
}

struct Staging {
    profile: Profile,
    directory: PathBuf,
}

impl Staging {
    fn create(template: &Profile) -> io::Result<Self> {
        let parent = template.root.join(template.provider.binary());
        if let Some(above) = template.root.parent() {
            fs::create_dir_all(above)?;
        }
        privacy::create(&template.root)?;
        privacy::create(&parent)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let name = format!("{STAGING_PREFIX}{}-{stamp}", std::process::id());
        let directory = parent.join(&name);
        privacy::create(&directory)?;
        let profile = Profile {
            name: name.clone(),
            source: ProfileSource::Isolated {
                root: template.root.clone(),
                name,
                env_overrides: template.env_overrides().cloned().unwrap_or_default(),
            },
            ..template.clone()
        };
        Ok(Self { profile, directory })
    }

    fn sign_in(&self, stdio: fn() -> Stdio) -> io::Result<()> {
        let status = self
            .profile
            .command(&self.directory)
            .current_dir(&self.directory)
            .args(self.profile.provider.action("login"))
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio())
            .status()?;
        succeeded(&self.profile, status)?;
        if self.profile.active(&self.directory)? {
            Ok(())
        } else {
            Err(io::Error::other("sign-in did not complete"))
        }
    }

    fn promote(self, name: String) -> io::Result<Profile> {
        let target = self.directory.with_file_name(&name);
        if fs::symlink_metadata(&target).is_ok() {
            privacy::verify(&target)?;
            fs::remove_dir_all(&target)?;
        }
        fs::rename(&self.directory, &target)?;
        let env_overrides = self.profile.env_overrides().cloned().unwrap_or_default();
        Ok(Profile {
            name: name.clone(),
            source: ProfileSource::Isolated {
                root: self.profile.root.clone(),
                name,
                env_overrides,
            },
            ..self.profile.clone()
        })
    }

    fn discard(&self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

pub(crate) fn connect(
    root: &Path,
    provider: Provider,
    requested: Option<&str>,
) -> io::Result<Profile> {
    let template = Profile::new(root.into(), provider, FALLBACK_PROFILE_NAME).map_err(invalid)?;
    enroll(&template, requested, Stdio::inherit)
}

fn enroll(
    template: &Profile,
    requested: Option<&str>,
    stdio: fn() -> Stdio,
) -> io::Result<Profile> {
    if let Some(requested) = requested {
        validate_name(requested).map_err(invalid)?;
    }
    let staging = Staging::create(template)?;
    let named = staging.sign_in(stdio).and_then(|()| {
        let account = staging.profile.account(&staging.directory);
        chosen_name(
            &template.root,
            template.provider,
            requested,
            account.as_deref(),
        )
        .map_err(invalid)
    });
    match named {
        Ok(name) => {
            let directory = staging.directory.clone();
            staging.promote(name).inspect_err(|_| {
                let _ = fs::remove_dir_all(directory);
            })
        }
        Err(error) => {
            staging.discard();
            Err(error)
        }
    }
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    if matches!(args, [] | [_])
        && args
            .first()
            .is_none_or(|value| value == "--help" || value == "-h")
    {
        println!("agent-command auth list [--all]\nagent-command auth detect [--json]\nagent-command auth login {{claude|codex|gemini}} [PROFILE]\nagent-command auth {{status|logout|run}} {{claude|codex|gemini}} PROFILE [-- CLI_ARGS...]\nagent-command auth disconnect {{claude|codex|gemini}} PROFILE\nWithout PROFILE, login names the profile from the signed-in account's email or name.\nlogout signs out and keeps the profile folder; disconnect signs out if it can and removes the profile folder.");
        return Ok(());
    }
    let root = root()?;
    if args == ["list"] {
        for profile in profiles(&root)? {
            println!("{} {}", profile.provider.binary(), profile.name);
        }
        return Ok(());
    }
    if args == ["list", "--all"] {
        for profile in all_profiles(&root)? {
            let tag = if profile.is_system_default() {
                " (system-default)"
            } else {
                ""
            };
            println!("{} {}{tag}", profile.provider.binary(), profile.name);
        }
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("detect") {
        let detections = detect_system_defaults();
        if args.iter().any(|arg| arg == "--json") {
            println!("{}", serde_json::to_string_pretty(&detections)?);
        } else {
            for item in &detections {
                let status = if item.logged_in {
                    "signed in"
                } else {
                    "not signed in"
                };
                let detail = item
                    .account
                    .as_deref()
                    .or(item.auth_method.as_deref())
                    .unwrap_or("-");
                println!("{}  {status}  {detail}", item.provider.binary());
            }
        }
        return Ok(());
    }
    if let [action, provider] = args {
        if action == "login" {
            let provider = Provider::named(provider)
                .ok_or_else(|| invalid("Provider must be claude, codex, or gemini"))?;
            let profile = connect(&root, provider, None)?;
            println!("{profile} is signed in");
            return Ok(());
        }
    }
    let [action, provider, name, rest @ ..] = args else {
        return Err(invalid("Use agent-command auth --help"));
    };
    let provider = Provider::named(provider)
        .ok_or_else(|| invalid("Provider must be claude, codex, or gemini"))?;
    let profile = if name == SYSTEM_DEFAULT_NAME && action != "login" && action != "disconnect" {
        Profile::system_default(root, provider)
    } else {
        Profile::new(root, provider, name).map_err(invalid)?
    };
    let status = match (action.as_str(), rest) {
        ("status", []) => {
            profile.authenticated_command()?;
            println!("{profile} is signed in");
            return Ok(());
        }
        ("login", []) => {
            let profile = connect(&profile.root, provider, Some(name))?;
            println!("{profile} is signed in");
            return Ok(());
        }
        ("disconnect", []) => {
            disconnect(&profile)?;
            println!("Disconnected {profile}. Its profile folder is removed.");
            return Ok(());
        }
        ("logout", []) => sign_out(&profile)?,
        ("run", arguments) => profile
            .authenticated_command()?
            .args(
                arguments
                    .strip_prefix(&["--".to_owned()])
                    .unwrap_or(arguments),
            )
            .status()?,
        _ => return Err(invalid("Use agent-command auth --help")),
    };
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!("ac-auth-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn directory(&self, name: &str) -> PathBuf {
            let directory = self.0.join(name);
            fs::create_dir_all(&directory).unwrap();
            directory
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn path_lookup_finds_only_present_executables() {
        let scratch = Scratch::new("path");
        let directory = scratch.directory("bin");
        fs::write(directory.join("fake-cli"), "").unwrap();
        fs::write(directory.join("other-cli.exe"), "").unwrap();
        let path = env::join_paths([directory.clone()]).unwrap();
        assert_eq!(
            resolve_in("fake-cli", &path, false),
            Some(directory.join("fake-cli"))
        );
        assert_eq!(resolve_in("absent-cli", &path, false), None);
        assert_eq!(
            resolve_in("other-cli", &path, true),
            Some(directory.join("other-cli.exe"))
        );
    }

    #[test]
    fn windows_resolution_is_path_directory_major_and_ignores_extensionless_shims() {
        let scratch = Scratch::new("windows-order");
        let first = scratch.directory("first");
        let second = scratch.directory("second");
        fs::write(first.join("codex"), "#!/bin/sh\n").unwrap();
        let path = env::join_paths([first.clone(), second.clone()]).unwrap();
        assert_eq!(resolve_in("codex", &path, true), None);

        fs::write(second.join("codex.exe"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(second.join("codex.exe"))
        );

        fs::write(first.join("codex.bat"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(first.join("codex.bat"))
        );

        fs::write(first.join("codex.cmd"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(first.join("codex.cmd"))
        );

        fs::write(first.join("codex.com"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(first.join("codex.com"))
        );

        fs::write(first.join("codex.exe"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(first.join("codex.exe"))
        );

        assert_eq!(resolve_in("codex", &path, false), Some(first.join("codex")));
    }

    #[test]
    fn passive_claude_detection_reads_files_and_cloud_envs_without_spawning() {
        let scratch = Scratch::new("passive-claude");
        let home = scratch.directory("home");
        fs::write(
            home.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"dev@acme.test","subscriptionType":"max","accountUuid":"uuid-1"}}"#,
        )
        .unwrap();
        let detected = detect_claude_passive(Some(&home), None, &BTreeMap::new());
        assert!(detected.logged_in);
        assert_eq!(detected.auth_method.as_deref(), Some("oauth_token"));
        assert_eq!(detected.account.as_deref(), Some("dev@acme.test"));
        assert_eq!(detected.tier.as_deref(), Some("max"));

        let mut bedrock_env = BTreeMap::new();
        bedrock_env.insert("CLAUDE_CODE_USE_BEDROCK".into(), "1".into());
        bedrock_env.insert("AWS_PROFILE".into(), "prod-eng".into());
        let bedrock = detect_claude_passive(Some(&home), None, &bedrock_env);
        assert!(bedrock.logged_in);
        assert_eq!(bedrock.auth_method.as_deref(), Some("bedrock"));
        assert_eq!(bedrock.api_provider.as_deref(), Some("bedrock"));

        assert!(claude_status_logged_in(&serde_json::json!({
            "loggedIn": true,
            "authMethod": "oauth_token",
            "loginExpiresAt": null
        })));
        assert!(claude_status_logged_in(&serde_json::json!({
            "loggedIn": true,
            "authMethod": "bedrock"
        })));
        assert!(!claude_status_logged_in(&serde_json::json!({
            "loggedIn": false,
            "authMethod": "oauth_token"
        })));
    }

    #[test]
    fn passive_codex_detection_accepts_chatgpt_api_key_and_workload_identity() {
        let scratch = Scratch::new("passive-codex");
        let home = scratch.directory("home");
        let codex_dir = home.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let claims = "eyJlbWFpbCI6InBpbG90QGV4YW1wbGUudGVzdCIsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ";
        fs::write(codex_dir.join("auth.json"), codex_auth(claims)).unwrap();

        let detected = detect_codex_passive(Some(&home), None, &BTreeMap::new());
        assert!(detected.logged_in);
        assert_eq!(detected.auth_method.as_deref(), Some("chatgpt"));
        assert_eq!(detected.account.as_deref(), Some("pilot@example.test"));
        assert_eq!(detected.tier.as_deref(), Some("pro"));

        assert!(codex_status_logged_in(b"Logged in using ChatGPT\n", b""));
        assert!(codex_status_logged_in(b"Logged in using an API key\n", b""));
        assert!(codex_status_logged_in(
            b"",
            b"Logged in using workload identity\n"
        ));
        assert!(!codex_status_logged_in(b"Not logged in\n", b""));
    }

    #[test]
    fn passive_gemini_detection_follows_settings_oauth_env_precedence_and_enforced_policy() {
        let scratch = Scratch::new("passive-gemini");
        let home = scratch.directory("home");
        let gemini_dir = home.join(".gemini");
        fs::create_dir_all(&gemini_dir).unwrap();
        fs::write(
            gemini_dir.join("settings.json"),
            r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        )
        .unwrap();
        fs::write(
            gemini_dir.join("oauth_creds.json"),
            r#"{"access_token":"tok","refresh_token":"ref"}"#,
        )
        .unwrap();
        fs::write(
            gemini_dir.join("google_accounts.json"),
            r#"{"active":"pilot@gmail.com"}"#,
        )
        .unwrap();

        let detected = detect_gemini_passive(Some(&home), None, None, &BTreeMap::new());
        assert!(detected.logged_in);
        assert_eq!(detected.auth_method.as_deref(), Some("oauth-personal"));
        assert_eq!(detected.account.as_deref(), Some("pilot@gmail.com"));
        assert_eq!(detected.tier.as_deref(), Some("google"));

        let mut env_order = BTreeMap::new();
        env_order.insert("GEMINI_API_KEY".into(), "key".into());
        assert_eq!(
            gemini_auth_type_from_env(&env_order).as_deref(),
            Some("gemini-api-key")
        );
        env_order.insert("GOOGLE_GEMINI_BASE_URL".into(), "https://gw.test".into());
        assert_eq!(
            gemini_auth_type_from_env(&env_order).as_deref(),
            Some("gateway")
        );
        env_order.insert("GOOGLE_GENAI_USE_VERTEXAI".into(), "true".into());
        assert_eq!(
            gemini_auth_type_from_env(&env_order).as_deref(),
            Some("vertex-ai")
        );
        env_order.insert("GOOGLE_GENAI_USE_GCA".into(), "true".into());
        assert_eq!(
            gemini_auth_type_from_env(&env_order).as_deref(),
            Some("oauth-personal")
        );

        fs::write(
            gemini_dir.join("settings.json"),
            r#"{"security":{"auth":{"selectedType":"oauth-personal","enforcedType":"vertex-ai"}}}"#,
        )
        .unwrap();
        let mismatch = detect_gemini_passive(Some(&home), None, None, &BTreeMap::new());
        assert!(!mismatch.logged_in);
        assert!(mismatch.warning.unwrap().contains("enforces auth type"));
    }

    #[test]
    fn system_default_profile_never_forces_codex_chatgpt_or_strips_credentials() {
        let scratch = Scratch::new("sysdefault-cmd");
        let profile = Profile {
            system_home: Some(scratch.0.clone()),
            ..Profile::system_default(scratch.0.join("state"), Provider::Codex)
        };
        let cmd = profile.command(&scratch.0);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg.contains("forced_login_method")));
        assert!(profile.is_system_default());
    }

    #[test]
    fn codex_profile_arguments_avoid_double_quotes() {
        assert_eq!(
            CODEX_PROFILE_ARGUMENTS.join(" "),
            "-c cli_auth_credentials_store='file' -c forced_login_method='chatgpt'"
        );
        assert!(!CODEX_PROFILE_ARGUMENTS
            .iter()
            .any(|argument| argument.contains('"')));
    }

    #[cfg(windows)]
    #[test]
    fn windows_runs_the_resolved_cmd_shim_with_codex_profile_arguments() {
        let scratch = Scratch::new("windows-cmd");
        let npm = scratch.directory("npm");
        let captured = npm.join("arguments.txt");
        fs::write(npm.join("codex"), "#!/bin/sh\nexit 9\n").unwrap();
        fs::write(
            npm.join("codex.cmd"),
            "@echo off\r\necho %*> \"%~dp0arguments.txt\"\r\nexit /b 0\r\n",
        )
        .unwrap();
        let path = env::join_paths([npm.clone()]).unwrap();
        let shim = resolve_in("codex", &path, true).unwrap();
        assert_eq!(shim, npm.join("codex.cmd"));
        let profile = Profile {
            executable: Some(shim),
            ..Profile::new(scratch.0.join("profiles"), Provider::Codex, "main").unwrap()
        };
        let home = scratch.directory("home");
        let status = profile
            .command(&home)
            .args(Provider::Codex.action("status"))
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{status}");
        let arguments = fs::read_to_string(captured).unwrap();
        for expected in [
            "cli_auth_credentials_store='file'",
            "forced_login_method='chatgpt'",
            "login",
            "status",
        ] {
            assert!(arguments.contains(expected), "{expected}: {arguments}");
        }
    }

    #[test]
    fn profile_names_accept_normalized_accounts_and_refuse_unsafe_names() {
        for accepted in [
            "main",
            "work_2",
            "pilot@example.test",
            "pilot+team@example.test",
            "pilot-example",
            "account-2",
        ] {
            assert!(validate_name(accepted).is_ok(), "{accepted:?}");
        }
        let long = "a".repeat(65);
        for refused in [
            "",
            "Main",
            "-main",
            ".main",
            "main.",
            "a..b",
            "../main",
            "a/b",
            "a\\b",
            "has space",
            "tab\tname",
            "con",
            "nul",
            "com1",
            "lpt9",
            "con.txt",
            "aux.example.test",
            long.as_str(),
        ] {
            assert_eq!(validate_name(refused), Err(NAME_RULE), "{refused:?}");
        }
    }

    #[test]
    fn account_names_normalize_into_valid_profile_names() {
        let cases = [
            ("  Pilot@Example.TEST ", Some("pilot@example.test")),
            ("Pilot Example", Some("pilot-example")),
            ("Acme Org!", Some("acme-org")),
            ("..hidden..", Some("hidden")),
            ("con", None),
            ("   ", None),
            ("ÆØÅ", None),
        ];
        for (account, expected) in cases {
            assert_eq!(normalized_name(account).as_deref(), expected, "{account:?}");
        }
        let long = format!("{}@example.test", "p".repeat(80));
        let name = normalized_name(&long).unwrap();
        assert_eq!(name.len(), MAX_NAME_BYTES);
        assert!(valid_name(&name));
    }

    #[test]
    fn claude_status_names_prefer_email_then_organization() {
        let full = serde_json::json!({
            "loggedIn": true,
            "email": "pilot@example.test",
            "orgName": "Example Org",
            "subscriptionType": "max",
        });
        assert_eq!(claude_account(&full).as_deref(), Some("pilot@example.test"));
        let organization = serde_json::json!({"email": "", "orgName": "Example Org"});
        assert_eq!(
            claude_account(&organization).as_deref(),
            Some("Example Org")
        );
        assert_eq!(claude_account(&serde_json::json!({"loggedIn": true})), None);
    }

    fn codex_auth(payload: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "tokens": {"id_token": format!("eyJhbGciOiJub25lIn0.{payload}.signature")}
        }))
        .unwrap()
    }

    const EMAIL_CLAIMS: &str =
        "eyJlbWFpbCI6ICJQaWxvdEBFeGFtcGxlLnRlc3QiLCAibmFtZSI6ICJQaWxvdCBFeGFtcGxlIn0";
    const NAME_CLAIMS: &str = "eyJuYW1lIjogIlBpbG90IEV4YW1wbGUifQ";

    #[test]
    fn codex_id_token_names_prefer_email_then_name() {
        assert_eq!(
            codex_account(&codex_auth(EMAIL_CLAIMS)).as_deref(),
            Some("Pilot@Example.test")
        );
        assert_eq!(
            codex_account(&codex_auth(NAME_CLAIMS)).as_deref(),
            Some("Pilot Example")
        );
        assert_eq!(codex_account(&codex_auth("not*base64")), None);
        assert_eq!(codex_account(b"{}"), None);
        assert_eq!(codex_account(b"not json"), None);
    }

    #[test]
    fn chosen_names_honor_custom_names_then_accounts_then_free_fallbacks() {
        let scratch = Scratch::new("names");
        let root = scratch.0.join("profiles");
        let chosen = |requested, account| chosen_name(&root, Provider::Codex, requested, account);
        assert_eq!(
            chosen(Some("work"), Some("pilot@example.test")).unwrap(),
            "work"
        );
        assert_eq!(chosen(Some("Bad Name"), None), Err(NAME_RULE));
        assert_eq!(
            chosen(None, Some("Pilot@Example.test")).unwrap(),
            "pilot@example.test"
        );
        assert_eq!(chosen(None, None).unwrap(), "main");
        assert_eq!(chosen(None, Some("con")).unwrap(), "main");
        fs::create_dir_all(root.join("codex").join("main")).unwrap();
        assert_eq!(chosen(None, None).unwrap(), "account-2");
        fs::create_dir_all(root.join("codex").join("account-2")).unwrap();
        assert_eq!(chosen(None, None).unwrap(), "account-3");
        assert_eq!(
            chosen_name(&root, Provider::Claude, None, None).unwrap(),
            "main"
        );
    }

    #[test]
    fn an_adopted_executable_replaces_the_bare_provider_name() {
        assert_eq!(program_for(Provider::Codex, None), "codex");
        let executable = env::temp_dir().join("bin").join("claude");
        assert_eq!(
            program_for(Provider::Claude, Some(executable.clone())),
            executable.into_os_string()
        );
    }

    fn absent_provider(root: &Path, provider: Provider, name: &str) -> Profile {
        let profile = Profile::new(root.into(), provider, name)
            .unwrap()
            .without_provider();
        profile.prepare().unwrap();
        profile
    }

    #[test]
    fn disconnect_removes_a_broken_profile_and_keeps_its_siblings() {
        let scratch = Scratch::new("disconnect-broken");
        let root = scratch.0.join("profiles");
        let broken = absent_provider(&root, Provider::Codex, "main");
        absent_provider(&root, Provider::Codex, "work");
        absent_provider(&root, Provider::Claude, "main");
        fs::write(broken.directory(false).unwrap().join("auth.json"), "{}").unwrap();
        assert!(!broken.signed_in());
        disconnect(&broken).unwrap();
        assert!(!root.join("codex/main").exists());
        assert!(root.join("codex/work").is_dir());
        assert!(root.join("claude/main").is_dir());
        let listed: Vec<String> = profiles(&root)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(listed, ["claude/main", "codex/work"]);
        let again = disconnect(&broken).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn disconnect_refuses_names_that_leave_the_profile_root() {
        let scratch = Scratch::new("disconnect-traversal");
        let root = scratch.0.join("profiles");
        absent_provider(&root, Provider::Claude, "main");
        let outside = scratch.directory("outside");
        fs::write(outside.join("keep"), "").unwrap();
        for name in ["../../outside", "..", "", "claude/../../outside"] {
            let escaping = Profile {
                name: name.into(),
                ..Profile::new(root.clone(), Provider::Claude, "main").unwrap()
            };
            assert!(disconnect(&escaping).is_err(), "{name:?}");
        }
        assert!(outside.join("keep").exists());
        assert!(root.join("claude/main").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn disconnect_refuses_a_linked_profile_and_keeps_its_target() {
        let scratch = Scratch::new("disconnect-link");
        let root = scratch.0.join("profiles");
        absent_provider(&root, Provider::Claude, "main");
        let outside = scratch.directory("outside");
        std::os::unix::fs::symlink(&outside, root.join("claude/linked")).unwrap();
        let linked = Profile::new(root.clone(), Provider::Claude, "linked").unwrap();
        assert!(disconnect(&linked).is_err());
        assert!(outside.is_dir());
    }

    #[cfg(unix)]
    mod enrollment {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn disconnect_signs_out_a_healthy_profile_then_removes_it() {
            let scratch = Scratch::new("disconnect-healthy");
            let log = scratch.0.join("logout.log");
            let claude = fake_claude(
                &scratch,
                "claude",
                "touch \"$dir/signed-in\"",
                CLAUDE_EMAIL_STATUS,
            );
            let template = template(&scratch, Provider::Claude, claude);
            let profile = enroll(&template, None, Stdio::null).unwrap();
            assert!(profile.signed_in());
            let logging = script(
                &scratch,
                "claude-logout",
                &format!("echo \"$*\" >> '{}'\n", log.display()),
            );
            let profile = Profile {
                executable: Some(logging),
                ..profile
            };
            disconnect(&profile).unwrap();
            assert_eq!(fs::read_to_string(&log).unwrap(), "auth logout\n");
            assert!(entries(&scratch, Provider::Claude).is_empty());
        }

        #[test]
        fn disconnect_still_removes_a_profile_whose_logout_is_refused() {
            let scratch = Scratch::new("disconnect-refused");
            let codex = fake_codex(&scratch, EMAIL_CLAIMS);
            let profile = enroll(
                &template(&scratch, Provider::Codex, codex),
                None,
                Stdio::null,
            )
            .unwrap();
            let refusing = script(
                &scratch,
                "codex-refused",
                "echo 'sign-in refused' >&2\nexit 1\n",
            );
            let profile = Profile {
                executable: Some(refusing),
                ..profile
            };
            assert!(!profile.signed_in());
            disconnect(&profile).unwrap();
            assert!(entries(&scratch, Provider::Codex).is_empty());
        }

        const CLAUDE_EMAIL_STATUS: &str = r#"{"loggedIn":true,"authMethod":"oauth_token","email":"Pilot@Example.test","orgName":"Example Org"}"#;
        const CLAUDE_ORG_STATUS: &str =
            r#"{"loggedIn":true,"authMethod":"claude.ai","orgName":"Example Org"}"#;

        fn script(scratch: &Scratch, name: &str, body: &str) -> PathBuf {
            let path = scratch.directory("tools").join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }

        fn fake_claude(scratch: &Scratch, name: &str, login: &str, status: &str) -> PathBuf {
            script(
                scratch,
                name,
                &format!(
                    "dir=\"$CLAUDE_CONFIG_DIR\"\ncase \"$*\" in\n\"auth login --claudeai\") {login} ;;\n\"auth status --json\") test -f \"$dir/signed-in\" && printf '%s' '{status}' ;;\n*) exit 76 ;;\nesac\n"
                ),
            )
        }

        fn fake_codex(scratch: &Scratch, claims: &str) -> PathBuf {
            let auth = String::from_utf8(codex_auth(claims)).unwrap();
            script(
                scratch,
                "codex",
                &format!(
                    "dir=\"$CODEX_HOME\"\ncase \"$*\" in\n*\" login status\") test -f \"$dir/signed-in\" && echo 'Logged in using ChatGPT' >&2 ;;\n*\" login\") printf '%s' '{auth}' > \"$dir/auth.json\" && touch \"$dir/signed-in\" ;;\n*) exit 76 ;;\nesac\n"
                ),
            )
        }

        fn template(scratch: &Scratch, provider: Provider, executable: PathBuf) -> Profile {
            Profile {
                executable: Some(executable),
                ..Profile::new(scratch.0.join("profiles"), provider, "main").unwrap()
            }
        }

        fn entries(scratch: &Scratch, provider: Provider) -> Vec<String> {
            let parent = scratch.0.join("profiles").join(provider.binary());
            let mut names: Vec<String> = fs::read_dir(parent)
                .map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }

        #[test]
        fn claude_sign_in_names_the_profile_from_the_account_email() {
            let scratch = Scratch::new("enroll-claude");
            let claude = fake_claude(
                &scratch,
                "claude",
                "touch \"$dir/signed-in\"",
                CLAUDE_EMAIL_STATUS,
            );
            let template = template(&scratch, Provider::Claude, claude);
            let profile = enroll(&template, None, Stdio::null).unwrap();
            assert_eq!(profile.to_string(), "claude/pilot@example.test");
            assert_eq!(entries(&scratch, Provider::Claude), ["pilot@example.test"]);
            assert!(profile.signed_in());
            let listed = profiles(&scratch.0.join("profiles")).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].to_string(), "claude/pilot@example.test");
            assert!(profile
                .usage_directory()
                .unwrap()
                .ends_with("pilot@example.test"));
        }

        #[test]
        fn claude_without_email_uses_the_organization_name() {
            let scratch = Scratch::new("enroll-org");
            let claude = fake_claude(
                &scratch,
                "claude",
                "touch \"$dir/signed-in\"",
                CLAUDE_ORG_STATUS,
            );
            let profile = enroll(
                &template(&scratch, Provider::Claude, claude),
                None,
                Stdio::null,
            )
            .unwrap();
            assert_eq!(profile.to_string(), "claude/example-org");
        }

        #[test]
        fn codex_sign_in_names_the_profile_from_the_id_token() {
            let scratch = Scratch::new("enroll-codex");
            let codex = fake_codex(&scratch, EMAIL_CLAIMS);
            let profile = enroll(
                &template(&scratch, Provider::Codex, codex),
                None,
                Stdio::null,
            )
            .unwrap();
            assert_eq!(profile.to_string(), "codex/pilot@example.test");
            let scratch = Scratch::new("enroll-codex-name");
            let codex = fake_codex(&scratch, NAME_CLAIMS);
            let profile = enroll(
                &template(&scratch, Provider::Codex, codex),
                None,
                Stdio::null,
            )
            .unwrap();
            assert_eq!(profile.to_string(), "codex/pilot-example");
        }

        #[test]
        fn an_unreadable_account_falls_back_to_main_then_numbered_names() {
            let scratch = Scratch::new("enroll-fallback");
            let codex = fake_codex(&scratch, "e30");
            let template = template(&scratch, Provider::Codex, codex);
            let first = enroll(&template, None, Stdio::null).unwrap();
            let second = enroll(&template, None, Stdio::null).unwrap();
            assert_eq!(first.to_string(), "codex/main");
            assert_eq!(second.to_string(), "codex/account-2");
        }

        #[test]
        fn a_custom_name_overrides_the_account_name() {
            let scratch = Scratch::new("enroll-custom");
            let claude = fake_claude(
                &scratch,
                "claude",
                "touch \"$dir/signed-in\"",
                CLAUDE_EMAIL_STATUS,
            );
            let template = template(&scratch, Provider::Claude, claude);
            let profile = enroll(&template, Some("work"), Stdio::null).unwrap();
            assert_eq!(profile.to_string(), "claude/work");
            assert!(enroll(&template, Some("../escape"), Stdio::null).is_err());
            assert_eq!(entries(&scratch, Provider::Claude), ["work"]);
        }

        #[test]
        fn signing_in_again_replaces_only_the_matching_account_profile() {
            let scratch = Scratch::new("enroll-again");
            let claude = fake_claude(
                &scratch,
                "claude",
                "touch \"$dir/signed-in\"",
                CLAUDE_EMAIL_STATUS,
            );
            let template = template(&scratch, Provider::Claude, claude);
            let first = enroll(&template, None, Stdio::null).unwrap();
            let directory = first.directory(false).unwrap();
            fs::write(directory.join("stale"), "").unwrap();
            let unrelated = enroll(&template, Some("work"), Stdio::null).unwrap();
            fs::write(unrelated.directory(false).unwrap().join("kept"), "").unwrap();
            let again = enroll(&template, None, Stdio::null).unwrap();
            assert_eq!(again.to_string(), first.to_string());
            assert!(!directory.join("stale").exists());
            assert!(directory.join("signed-in").exists());
            assert!(unrelated.directory(false).unwrap().join("kept").exists());
            assert_eq!(
                entries(&scratch, Provider::Claude),
                ["pilot@example.test", "work"]
            );
        }

        #[test]
        fn cancelled_or_incomplete_sign_in_leaves_no_profile_behind() {
            let scratch = Scratch::new("enroll-cancel");
            let cancelled = fake_claude(&scratch, "claude", "exit 130", CLAUDE_EMAIL_STATUS);
            let error = enroll(
                &template(&scratch, Provider::Claude, cancelled),
                None,
                Stdio::null,
            )
            .unwrap_err();
            assert!(error.to_string().contains("130"), "{error}");
            assert!(entries(&scratch, Provider::Claude).is_empty());
            let incomplete =
                fake_claude(&scratch, "claude-incomplete", "true", CLAUDE_EMAIL_STATUS);
            let error = enroll(
                &template(&scratch, Provider::Claude, incomplete),
                None,
                Stdio::null,
            )
            .unwrap_err();
            assert_eq!(error.to_string(), "sign-in did not complete");
            assert!(entries(&scratch, Provider::Claude).is_empty());
        }
    }
}
