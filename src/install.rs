use crate::auth::Provider;
use std::{ffi::OsString, path::PathBuf};

pub(crate) const CLAUDE_DOCS: &str = "https://code.claude.com/docs/en/setup";
pub(crate) const CODEX_DOCS: &str =
    "https://github.com/openai/codex#installing-and-running-codex-cli";

const CLAUDE_UNIX: &str = "curl -fsSL https://claude.ai/install.sh | bash";
const CLAUDE_WINDOWS: &str = "irm https://claude.ai/install.ps1 | iex";
const CODEX_UNIX: &str = "curl -fsSL https://chatgpt.com/codex/install.sh | sh";
const CODEX_WINDOWS: &str = "irm https://chatgpt.com/codex/install.ps1 | iex";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Platform {
    Unix,
    Windows,
}

impl Platform {
    pub(crate) fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }

    fn executable(self, binary: &str) -> String {
        match self {
            Self::Unix => binary.into(),
            Self::Windows => format!("{binary}.exe"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Plan {
    pub(crate) provider: Provider,
    pub(crate) program: &'static str,
    pub(crate) arguments: Vec<&'static str>,
    pub(crate) command: String,
    pub(crate) needs: &'static [&'static str],
    pub(crate) docs: &'static str,
}

pub(crate) fn plan(provider: Provider, platform: Platform) -> Plan {
    match (provider, platform) {
        (Provider::Claude, Platform::Unix) => shell_plan(provider, CLAUDE_UNIX, &["curl", "bash"]),
        (Provider::Codex, Platform::Unix) => shell_plan(provider, CODEX_UNIX, &["curl", "sh"]),
        (Provider::Claude, Platform::Windows) => {
            powershell_plan(provider, vec!["-NoProfile", "-Command", CLAUDE_WINDOWS])
        }
        (Provider::Codex, Platform::Windows) => powershell_plan(
            provider,
            vec!["-ExecutionPolicy", "ByPass", "-c", CODEX_WINDOWS],
        ),
    }
}

pub(crate) fn docs(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => CLAUDE_DOCS,
        Provider::Codex => CODEX_DOCS,
    }
}

fn shell_plan(provider: Provider, line: &'static str, needs: &'static [&'static str]) -> Plan {
    Plan {
        provider,
        program: "sh",
        arguments: vec!["-c", line],
        command: line.into(),
        needs,
        docs: docs(provider),
    }
}

fn powershell_plan(provider: Provider, arguments: Vec<&'static str>) -> Plan {
    let (script, switches) = arguments
        .split_last()
        .expect("powershell plans carry a script");
    Plan {
        provider,
        program: "powershell",
        command: format!("powershell {} \"{script}\"", switches.join(" ")),
        arguments,
        needs: &["powershell"],
        docs: docs(provider),
    }
}

pub(crate) fn missing_prerequisites(
    plan: &Plan,
    available: impl Fn(&str) -> bool,
) -> Vec<&'static str> {
    plan.needs
        .iter()
        .copied()
        .filter(|program| !available(program))
        .collect()
}

pub(crate) fn documented_executable(
    provider: Provider,
    platform: Platform,
    variable: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let absolute = |name: &str| {
        variable(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let directory = match (provider, platform) {
        (Provider::Claude, Platform::Unix) => absolute("HOME")?.join(".local").join("bin"),
        (Provider::Claude, Platform::Windows) => {
            absolute("USERPROFILE")?.join(".local").join("bin")
        }
        (Provider::Codex, Platform::Unix) => absolute("CODEX_INSTALL_DIR")
            .or_else(|| Some(absolute("HOME")?.join(".local").join("bin")))?,
        (Provider::Codex, Platform::Windows) => absolute("CODEX_INSTALL_DIR").or_else(|| {
            Some(
                absolute("LOCALAPPDATA")?
                    .join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin"),
            )
        })?,
    };
    Some(directory.join(platform.executable(provider.binary())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(name, value)| ((*name).into(), (*value).into()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn absolute(unix: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", unix.replace('/', "\\"))
        } else {
            unix.into()
        }
    }

    #[test]
    fn unix_plans_run_the_official_installers_through_sh() {
        let claude = plan(Provider::Claude, Platform::Unix);
        assert_eq!(
            claude.command,
            "curl -fsSL https://claude.ai/install.sh | bash"
        );
        assert_eq!((claude.program, claude.arguments[0]), ("sh", "-c"));
        assert_eq!(claude.arguments[1], claude.command);
        assert_eq!(claude.needs, ["curl", "bash"]);
        let codex = plan(Provider::Codex, Platform::Unix);
        assert_eq!(
            codex.command,
            "curl -fsSL https://chatgpt.com/codex/install.sh | sh"
        );
        assert_eq!(codex.docs, CODEX_DOCS);
    }

    #[test]
    fn windows_plans_use_powershell_without_elevation() {
        let claude = plan(Provider::Claude, Platform::Windows);
        assert_eq!(
            claude.command,
            "powershell -NoProfile -Command \"irm https://claude.ai/install.ps1 | iex\""
        );
        let codex = plan(Provider::Codex, Platform::Windows);
        assert_eq!(
            codex.command,
            "powershell -ExecutionPolicy ByPass -c \"irm https://chatgpt.com/codex/install.ps1 | iex\""
        );
        assert_eq!(codex.program, "powershell");
        assert_eq!(codex.arguments.last(), Some(&CODEX_WINDOWS));
    }

    #[test]
    fn no_plan_elevates_or_targets_a_system_path() {
        for provider in Provider::ALL {
            for platform in [Platform::Unix, Platform::Windows] {
                let shown = plan(provider, platform).command.to_lowercase();
                for refused in ["sudo", "runas", "-verb", "/usr/", "program files", " -g "] {
                    assert!(!shown.contains(refused), "{shown}");
                }
            }
        }
    }

    #[test]
    fn missing_prerequisites_lists_only_absent_programs() {
        let codex = plan(Provider::Codex, Platform::Unix);
        assert_eq!(missing_prerequisites(&codex, |name| name == "sh"), ["curl"]);
        assert!(missing_prerequisites(&codex, |_| true).is_empty());
    }

    #[test]
    fn documented_directories_follow_each_installer() {
        let home = absolute("/home/pilot");
        let unix = lookup(&[("HOME", &home)]);
        assert_eq!(
            documented_executable(Provider::Claude, Platform::Unix, &unix).unwrap(),
            Path::new(&home).join(".local").join("bin").join("claude")
        );
        assert_eq!(
            documented_executable(Provider::Codex, Platform::Unix, &unix).unwrap(),
            Path::new(&home).join(".local").join("bin").join("codex")
        );
        let custom = absolute("/opt/pilot/codex");
        let chosen = lookup(&[("HOME", &home), ("CODEX_INSTALL_DIR", &custom)]);
        assert_eq!(
            documented_executable(Provider::Codex, Platform::Unix, chosen).unwrap(),
            Path::new(&custom).join("codex")
        );
        let profile = absolute("/Users/pilot");
        let local = absolute("/Users/pilot/AppData/Local");
        let windows = lookup(&[("USERPROFILE", &profile), ("LOCALAPPDATA", &local)]);
        assert_eq!(
            documented_executable(Provider::Claude, Platform::Windows, &windows).unwrap(),
            Path::new(&profile)
                .join(".local")
                .join("bin")
                .join("claude.exe")
        );
        assert_eq!(
            documented_executable(Provider::Codex, Platform::Windows, &windows).unwrap(),
            Path::new(&local)
                .join("Programs")
                .join("OpenAI")
                .join("Codex")
                .join("bin")
                .join("codex.exe")
        );
    }

    #[test]
    fn relative_or_absent_homes_give_no_location() {
        let relative = lookup(&[("HOME", "pilot")]);
        assert!(documented_executable(Provider::Claude, Platform::Unix, relative).is_none());
        assert!(documented_executable(Provider::Codex, Platform::Windows, lookup(&[])).is_none());
    }
}
