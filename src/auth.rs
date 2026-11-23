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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Found {
    OnPath,
    At(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 64
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
        && !matches!(
            name,
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com0"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt0"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
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
            command.args([
                "-c",
                "cli_auth_credentials_store=\"file\"",
                "-c",
                "forced_login_method=\"chatgpt\"",
            ]);
        }
        command
    }

    fn active(&self, directory: &Path) -> io::Result<bool> {
        let output = self
            .command(directory)
            .current_dir(directory)
            .args(self.provider.action("status"))
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Ok(false);
        }
        match self.provider {
            Provider::Claude => {
                let data: serde_json::Value = serde_json::from_slice(&output.stdout)
                    .map_err(|_| invalid("Claude returned an unreadable authentication status"))?;
                Ok(data["loggedIn"] == true && data["authMethod"] == "claude.ai")
            }
            Provider::Codex => Ok([output.stdout, output.stderr].iter().any(|stream| {
                String::from_utf8_lossy(stream)
                    .lines()
                    .any(|line| line.trim().eq_ignore_ascii_case("logged in using chatgpt"))
            })),
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
    program_for(provider, adopted(provider))
}

fn program_for(provider: Provider, adopted: Option<PathBuf>) -> OsString {
    adopted.map_or_else(|| provider.binary().into(), PathBuf::into_os_string)
}

pub(crate) fn found(provider: Provider) -> Option<Found> {
    if let Some(executable) = adopted(provider).filter(|path| path.is_file()) {
        return Some(Found::At(executable));
    }
    program_on_path(provider.binary()).then_some(Found::OnPath)
}

pub(crate) fn program_on_path(binary: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| on_path(binary, &path))
}

fn on_path(binary: &str, path: &OsStr) -> bool {
    let extensions = executable_extensions();
    env::split_paths(path).any(|directory| {
        extensions
            .iter()
            .any(|extension| directory.join(format!("{binary}{extension}")).is_file())
    })
}

fn executable_extensions() -> Vec<String> {
    if cfg!(windows) {
        let listed = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(String::new())
            .chain(listed.split(';').map(str::to_owned))
            .collect()
    } else {
        vec![String::new()]
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

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    if matches!(args, [] | [_])
        && args
            .first()
            .is_none_or(|value| value == "--help" || value == "-h")
    {
        println!("agent-command auth list\nagent-command auth {{login|status|logout|run}} {{claude|codex}} PROFILE [-- CLI_ARGS...]");
        return Ok(());
    }
    let root = root()?;
    if args == ["list"] {
        for profile in profiles(&root)? {
            println!("{} {}", profile.provider.binary(), profile.name);
        }
        return Ok(());
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
        ("login", []) => sign_in(&profile)?,
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

    #[test]
    fn path_lookup_finds_only_present_executables() {
        let directory = env::temp_dir().join(format!("ac-path-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let binary = if cfg!(windows) {
            "fake-cli.exe"
        } else {
            "fake-cli"
        };
        fs::write(directory.join(binary), "").unwrap();
        let path = env::join_paths([directory.clone()]).unwrap();
        assert!(on_path("fake-cli", &path));
        assert!(!on_path("absent-cli", &path));
        fs::remove_dir_all(directory).unwrap();
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

    #[test]
    fn profile_names_follow_the_existing_rule() {
        assert!(validate_name("main").is_ok());
        for refused in ["", "Main", "-main", "con", "has space"] {
            assert_eq!(validate_name(refused), Err(NAME_RULE), "{refused:?}");
        }
    }
}
