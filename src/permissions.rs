use crate::{
    auth::Provider,
    prodgate::{ProdGate, ProdGateDecision},
    specialists::Isolation,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    #[default]
    Supervised,
    AutoAcceptEdits,
    WorktreeAuto,
    WorkspaceAuto,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: PermissionEffect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionResolution {
    pub effect: PermissionEffect,
    pub reason: Option<String>,
    pub prod_gate: Option<ProdGateDecision>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionPolicy {
    pub overrides: Vec<PermissionRule>,
    pub restrictions: Vec<PermissionRule>,
    pub fallbacks: Vec<PermissionRule>,
    pub prod_gate_enabled: bool,
    pub extra_denied_commands: Vec<String>,
    pub unattended: bool,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            overrides: Vec::new(),
            restrictions: Vec::new(),
            fallbacks: Vec::new(),
            prod_gate_enabled: true,
            extra_denied_commands: Vec::new(),
            unattended: false,
        }
    }
}

impl PermissionPolicy {
    pub fn for_mode(mode: PermissionMode) -> Self {
        let mut policy = Self::default();
        match mode {
            PermissionMode::ReadOnly => {
                for tool in ["Read", "Glob", "Grep", "LS", "WebFetch", "WebSearch"] {
                    policy.push_override(tool, "*", PermissionEffect::Allow);
                }
                for tool in ["Edit", "Write", "NotebookEdit", "Bash", "execute", "shell"] {
                    policy.push_restriction(tool, "*", PermissionEffect::Deny);
                }
            }
            PermissionMode::Supervised => {
                for tool in ["Read", "Glob", "Grep", "LS"] {
                    policy.push_override(tool, "*", PermissionEffect::Allow);
                }
                policy.push_override("*", "*", PermissionEffect::Ask);
            }
            PermissionMode::AutoAcceptEdits => {
                for tool in [
                    "Read",
                    "Glob",
                    "Grep",
                    "LS",
                    "Edit",
                    "Write",
                    "NotebookEdit",
                ] {
                    policy.push_override(tool, "*", PermissionEffect::Allow);
                }
            }
            PermissionMode::WorktreeAuto | PermissionMode::WorkspaceAuto => {
                policy.push_override("*", "*", PermissionEffect::Allow);
            }
        }
        policy
    }

    pub fn push_override(
        &mut self,
        action: impl Into<String>,
        resource: impl Into<String>,
        effect: PermissionEffect,
    ) {
        self.overrides.push(PermissionRule {
            action: action.into(),
            resource: resource.into(),
            effect,
        });
    }

    pub fn push_restriction(
        &mut self,
        action: impl Into<String>,
        resource: impl Into<String>,
        effect: PermissionEffect,
    ) {
        self.restrictions.push(PermissionRule {
            action: action.into(),
            resource: resource.into(),
            effect,
        });
    }

    pub fn push_fallback(
        &mut self,
        action: impl Into<String>,
        resource: impl Into<String>,
        effect: PermissionEffect,
    ) {
        self.fallbacks.push(PermissionRule {
            action: action.into(),
            resource: resource.into(),
            effect,
        });
    }

    pub fn evaluate(&self, tool_name: &str, resource: &str, input: &Value) -> PermissionResolution {
        let mut effect = PermissionEffect::Ask;
        let mut reason: Option<String> = None;

        for rule in &self.overrides {
            if rule_matches(rule, tool_name, resource) {
                effect = rule.effect;
            }
        }

        let prod_gate = if self.prod_gate_enabled {
            ProdGate::check_with_extra(tool_name, input, &self.extra_denied_commands)
        } else {
            None
        };

        let mut restricted_to_ask_or_deny = false;
        if let Some(hit) = &prod_gate {
            let floor = if self.unattended {
                PermissionEffect::Deny
            } else {
                PermissionEffect::Ask
            };
            if floor > effect {
                effect = floor;
            }
            reason = Some(hit.reason.clone());
            restricted_to_ask_or_deny = true;
        }

        for rule in &self.restrictions {
            if rule_matches(rule, tool_name, resource) && rule.effect > effect {
                effect = rule.effect;
                if rule.effect >= PermissionEffect::Ask {
                    restricted_to_ask_or_deny = true;
                }
                if reason.is_none() {
                    reason = Some(format!(
                        "Restricted by policy rule ({}: {}).",
                        rule.action, rule.resource
                    ));
                }
            }
        }

        if effect == PermissionEffect::Ask && !restricted_to_ask_or_deny {
            for rule in &self.fallbacks {
                if rule_matches(rule, tool_name, resource) {
                    effect = rule.effect;
                }
            }
        }

        if self.unattended && effect == PermissionEffect::Ask {
            effect = PermissionEffect::Deny;
            if reason.is_none() {
                reason = Some("Unattended execution denies interactive permission prompts.".into());
            }
        }

        PermissionResolution {
            effect,
            reason,
            prod_gate,
        }
    }
}

fn rule_matches(rule: &PermissionRule, action: &str, resource: &str) -> bool {
    glob_match(
        &rule.action.to_ascii_lowercase(),
        &action.to_ascii_lowercase(),
    ) && glob_match(&rule.resource, resource)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;
    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star_idx = Some(p);
            match_idx = t;
            p += 1;
        } else if let Some(star) = star_idx {
            p = star + 1;
            match_idx += 1;
            t = match_idx;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
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
const AGY_STREAM_ARGUMENTS: [&str; 5] = [
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--disable-slash-commands",
];
pub(crate) const AGY_READ_ONLY_SETTINGS: &str = r#"{"agentMode":"","toolPermission":"request-review","permissions":{"deny":["write_file(*)","command(*)"]}}"#;
const AGY_SUPERVISED_SETTINGS: &str = r#"{"agentMode":"","toolPermission":"request-review"}"#;
const CODEX_WORKSPACE_NETWORK_CONFIG: &str = "sandbox_workspace_write.network_access=true";
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

    pub fn flag(self) -> &'static str {
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
        Self::default_for_role_with_isolation(role, None)
    }

    pub fn default_for_role_with_isolation(role: &str, isolation: Option<&Isolation>) -> Self {
        let role = role.to_ascii_lowercase();
        let mentions = |words: &[&str]| words.iter().any(|word| role.contains(word));
        if mentions(&READ_ONLY_ROLE_WORDS) {
            Self::ReadOnly
        } else if mentions(&EDITING_ROLE_WORDS) {
            Self::AutoAcceptEdits
        } else if matches!(isolation, Some(Isolation::Worktree { .. })) {
            Self::WorktreeAuto
        } else {
            Self::WorkspaceAuto
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

    pub(crate) fn claude_arguments(self) -> Vec<&'static str> {
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

    pub(crate) fn codex_sandbox(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Supervised | Self::AutoAcceptEdits | Self::WorktreeAuto | Self::WorkspaceAuto => {
                "workspace-write"
            }
        }
    }

    pub(crate) fn codex_leading_arguments(self) -> Vec<&'static str> {
        let mut arguments = vec!["-a", "never"];
        if matches!(self, Self::WorktreeAuto | Self::WorkspaceAuto) {
            arguments.extend(["-c", CODEX_WORKSPACE_NETWORK_CONFIG]);
        }
        arguments
    }

    fn gemini_arguments(self) -> Vec<&'static str> {
        match self {
            Self::ReadOnly => vec!["--settings", AGY_READ_ONLY_SETTINGS],
            Self::Supervised => vec!["--settings", AGY_SUPERVISED_SETTINGS],
            Self::AutoAcceptEdits => vec!["--mode", "accept-edits"],
            Self::WorkspaceAuto | Self::WorktreeAuto => vec!["--mode", "auto"],
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
        Provider::Codex => {
            let mut arguments = mode.codex_leading_arguments();
            arguments.extend([
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                mode.codex_sandbox(),
                "-",
            ]);
            arguments
        }
        Provider::Gemini => {
            let mut arguments = AGY_STREAM_ARGUMENTS.to_vec();
            arguments.extend(mode.gemini_arguments());
            arguments
        }
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
    use serde_json::json;
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
        let sandboxed = |sandbox: &str| {
            format!("-a never exec --json --skip-git-repo-check --sandbox {sandbox} -")
        };
        assert_eq!(
            joined(Provider::Codex, PermissionMode::ReadOnly),
            sandboxed("read-only")
        );
        for mode in [PermissionMode::Supervised, PermissionMode::AutoAcceptEdits] {
            assert_eq!(joined(Provider::Codex, mode), sandboxed("workspace-write"));
        }
        let network_enabled = format!(
            "-a never -c {CODEX_WORKSPACE_NETWORK_CONFIG} exec --json --skip-git-repo-check --sandbox workspace-write -"
        );
        for mode in [PermissionMode::WorktreeAuto, PermissionMode::WorkspaceAuto] {
            assert_eq!(joined(Provider::Codex, mode), network_enabled);
        }
    }

    #[test]
    fn gemini_argv_for_every_mode_is_exact() {
        let base =
            "--input-format stream-json --output-format stream-json --disable-slash-commands";
        assert_eq!(
            joined(Provider::Gemini, PermissionMode::ReadOnly),
            format!("{base} --settings {AGY_READ_ONLY_SETTINGS}")
        );
        assert_eq!(
            joined(Provider::Gemini, PermissionMode::Supervised),
            format!("{base} --settings {AGY_SUPERVISED_SETTINGS}")
        );
        assert_eq!(
            joined(Provider::Gemini, PermissionMode::AutoAcceptEdits),
            format!("{base} --mode accept-edits")
        );
        for mode in [PermissionMode::WorkspaceAuto, PermissionMode::WorktreeAuto] {
            assert_eq!(
                joined(Provider::Gemini, mode),
                format!("{base} --mode auto")
            );
        }
    }

    #[test]
    fn read_only_locks_every_write_tool_on_all_providers() {
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
        assert!(!codex.contains(&CODEX_WORKSPACE_NETWORK_CONFIG));
        let gemini = dispatch_arguments(Provider::Gemini, PermissionMode::ReadOnly);
        let settings_raw = gemini
            .iter()
            .position(|argument| *argument == "--settings")
            .map(|index| gemini[index + 1])
            .unwrap();
        let settings: Value = serde_json::from_str(settings_raw).unwrap();
        assert_eq!(settings["agentMode"], "");
        assert_eq!(settings["toolPermission"], "request-review");
        let denied = settings["permissions"]["deny"].as_array().unwrap();
        assert!(denied.contains(&json!("write_file(*)")));
        assert!(denied.contains(&json!("command(*)")));
        assert!(!gemini.contains(&"--mode"));
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
    fn role_defaults_never_cripple_builders_or_commander_with_supervised() {
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
        let worktree = Isolation::Worktree {
            repository: PathBuf::from("repo"),
        };
        for role in ["lead", "entry", "builder", "commander"] {
            assert_eq!(
                PermissionMode::default_for_role(role),
                PermissionMode::WorkspaceAuto
            );
            assert_eq!(
                PermissionMode::default_for_role_with_isolation(role, Some(&worktree)),
                PermissionMode::WorktreeAuto
            );
        }
    }

    #[test]
    fn three_layer_policy_enforces_monotone_restrictions_and_prod_gate_over_overrides_and_fallbacks(
    ) {
        let mut policy = PermissionPolicy::for_mode(PermissionMode::WorkspaceAuto);
        policy.push_fallback("Bash", "*", PermissionEffect::Allow);

        let safe = policy.evaluate("Bash", "cargo test", &json!({"command": "cargo test"}));
        assert_eq!(safe.effect, PermissionEffect::Allow);
        assert!(safe.prod_gate.is_none());

        let prod_hit = policy.evaluate(
            "Bash",
            "git push --force",
            &json!({"command": "git push origin main --force"}),
        );
        assert_eq!(prod_hit.effect, PermissionEffect::Ask);
        assert!(prod_hit.prod_gate.is_some());

        policy.unattended = true;
        let unattended_hit = policy.evaluate(
            "Bash",
            "kubectl delete pod web",
            &json!({"command": "kubectl delete pod web"}),
        );
        assert_eq!(unattended_hit.effect, PermissionEffect::Deny);

        let mut supervised = PermissionPolicy::for_mode(PermissionMode::Supervised);
        supervised.push_fallback("WebFetch", "https://docs.rs/*", PermissionEffect::Allow);
        supervised.push_restriction(
            "WebFetch",
            "https://internal.corp/*",
            PermissionEffect::Deny,
        );
        assert_eq!(
            supervised
                .evaluate("WebFetch", "https://docs.rs/ratatui", &json!({}))
                .effect,
            PermissionEffect::Allow
        );
        assert_eq!(
            supervised
                .evaluate("WebFetch", "https://internal.corp/secret", &json!({}))
                .effect,
            PermissionEffect::Deny
        );
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
        let codex = dispatch_arguments(Provider::Codex, commander);
        assert!(codex.windows(2).any(|pair| pair == ["-a", "never"]));
        assert!(codex
            .windows(2)
            .any(|pair| pair == ["-c", CODEX_WORKSPACE_NETWORK_CONFIG]));
        assert!(codex
            .windows(2)
            .any(|pair| pair == ["--sandbox", "workspace-write"]));
        assert!(refuse_bypass(codex.iter().map(OsStr::new)).is_ok());
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
