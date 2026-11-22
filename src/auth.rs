#[cfg(unix)]
#[path = "auth/unix.rs"]
mod privacy;
#[cfg(windows)]
#[path = "auth/windows.rs"]
mod privacy;

use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Provider {
    Claude,
    Codex,
}

impl Provider {
    const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
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
    #[cfg(all(test, unix))]
    pub(crate) fn fixture(root: &Path, provider: Provider, executable: PathBuf) -> Self {
        let profile = Self {
            provider,
            name: "fixture".into(),
            root: root.into(),
            executable: Some(executable),
        };
        profile.directory(true).unwrap();
        profile
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
        let mut command = Command::new(self.provider.binary());
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
    let provider = Provider::ALL
        .into_iter()
        .find(|p| p.binary() == provider)
        .ok_or_else(|| invalid("Provider must be claude or codex"))?;
    if !valid_name(name) {
        return Err(invalid("Use a profile name of 1–64 lowercase letters, digits, _ or -, starting with a letter or digit. Windows device names are reserved."));
    }
    let profile = Profile {
        provider,
        name: name.clone(),
        root,
        #[cfg(test)]
        executable: None,
    };
    let status = match (action.as_str(), rest) {
        ("status", []) => {
            profile.authenticated_command()?;
            println!("{profile} is signed in");
            return Ok(());
        }
        ("login" | "logout", []) => {
            let directory = profile.directory(action == "login")?;
            profile
                .command(&directory)
                .current_dir(&directory)
                .args(provider.action(action))
                .status()?
        }
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
