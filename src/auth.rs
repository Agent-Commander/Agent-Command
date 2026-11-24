#[cfg(unix)]
#[path = "auth/unix.rs"]
mod privacy;
#[cfg(windows)]
#[path = "auth/windows.rs"]
mod privacy;

use std::{
    env,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{Mutex, PoisonError},
};

static ADOPTED: Mutex<[Option<PathBuf>; 2]> = Mutex::new([None, None]);

const CODEX_PROFILE_ARGUMENTS: [&str; 4] = [
    "-c",
    "cli_auth_credentials_store='file'",
    "-c",
    "forced_login_method='chatgpt'",
];
const WINDOWS_EXECUTABLE_EXTENSIONS: [&str; 4] = [".exe", ".com", ".cmd", ".bat"];
const STAGING_PREFIX: &str = ".connecting-";
const FALLBACK_PROFILE_NAME: &str = "main";
const CODEX_AUTH_FILE: &str = "auth.json";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Found {
    OnPath,
    At(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub(crate) const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.binary() == name)
    }

    pub(crate) fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
        }
    }

    fn home_variable(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }

    fn action(self, action: &str) -> Vec<&str> {
        match (self, action) {
            (Self::Claude, "login") => vec!["auth", "login", "--claudeai"],
            (Self::Claude, "status") => vec!["auth", "status", "--json"],
            (Self::Claude, _) => vec!["auth", action],
            (Self::Codex, "status") => vec!["login", "status"],
            (Self::Codex, _) => vec![action],
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

pub(crate) fn root() -> io::Result<PathBuf> {
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
pub(crate) struct Profile {
    pub(crate) provider: Provider,
    name: String,
    root: PathBuf,
    #[cfg(test)]
    executable: Option<PathBuf>,
}

impl fmt::Display for Profile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider.binary(), self.name)
    }
}

fn inherited_provider_variable(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "OPENAI_",
        "ANTHROPIC_",
        "CLAUDE_CODE_",
        "CODEX_",
        "CHATGPT_",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
        || matches!(
            upper.as_str(),
            "CLAUDECODE" | "CLAUDE_CONFIG_DIR" | "CLAUDE_SESSION_ID"
        )
}

impl Profile {
    pub(crate) fn new(root: PathBuf, provider: Provider, name: &str) -> Result<Self, &'static str> {
        validate_name(name)?;
        Ok(Self {
            provider,
            name: name.into(),
            root,
            #[cfg(test)]
            executable: None,
        })
    }

    pub(crate) fn signed_in(&self) -> bool {
        self.authenticated_command().is_ok()
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

    #[cfg(all(test, unix))]
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
            executable: Some(executable),
        };
        profile.directory(true).unwrap();
        profile
    }

    #[cfg(all(test, unix))]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn usage_directory(&self) -> io::Result<PathBuf> {
        self.directory(false)?;
        let usage = self.root.join("agent-command-usage-v1");
        let provider = usage.join(self.provider.binary());
        let profile = provider.join(&self.name);
        for path in [&usage, &provider, &profile] {
            privacy::create(path)?;
        }
        Ok(profile)
    }

    pub(crate) fn checkpoint_directory(&self) -> io::Result<PathBuf> {
        self.directory(false)?;
        let directory = self.root.join(crate::checkpoint::DIRECTORY);
        privacy::create(&directory)?;
        Ok(directory)
    }

    pub(crate) fn isolated_command(&self) -> io::Result<(Command, PathBuf)> {
        let directory = self.directory(false)?;
        Ok((self.command(&directory), directory))
    }

    fn directory(&self, create: bool) -> io::Result<PathBuf> {
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

    fn command(&self, directory: &Path) -> Command {
        let mut command = Command::new(program(self.provider));
        #[cfg(test)]
        if let Some(executable) = &self.executable {
            command = Command::new(executable);
        }
        for (key, _) in env::vars_os() {
            if key.to_str().is_some_and(inherited_provider_variable) {
                command.env_remove(key);
            }
        }
        command.env(self.provider.home_variable(), directory);
        if self.provider == Provider::Codex {
            command.args(CODEX_PROFILE_ARGUMENTS);
        }
        command
    }

    fn status(&self, directory: &Path) -> io::Result<Option<std::process::Output>> {
        let output = self
            .command(directory)
            .current_dir(directory)
            .args(self.provider.action("status"))
            .stdin(Stdio::null())
            .output()?;
        Ok(output.status.success().then_some(output))
    }

    fn claude_status(&self, directory: &Path) -> io::Result<Option<serde_json::Value>> {
        let Some(output) = self.status(directory)? else {
            return Ok(None);
        };
        serde_json::from_slice(&output.stdout)
            .map(Some)
            .map_err(|_| invalid("Claude returned an unreadable authentication status"))
    }

    fn active(&self, directory: &Path) -> io::Result<bool> {
        match self.provider {
            Provider::Claude => Ok(self
                .claude_status(directory)?
                .is_some_and(|data| data["loggedIn"] == true && data["authMethod"] == "claude.ai")),
            Provider::Codex => Ok(self.status(directory)?.is_some_and(|output| {
                [output.stdout, output.stderr].iter().any(|stream| {
                    String::from_utf8_lossy(stream)
                        .lines()
                        .any(|line| line.trim().eq_ignore_ascii_case("logged in using chatgpt"))
                })
            })),
        }
    }

    fn account(&self, directory: &Path) -> Option<String> {
        match self.provider {
            Provider::Claude => claude_account(&self.claude_status(directory).ok()??),
            Provider::Codex => codex_account(&fs::read(directory.join(CODEX_AUTH_FILE)).ok()?),
        }
    }

    pub(crate) fn authenticated_command(&self) -> io::Result<Command> {
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

pub(crate) fn profiles(root: &Path) -> io::Result<Vec<Profile>> {
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
                profiles.push(Profile {
                    root: root.into(),
                    provider,
                    name: name.into(),
                    #[cfg(test)]
                    executable: None,
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

pub(crate) fn validate_name(name: &str) -> Result<(), &'static str> {
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

pub(crate) fn resolve(binary: &str) -> Option<PathBuf> {
    resolve_in(binary, &env::var_os("PATH")?, cfg!(windows))
}

fn resolve_in(binary: &str, path: &OsStr, windows: bool) -> Option<PathBuf> {
    let directories: Vec<PathBuf> = env::split_paths(path).collect();
    candidate_names(binary, windows)
        .into_iter()
        .flat_map(|name| {
            directories
                .iter()
                .map(move |directory| directory.join(&name))
        })
        .find(|candidate| candidate.is_file())
}

fn candidate_names(binary: &str, windows: bool) -> Vec<String> {
    if windows {
        WINDOWS_EXECUTABLE_EXTENSIONS
            .iter()
            .map(|extension| format!("{binary}{extension}"))
            .collect()
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

fn first_text(data: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| data[*key].as_str())
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(crate) fn claude_account(status: &serde_json::Value) -> Option<String> {
    first_text(status, &["email", "orgName"])
}

pub(crate) fn codex_account(auth_json: &[u8]) -> Option<String> {
    let auth: serde_json::Value = serde_json::from_slice(auth_json).ok()?;
    let payload = auth["tokens"]["id_token"].as_str()?.split('.').nth(1)?;
    let claims: serde_json::Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    first_text(&claims, &["email", "name"])
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
            name,
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
        Ok(Profile {
            name,
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
        println!("agent-command auth list\nagent-command auth login {{claude|codex}} [PROFILE]\nagent-command auth {{status|logout|run}} {{claude|codex}} PROFILE [-- CLI_ARGS...]\nWithout PROFILE, login names the profile from the signed-in account's email or name.");
        return Ok(());
    }
    let root = root()?;
    if args == ["list"] {
        for profile in profiles(&root)? {
            println!("{} {}", profile.provider.binary(), profile.name);
        }
        return Ok(());
    }
    if let [action, provider] = args {
        if action == "login" {
            let provider = Provider::named(provider)
                .ok_or_else(|| invalid("Provider must be claude or codex"))?;
            let profile = connect(&root, provider, None)?;
            println!("{profile} is signed in");
            return Ok(());
        }
    }
    let [action, provider, name, rest @ ..] = args else {
        return Err(invalid("Use agent-command auth --help"));
    };
    let provider =
        Provider::named(provider).ok_or_else(|| invalid("Provider must be claude or codex"))?;
    let profile = Profile::new(root, provider, name).map_err(invalid)?;
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
    fn windows_resolution_ignores_extensionless_shims_and_prefers_exe() {
        let scratch = Scratch::new("windows-order");
        let npm = scratch.directory("npm");
        let native = scratch.directory("native");
        fs::write(npm.join("codex"), "#!/bin/sh\n").unwrap();
        let path = env::join_paths([npm.clone(), native.clone()]).unwrap();
        assert_eq!(resolve_in("codex", &path, true), None);
        fs::write(npm.join("codex.cmd"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(npm.join("codex.cmd"))
        );
        fs::write(npm.join("codex.bat"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(npm.join("codex.cmd"))
        );
        fs::write(native.join("codex.com"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(native.join("codex.com"))
        );
        fs::write(native.join("codex.exe"), "").unwrap();
        assert_eq!(
            resolve_in("codex", &path, true),
            Some(native.join("codex.exe"))
        );
        assert_eq!(
            resolve_in("codex", &path, false),
            Some(npm.join("codex")),
            "POSIX keeps the bare name"
        );
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

    #[cfg(unix)]
    mod enrollment {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        const CLAUDE_EMAIL_STATUS: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","email":"Pilot@Example.test","orgName":"Example Org"}"#;
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
