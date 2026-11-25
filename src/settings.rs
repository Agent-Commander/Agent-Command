use crate::permissions::PermissionMode;
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
pub(crate) const SOUL_FILE: &str = "SOUL.md";

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub(crate) check_for_updates: bool,
    pub(crate) auto_resume_on_reset: bool,
    pub(crate) snooze_limited_missions: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fallback_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text_gen_profile: Option<String>,
    pub(crate) goal_turn_cap: u32,
    pub(crate) commander_permission: PermissionMode,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub(crate) prod_gate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) soul_path: Option<PathBuf>,
}

pub(crate) const DEFAULT_GOAL_TURN_CAP: u32 = 10;
pub(crate) const MAX_GOAL_TURN_CAP: u32 = 100;

impl Default for Settings {
    fn default() -> Self {
        Self {
            check_for_updates: true,
            auto_resume_on_reset: false,
            snooze_limited_missions: false,
            fallback_profile: None,
            text_gen_profile: None,
            goal_turn_cap: DEFAULT_GOAL_TURN_CAP,
            commander_permission: PermissionMode::COMMANDER_DEFAULT,
            prod_gate: true,
            soul_path: None,
        }
    }
}

pub(crate) fn next_profile_slot(current: Option<&str>, labels: &[String]) -> Option<String> {
    let position = current.and_then(|label| labels.iter().position(|candidate| candidate == label));
    match position {
        None => labels.first().cloned(),
        Some(index) => labels.get(index + 1).cloned(),
    }
}

pub(crate) fn slot_label(slot: Option<&str>, labels: &[String]) -> String {
    match slot {
        None => "Off".into(),
        Some(label) if labels.iter().any(|candidate| candidate == label) => label.into(),
        Some(label) => format!("{label} (not signed in)"),
    }
}

impl Settings {
    pub(crate) fn goal_cap(&self) -> u32 {
        self.goal_turn_cap.clamp(1, MAX_GOAL_TURN_CAP)
    }

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

pub fn config_directory() -> Option<PathBuf> {
    let under = |variable: &str, suffix: &str| {
        env::var_os(variable).map(|base| PathBuf::from(base).join(suffix))
    };
    under("XDG_CONFIG_HOME", "agent-command")
        .or_else(|| under("APPDATA", "agent-command"))
        .or_else(|| under("HOME", ".config/agent-command"))
}

pub fn default_soul_path() -> Option<PathBuf> {
    config_directory().map(|dir| dir.join(SOUL_FILE))
}

impl Settings {
    pub fn effective_soul_path(&self) -> Option<PathBuf> {
        self.soul_path.clone().or_else(default_soul_path)
    }
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
            ..Settings::default()
        };
        off.save(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "check_for_updates = false\nauto_resume_on_reset = false\nsnooze_limited_missions = false\ngoal_turn_cap = 10\ncommander_permission = \"workspace-auto\"\n"
        );
        assert_eq!(Settings::load(&path).unwrap(), off);
        fs::write(&path, "check_for_updates = false\n").unwrap();
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
            ..Settings::default()
        };
        assert!(!launch_update_check(&off, None));
    }

    #[test]
    fn limit_and_profile_slots_round_trip_through_settings_toml() {
        let path = env::temp_dir()
            .join(format!("ac-settings-slots-{}", std::process::id()))
            .join(SETTINGS_FILE);
        let configured = Settings {
            check_for_updates: true,
            auto_resume_on_reset: true,
            snooze_limited_missions: true,
            fallback_profile: Some("codex/backup".into()),
            text_gen_profile: Some("claude/light".into()),
            goal_turn_cap: 4,
            commander_permission: PermissionMode::ReadOnly,
            ..Settings::default()
        };
        configured.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("auto_resume_on_reset = true"));
        assert!(text.contains("fallback_profile = \"codex/backup\""));
        assert!(text.contains("text_gen_profile = \"claude/light\""));
        assert!(text.contains("commander_permission = \"read-only\""));
        assert_eq!(Settings::load(&path).unwrap(), configured);
        fs::write(&path, "fallback_token = \"x\"").unwrap();
        assert_eq!(
            Settings::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn goal_cap_is_clamped_and_profile_slots_cycle_through_off() {
        let mut settings = Settings::default();
        assert_eq!(settings.goal_cap(), DEFAULT_GOAL_TURN_CAP);
        settings.goal_turn_cap = 0;
        assert_eq!(settings.goal_cap(), 1);
        settings.goal_turn_cap = 5_000;
        assert_eq!(settings.goal_cap(), MAX_GOAL_TURN_CAP);
        let labels = vec!["claude/a".to_owned(), "codex/b".to_owned()];
        assert_eq!(
            next_profile_slot(None, &labels).as_deref(),
            Some("claude/a")
        );
        assert_eq!(
            next_profile_slot(Some("claude/a"), &labels).as_deref(),
            Some("codex/b")
        );
        assert_eq!(next_profile_slot(Some("codex/b"), &labels), None);
        assert_eq!(
            next_profile_slot(Some("gone/x"), &labels).as_deref(),
            Some("claude/a")
        );
        assert_eq!(slot_label(None, &labels), "Off");
        assert_eq!(slot_label(Some("codex/b"), &labels), "codex/b");
        assert_eq!(
            slot_label(Some("gone/x"), &labels),
            "gone/x (not signed in)"
        );
    }
}
