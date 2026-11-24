use crate::{auth::Provider, specialists::Isolation};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PermissionMode {
    ReadOnly,
    #[default]
    Supervised,
    AutoAcceptEdits,
    WorktreeAuto,
    WorkspaceAuto,
}

pub(crate) const MODE_USAGE: &str =
    "--mode read-only|supervised|auto-edits|worktree-auto|workspace-auto";
pub(crate) const COMMANDER_MODE_USAGE: &str =
    "Usage: /mode read-only|supervised|auto-edits|workspace-auto";

pub(crate) const FORBIDDEN_FLAGS: [&str; 3] = [
    "--dangerously-skip-permissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "--yolo",
];

const CLAUDE_WRITE_TOOLS: &str = "Edit,Write,NotebookEdit";
const CLAUDE_WORKTREE_TOOLS: &str = "Bash,Edit,Write,NotebookEdit";
const READ_ONLY_ROLE_WORDS: [&str; 4] = ["playtest", "review", "qa", "audit"];
const EDITING_ROLE_WORDS: [&str; 4] = ["copywrit", "doc", "writer", "editor"];

const CLAUDE_STREAM_ARGUMENTS: [&str; 6] = [
    "--print",
    "--verbose",
    "--output-format",
    "stream-json",
    "--setting-sources",
    "user",
];
const END_OF_OPTIONS: &str = "--";

impl PermissionMode {
    pub(crate) const ALL: [Self; 5] = [
        Self::ReadOnly,
        Self::Supervised,
        Self::AutoAcceptEdits,
        Self::WorktreeAuto,
        Self::WorkspaceAuto,
    ];

    pub(crate) const COMMANDER: [Self; 4] = [
        Self::ReadOnly,
        Self::Supervised,
        Self::AutoAcceptEdits,
        Self::WorkspaceAuto,
    ];

    pub(crate) const COMMANDER_DEFAULT: Self = Self::WorkspaceAuto;

    pub(crate) fn flag(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Supervised => "supervised",
            Self::AutoAcceptEdits => "auto-edits",
            Self::WorktreeAuto => "worktree-auto",
            Self::WorkspaceAuto => "workspace-auto",
        }
    }

    pub(crate) fn parse_commander(text: &str) -> Result<Self, String> {
        Self::COMMANDER
            .into_iter()
            .find(|mode| mode.flag().eq_ignore_ascii_case(text))
            .ok_or_else(|| format!("Commander cannot use {text}. {COMMANDER_MODE_USAGE}"))
    }

    pub(crate) fn next_commander(self) -> Self {
        let position = Self::COMMANDER.iter().position(|mode| *mode == self);
        let next = position.map_or(0, |index| (index + 1) % Self::COMMANDER.len());
        Self::COMMANDER[next]
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.flag().eq_ignore_ascii_case(text))
            .ok_or_else(|| format!("Unknown permission mode {text}. Use {MODE_USAGE}"))
    }

    pub(crate) fn default_for_role(role: &str) -> Self {
        let role = role.to_ascii_lowercase();
        let mentions = |words: &[&str]| words.iter().any(|word| role.contains(word));
        if mentions(&READ_ONLY_ROLE_WORDS) {
            Self::ReadOnly
        } else if mentions(&EDITING_ROLE_WORDS) {
            Self::AutoAcceptEdits
        } else {
            Self::Supervised
        }
    }

    pub(crate) fn require_isolation(self, isolation: Option<&Isolation>) -> Result<(), String> {
        match (self, isolation) {
            (Self::WorktreeAuto, Some(Isolation::Worktree { .. })) => Ok(()),
            (Self::WorktreeAuto, _) => Err(
                "worktree-auto is refused: the workspace is not an isolated git worktree.".into(),
            ),
            _ => Ok(()),
        }
    }

    fn claude_arguments(self) -> Vec<&'static str> {
        match self {
            Self::ReadOnly => vec![
                "--permission-mode",
                "default",
                "--disallowedTools",
                CLAUDE_WRITE_TOOLS,
            ],
            Self::Supervised => vec!["--permission-mode", "default"],
            Self::AutoAcceptEdits => vec!["--permission-mode", "acceptEdits"],
            Self::WorktreeAuto | Self::WorkspaceAuto => vec![
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                CLAUDE_WORKTREE_TOOLS,
            ],
        }
    }

    fn codex_sandbox(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Supervised | Self::AutoAcceptEdits | Self::WorktreeAuto | Self::WorkspaceAuto => {
                "workspace-write"
            }
        }
    }
}

pub(crate) fn dispatch_arguments(provider: Provider, mode: PermissionMode) -> Vec<&'static str> {
    match provider {
        Provider::Claude => {
            let mut arguments = CLAUDE_STREAM_ARGUMENTS.to_vec();
            arguments.extend(mode.claude_arguments());
            arguments.push(END_OF_OPTIONS);
            arguments
        }
        Provider::Codex => vec![
            "-a",
            "never",
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--sandbox",
            mode.codex_sandbox(),
            "-",
        ],
    }
}

pub(crate) fn refuse_bypass<'a>(
    arguments: impl IntoIterator<Item = &'a OsStr>,
) -> Result<(), String> {
    match arguments
        .into_iter()
        .filter_map(OsStr::to_str)
        .find(|argument| FORBIDDEN_FLAGS.contains(argument))
    {
        Some(flag) => Err(format!("Refused to launch a provider CLI with {flag}.")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn joined(provider: Provider, mode: PermissionMode) -> String {
        dispatch_arguments(provider, mode).join(" ")
    }

    #[test]
    fn claude_argv_for_every_mode_is_exact() {
        let stream = "--print --verbose --output-format stream-json --setting-sources user";
        assert_eq!(
            joined(Provider::Claude, PermissionMode::ReadOnly),
            format!(
                "{stream} --permission-mode default --disallowedTools Edit,Write,NotebookEdit --"
            )
        );
        assert_eq!(
            joined(Provider::Claude, PermissionMode::Supervised),
            format!("{stream} --permission-mode default --")
        );
        assert_eq!(
            joined(Provider::Claude, PermissionMode::AutoAcceptEdits),
            format!("{stream} --permission-mode acceptEdits --")
        );
        for mode in [PermissionMode::WorktreeAuto, PermissionMode::WorkspaceAuto] {
            assert_eq!(
                joined(Provider::Claude, mode),
                format!(
                    "{stream} --permission-mode acceptEdits --allowedTools Bash,Edit,Write,NotebookEdit --"
                )
            );
        }
    }

    #[test]
    fn codex_argv_for_every_mode_is_exact() {
        let expected = |sandbox: &str| {
            format!("-a never exec --json --skip-git-repo-check --sandbox {sandbox} -")
        };
        assert_eq!(
            joined(Provider::Codex, PermissionMode::ReadOnly),
            expected("read-only")
        );
        for mode in [
            PermissionMode::Supervised,
            PermissionMode::AutoAcceptEdits,
            PermissionMode::WorktreeAuto,
            PermissionMode::WorkspaceAuto,
        ] {
            assert_eq!(joined(Provider::Codex, mode), expected("workspace-write"));
        }
    }

    #[test]
    fn read_only_locks_every_write_tool_on_both_providers() {
        let claude = dispatch_arguments(Provider::Claude, PermissionMode::ReadOnly);
        let disallowed = claude
            .iter()
            .position(|argument| *argument == "--disallowedTools")
            .map(|index| claude[index + 1])
            .unwrap();
        for tool in ["Edit", "Write", "NotebookEdit"] {
            assert!(disallowed.split(',').any(|listed| listed == tool), "{tool}");
        }
        assert!(!claude.contains(&"acceptEdits"));
        assert!(!claude.contains(&"--allowedTools"));
        let codex = dispatch_arguments(Provider::Codex, PermissionMode::ReadOnly);
        let sandbox = codex
            .iter()
            .position(|argument| *argument == "--sandbox")
            .map(|index| codex[index + 1])
            .unwrap();
        assert_eq!(sandbox, "read-only");
        assert!(!codex.contains(&"workspace-write"));
    }

    #[test]
    fn no_mode_ever_emits_a_bypass_flag() {
        for provider in Provider::ALL {
            for mode in PermissionMode::ALL {
                let arguments = dispatch_arguments(provider, mode);
                assert!(refuse_bypass(arguments.iter().map(OsStr::new)).is_ok());
                assert!(!arguments
                    .iter()
                    .any(|argument| argument.contains("dangerous")));
            }
        }
    }

    #[test]
    fn bypass_flags_are_refused_anywhere_in_the_argv() {
        for flag in FORBIDDEN_FLAGS {
            let arguments = ["--model", "x", flag, "--print"];
            assert_eq!(
                refuse_bypass(arguments.iter().map(OsStr::new)).unwrap_err(),
                format!("Refused to launch a provider CLI with {flag}.")
            );
        }
    }

    #[test]
    fn worktree_auto_is_refused_outside_an_isolated_worktree() {
        let worktree = Isolation::Worktree {
            repository: PathBuf::from("repo"),
        };
        assert!(PermissionMode::WorktreeAuto
            .require_isolation(Some(&worktree))
            .is_ok());
        for isolation in [None, Some(&Isolation::Directory)] {
            assert!(PermissionMode::WorktreeAuto
                .require_isolation(isolation)
                .unwrap_err()
                .contains("not an isolated git worktree"));
        }
        for mode in [
            PermissionMode::ReadOnly,
            PermissionMode::Supervised,
            PermissionMode::AutoAcceptEdits,
            PermissionMode::WorkspaceAuto,
        ] {
            assert!(mode.require_isolation(Some(&Isolation::Directory)).is_ok());
            assert!(mode.require_isolation(None).is_ok());
        }
    }

    #[test]
    fn role_defaults_follow_the_ladder() {
        for role in ["playtester", "Reviewer", "QA", "security-audit"] {
            assert_eq!(
                PermissionMode::default_for_role(role),
                PermissionMode::ReadOnly
            );
        }
        for role in ["copywriter", "docs", "Writer"] {
            assert_eq!(
                PermissionMode::default_for_role(role),
                PermissionMode::AutoAcceptEdits
            );
        }
        for role in ["lead", "entry", "builder", "commander"] {
            assert_eq!(
                PermissionMode::default_for_role(role),
                PermissionMode::Supervised
            );
        }
    }

    #[test]
    fn modes_parse_from_their_flags_and_round_trip_through_serde() {
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::parse(mode.flag()).unwrap(), mode);
            let text = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<PermissionMode>(&text).unwrap(), mode);
        }
        assert_eq!(
            serde_json::to_string(&PermissionMode::AutoAcceptEdits).unwrap(),
            "\"auto-accept-edits\""
        );
        assert!(PermissionMode::parse("full-access")
            .unwrap_err()
            .contains(MODE_USAGE));
    }

    #[test]
    fn commander_defaults_to_workspace_auto_without_any_bypass_flag() {
        let commander = PermissionMode::COMMANDER_DEFAULT;
        assert_eq!(commander, PermissionMode::WorkspaceAuto);
        assert!(commander.require_isolation(None).is_ok());
        let claude = dispatch_arguments(Provider::Claude, commander);
        let joined = claude.join(" ");
        assert!(
            joined.contains(
                "--permission-mode acceptEdits --allowedTools Bash,Edit,Write,NotebookEdit"
            ),
            "{joined}"
        );
        assert!(!joined.contains("--permission-mode default"));
        assert!(refuse_bypass(claude.iter().map(OsStr::new)).is_ok());
        let mut smuggled = claude.clone();
        for flag in FORBIDDEN_FLAGS {
            smuggled.push(flag);
            assert!(refuse_bypass(smuggled.iter().map(OsStr::new)).is_err());
            smuggled.pop();
        }
        assert_eq!(
            serde_json::to_string(&commander).unwrap(),
            "\"workspace-auto\""
        );
    }

    #[test]
    fn commander_modes_parse_cycle_and_refuse_worktree_auto() {
        for mode in PermissionMode::COMMANDER {
            assert_eq!(PermissionMode::parse_commander(mode.flag()).unwrap(), mode);
        }
        assert!(PermissionMode::parse_commander("worktree-auto")
            .unwrap_err()
            .contains(COMMANDER_MODE_USAGE));
        let mut seen = Vec::new();
        let mut mode = PermissionMode::COMMANDER_DEFAULT;
        for _ in PermissionMode::COMMANDER {
            mode = mode.next_commander();
            seen.push(mode);
        }
        assert_eq!(seen.last(), Some(&PermissionMode::COMMANDER_DEFAULT));
        assert_eq!(seen.len(), PermissionMode::COMMANDER.len());
        assert_eq!(
            PermissionMode::WorktreeAuto.next_commander(),
            PermissionMode::ReadOnly
        );
    }
}
