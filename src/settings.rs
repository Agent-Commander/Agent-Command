use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) const NO_UPDATE_CHECK: &str = "AGENT_COMMAND_NO_UPDATE_CHECK";
const SETTINGS_FILE: &str = "settings.toml";
const THEME_FILE: &str = "theme.toml";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub(crate) check_for_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            check_for_updates: true,
        }
    }
}

impl Settings {
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(self).map_err(io::Error::other)?;
        fs::write(path, text)
    }
}

pub(crate) fn forced_off(no_update_check: Option<&OsString>) -> bool {
    no_update_check.is_some_and(|value| value == "1")
}

pub(crate) fn launch_update_check(settings: &Settings, no_update_check: Option<&OsString>) -> bool {
    settings.check_for_updates && !forced_off(no_update_check)
}

fn config_directory() -> Option<PathBuf> {
    let under = |variable: &str, suffix: &str| {
        env::var_os(variable).map(|base| PathBuf::from(base).join(suffix))
    };
    under("XDG_CONFIG_HOME", "agent-command")
        .or_else(|| under("APPDATA", "agent-command"))
        .or_else(|| under("HOME", ".config/agent-command"))
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LaunchConfig {
    pub(crate) settings_path: Option<PathBuf>,
    pub(crate) theme_path: Option<PathBuf>,
    pub(crate) no_update_check: Option<OsString>,
}

impl LaunchConfig {
    pub(crate) fn from_env() -> Self {
        let directory = config_directory();
        let theme_path = env::var_os("AGENT_COMMAND_THEME")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("AGENT_COMMAND_DEV")
                    .map(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(THEME_FILE))
            })
            .or_else(|| directory.as_ref().map(|path| path.join(THEME_FILE)));
        Self {
            settings_path: directory.map(|path| path.join(SETTINGS_FILE)),
            theme_path,
            no_update_check: env::var_os(NO_UPDATE_CHECK),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_on_and_saved_toggle_round_trips() {
        let path = env::temp_dir()
            .join(format!("ac-settings-{}", std::process::id()))
            .join(SETTINGS_FILE);
        let _ = fs::remove_file(&path);
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        let off = Settings {
            check_for_updates: false,
        };
        off.save(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "check_for_updates = false\n"
        );
        assert_eq!(Settings::load(&path).unwrap(), off);
        fs::write(&path, "check_for_updates = 3").unwrap();
        assert_eq!(
            Settings::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn environment_variable_forces_the_launch_check_off() {
        let on = Settings::default();
        let one = OsString::from("1");
        let zero = OsString::from("0");
        assert!(launch_update_check(&on, None));
        assert!(launch_update_check(&on, Some(&zero)));
        assert!(!launch_update_check(&on, Some(&one)));
        let off = Settings {
            check_for_updates: false,
        };
        assert!(!launch_update_check(&off, None));
    }
}
