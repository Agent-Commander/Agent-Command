use serde::{Deserialize, Serialize};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

pub const SOUL_FILE_NAME: &str = "SOUL.md";
pub const MAX_LEARNING_LINES: usize = 100;
pub const LIGHTWEIGHT_EXCERPT_CHARS: usize = 600;
pub const DEFAULT_HANDOFF_TOKENS: usize = 16_000;
pub const CHARS_PER_TOKEN_APPROX: usize = 4;

const CONFIG_DIR_NAME: &str = "agent-command";
const LEARNINGS_HEADING: &str = "## Learnings";
const MIN_HANDOFF_TOKENS: usize = 256;
const MIN_SECRET_HEX_LEN: usize = 48;
const MIN_BEARER_TOKEN_LEN: usize = 16;

const SECRET_PREFIXES: [&str; 13] = [
    "sk-ant-",
    "sk-proj-",
    "sk-live-",
    "sk-test-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "aiza",
    "akia",
    "asia",
];

const SLACK_PREFIXES: [&str; 3] = ["xoxb-", "xoxp-", "xoxs-"];

const SECRET_ASSIGNMENT_KEYS: [&str; 8] = [
    "api_key=",
    "apikey=",
    "secret=",
    "password=",
    "passwd=",
    "token=",
    "access_key=",
    "private_key=",
];

const PASSIVE_STALL_PHRASES: [&str; 4] = [
    "read-only until confirmed",
    "read only until confirmed",
    "do not modify files until confirmed",
    "wait for confirmation before taking any action",
];

const DEFAULT_SOUL_TEMPLATE: &str = "\
# Commander Soul

## Identity
You are Commander, the single-entry operator for Agent Command. You talk directly to the user, act decisively on their behalf, and coordinate parallel workers when work benefits from isolation or concurrency.

## Style
- Lead with the concrete result in plain English.
- Keep routine coordination and plumbing quiet unless the user asks for details.
- Check files, git status, and gh directly before asking questions.
- Own delegated work end-to-end; fix or re-route worker failures before surfacing anything.
- Never argue by citing policy or rules; follow explicit user overrides directly.

## Learnings
";

pub fn default_soul_template() -> &'static str {
    DEFAULT_SOUL_TEMPLATE
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoulStore {
    path: PathBuf,
}

impl SoulStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> Option<PathBuf> {
        config_directory().map(|dir| dir.join(SOUL_FILE_NAME))
    }

    pub fn from_env() -> Option<Self> {
        Self::default_path().map(Self::new)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_initialized(&self) -> io::Result<bool> {
        if self.path.exists() {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
        {
            Ok(mut file) => {
                use io::Write;
                file.write_all(DEFAULT_SOUL_TEMPLATE.as_bytes())?;
                file.sync_all()?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn read(&self) -> io::Result<String> {
        let _ = self.ensure_initialized()?;
        fs::read_to_string(&self.path)
    }

    pub fn append_learning(&self, note: &str) -> io::Result<()> {
        if contains_secret_like(note) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Refused to persist secret-like material into SOUL.md.",
            ));
        }
        let cleaned = sanitize_learning_note(note)?;
        if contains_secret_like(&cleaned) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Refused to persist secret-like material into SOUL.md.",
            ));
        }
        let current = self.read()?;
        let updated = merge_learning_section(&current, &cleaned, MAX_LEARNING_LINES);
        write_atomic(&self.path, updated.as_bytes())
    }

    pub fn search_learnings(&self, query: &str) -> io::Result<Vec<String>> {
        let needle = query.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let content = self.read()?;
        let terms: Vec<&str> = needle.split_whitespace().collect();
        let (_, learnings) = split_learnings(&content);
        Ok(learnings
            .into_iter()
            .filter(|line| {
                let lowered = line.to_ascii_lowercase();
                terms.iter().all(|term| lowered.contains(term))
            })
            .collect())
    }

    pub fn excerpt(&self, max_chars: usize) -> io::Result<String> {
        let content = self.read()?;
        Ok(excerpt_text(&content, max_chars))
    }
}

fn config_directory() -> Option<PathBuf> {
    if let Some(base) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(base).join(CONFIG_DIR_NAME));
    }
    if let Some(base) = env::var_os("APPDATA").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(base).join(CONFIG_DIR_NAME));
    }
    env::home_dir().map(|home| home.join(".config").join(CONFIG_DIR_NAME))
}

fn sanitize_learning_note(note: &str) -> io::Result<String> {
    if note.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Learning note must not contain NUL bytes.",
        ));
    }
    let joined = note.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = joined
        .strip_prefix("- ")
        .unwrap_or(&joined)
        .trim()
        .to_owned();
    if collapsed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Learning note must not be empty.",
        ));
    }
    Ok(collapsed)
}

fn split_learnings(content: &str) -> (String, Vec<String>) {
    let mut prefix_lines = Vec::new();
    let mut learnings = Vec::new();
    let mut in_learnings = false;
    for line in content.lines() {
        if !in_learnings && line.trim().eq_ignore_ascii_case(LEARNINGS_HEADING) {
            in_learnings = true;
            continue;
        }
        if in_learnings {
            if line.trim_start().starts_with("## ") {
                in_learnings = false;
                prefix_lines.push(line.to_owned());
                continue;
            }
            let item = line.trim().trim_start_matches('-').trim();
            if !item.is_empty() {
                learnings.push(item.to_owned());
            }
        } else {
            prefix_lines.push(line.to_owned());
        }
    }
    let prefix = prefix_lines.join("\n").trim_end().to_owned();
    (prefix, learnings)
}

fn merge_learning_section(content: &str, note: &str, max_lines: usize) -> String {
    let (prefix, mut learnings) = split_learnings(content);
    let already_present = learnings
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(note));
    if !already_present {
        learnings.push(note.to_owned());
    }
    let overflow = learnings.len().saturating_sub(max_lines);
    if overflow > 0 {
        learnings.drain(..overflow);
    }
    let mut output = String::new();
    if !prefix.is_empty() {
        output.push_str(&prefix);
        output.push_str("\n\n");
    }
    output.push_str(LEARNINGS_HEADING);
    output.push('\n');
    for item in learnings {
        output.push_str("- ");
        output.push_str(&item);
        output.push('\n');
    }
    output
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let mut file = fs::File::create_new(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)
}

pub fn contains_secret_like(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("-----begin ") && lowered.contains("key-----") {
        return true;
    }
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| lowered.contains(prefix))
        || SLACK_PREFIXES.iter().any(|prefix| lowered.contains(prefix))
    {
        return true;
    }
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|ch: char| "\"'`.,;:()[]{}<>".contains(ch));
        let lower_token = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower_token.strip_prefix("sk-") {
            if rest.len() >= 16
                && rest
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            {
                return true;
            }
        }
        if SECRET_ASSIGNMENT_KEYS.iter().any(|key| {
            lower_token
                .strip_prefix(key)
                .is_some_and(|val| val.trim_matches(|ch: char| "\"'".contains(ch)).len() >= 6)
        }) {
            return true;
        }
        if trimmed.len() >= MIN_SECRET_HEX_LEN
            && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return true;
        }
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    for pair in words.windows(2) {
        if pair[0].eq_ignore_ascii_case("bearer") {
            let credential = pair[1].trim_matches(|ch: char| "\"'`.,;:()[]{}<>".contains(ch));
            if credential.len() >= MIN_BEARER_TOKEN_LEN
                && credential.chars().all(|ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+' | '/' | '=')
                })
            {
                return true;
            }
        }
    }
    false
}

pub fn redact_secrets(text: &str) -> String {
    if !contains_secret_like(text) {
        return text.to_owned();
    }
    let mut out = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        if word.eq_ignore_ascii_case("bearer")
            && words.get(index + 1).is_some_and(|next| {
                let cleaned = next.trim_matches(|ch: char| "\"'`.,;:()[]{}<>".contains(ch));
                cleaned.len() >= MIN_BEARER_TOKEN_LEN
            })
        {
            out.push("Bearer [REDACTED]".to_owned());
            index += 2;
            continue;
        }
        if contains_secret_like(word) {
            out.push("[REDACTED]".to_owned());
        } else {
            out.push(word.to_owned());
        }
        index += 1;
    }
    out.join(" ")
}

pub fn excerpt_text(content: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let (prefix, learnings) = split_learnings(content);
    let primary = if prefix.trim().is_empty() {
        learnings.join("\n")
    } else {
        prefix
    };
    let normalized = primary
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    truncate_chars(&normalized, max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 1 {
        return "…".chars().take(max_chars).collect();
    }
    let mut clipped: String = text.chars().take(max_chars - 1).collect();
    if let Some(pos) = clipped.rfind(|ch: char| ch.is_whitespace()) {
        if pos > clipped.len() / 2 {
            clipped.truncate(pos);
        }
    }
    clipped = clipped.trim_end().to_owned();
    clipped.push('…');
    clipped
}

pub fn strip_passive_stall_phrases(text: &str) -> String {
    let mut cleaned = Vec::new();
    for line in text.lines() {
        let lowered = line.to_ascii_lowercase();
        if PASSIVE_STALL_PHRASES
            .iter()
            .any(|phrase| lowered.contains(phrase))
        {
            continue;
        }
        cleaned.push(line);
    }
    cleaned.join("\n")
}

pub fn commander_system_prompt(
    soul: &str,
    workspace: &Path,
    gh_context: Option<&str>,
    workers_summary: Option<&str>,
    tools_summary: Option<&str>,
) -> String {
    let sanitized_soul = strip_passive_stall_phrases(soul.trim());
    let mut sections = vec![
        format!(
            "You are Commander, the single-entry operator for Agent Command.\n\
             Workspace: {}",
            workspace.display()
        ),
        "Operating Charter:\n\
         - Act immediately on reversible work without asking permission. Inspect files, git state, and gh before asking the user.\n\
         - Own delegated work end-to-end: when a worker fails, stalls, or misses part of the ask, fix it yourself or re-route to another worker before surfacing anything to the user.\n\
         - Delegate independent or isolated work using structured worker tools (spawn_worker, message_worker, read_worker, list_workers, interrupt_worker). Never sleep or poll waiting for workers; reactive <background-result> wakeups arrive when workers finish.\n\
         - Never argue by citing policy or rules. Follow explicit user overrides directly and briefly note which default you set aside.\n\
         - Pause ONLY for: (1) spending real money, (2) ProdGate irreversible destructive or external actions (force-push, production deploy, deleting data you did not create, sending external messages in the user's name), or (3) genuine unresolvable ambiguity where only the user knows the intent.\n\
         - If the user tells you to stop or cancel, halt immediately and interrupt active workers.\n\
         - Lead with plain-English results first. Keep routine coordination internal unless the user asks."
            .to_owned(),
        "Environment & Tool Routing:\n\
         - You are connected to the `agent_command` loopback MCP server. Prefer its structured tools over ad-hoc shell scripts:\n\
         - Environment & Status: call `env_status` for workspace, git status, active provider/profile/mode, quota headroom, and vault/inbox/ledger counts.\n\
         - Workers: use `spawn_worker`, `message_worker`, `read_worker`, `list_workers`, `interrupt_worker`; rely on `<background-result>` wakeups rather than polling.\n\
         - Inbox & Decisions: use `inbox_list`, `inbox_post`, `inbox_submit_decision`, `inbox_update_decision`, `inbox_dismiss` (1 decision = 1 card, concrete recommendation + options, never `TBD`).\n\
         - Ledger & Task Docs: use `ledger_list`, `ledger_record`, `ledger_update`, `ledger_read_doc`, `ledger_append_doc` to keep durable task state and Markdown docs current.\n\
         - Rich Chat UI: use `ui_post_table` and `ui_post_card` to render structured comparisons, metrics, status cards, or checklists directly in chat.\n\
         - Credentials (Zero-Leak): call `vault_list` / `vault_check` first; if a secret is missing, call `vault_request` so the user enters it in a masked prompt. NEVER ask for raw secrets in chat and NEVER print, log, or pass secret values in shell arguments.\n\
         - Two-Tier Memory: use `memory_search`, `memory_write`, `memory_supersede`, `memory_delete` (`shared` for cross-bot facts/decisions/conventions/warnings, `self` for bot-private playbooks/recipes/corrections; secrets are refused).\n\
         - Bot Forge & Evolution: use `bot_scorecard` for bot health, `bot_forge` to create narrowly scoped specialists, `bot_record_feedback` for outcomes, `bot_evolve` for probation-gated charter mutations with LKG rollback, and `bot_decommission` (requires operator confirmation for `pinned_core` bots).\n\
         - GitHub & External MCP: use `github_pr_*` / `github_issue_*` for GitHub, and `tool_search` -> `tool_describe` -> `tool_call` for user-configured external MCP servers."
            .to_owned(),
    ];

    if let Some(gh) = gh_context.map(str::trim).filter(|text| !text.is_empty()) {
        sections.push(format!("GitHub Context:\n{gh}"));
    }
    if let Some(workers) = workers_summary
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        sections.push(format!("Active Workers:\n{workers}"));
    }
    if let Some(tools) = tools_summary.map(str::trim).filter(|text| !text.is_empty()) {
        sections.push(format!("Available Tools:\n{tools}"));
    }
    if !sanitized_soul.is_empty() {
        sections.push(format!("Persona & Durable Learnings:\n{sanitized_soul}"));
    }
    sections.join("\n\n")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnRole {
    User,
    Assistant,
    Tool,
    System,
}

impl TurnRole {
    fn label(self) -> &'static str {
        match self {
            Self::User => "USER",
            Self::Assistant => "ASSISTANT",
            Self::Tool => "TOOL",
            Self::System => "SYSTEM",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HandoffTurn {
    pub seq: u64,
    pub role: TurnRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_outcome: Option<String>,
}

impl HandoffTurn {
    fn render(&self, max_chars: usize) -> String {
        let mut block = format!(
            "[#{} {}] {}",
            self.seq,
            self.role.label(),
            self.content.trim()
        );
        if let Some(outcome) = self
            .command_outcome
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            block.push_str("\n  outcome: ");
            block.push_str(outcome);
        }
        truncate_chars(&block, max_chars)
    }
}

pub fn build_portable_handoff(turns: &[HandoffTurn], max_tokens_approx: usize) -> String {
    let header = "Portable cross-provider handoff (recent context slice). \
                  If you need earlier turns omitted below, call read_worker or search_history (or read_thread) to inspect them directly.";
    if turns.is_empty() {
        return format!("{header}\n\nNo prior turns recorded.");
    }
    let char_budget = max_tokens_approx
        .max(MIN_HANDOFF_TOKENS)
        .saturating_mul(CHARS_PER_TOKEN_APPROX);
    let header_len = header.chars().count() + 2;
    let body_budget = char_budget.saturating_sub(header_len);

    let goal_index = turns
        .iter()
        .position(|turn| turn.role == TurnRole::User)
        .unwrap_or(0);
    let max_goal_chars = (body_budget / 3).max(200);
    let goal_rendered = turns[goal_index].render(max_goal_chars);

    let mut chosen = vec![false; turns.len()];
    let mut rendered: Vec<Option<String>> = vec![None; turns.len()];
    chosen[goal_index] = true;
    rendered[goal_index] = Some(goal_rendered.clone());
    let mut used = goal_rendered.chars().count() + 1;

    let marker_reserve = 240usize;
    let per_turn_cap = (body_budget / 2).max(240);
    let guaranteed_tail = 4usize;
    let mut tail_added = 0usize;

    for index in (0..turns.len()).rev() {
        if chosen[index] {
            continue;
        }
        if tail_added >= guaranteed_tail {
            break;
        }
        let block = turns[index].render(per_turn_cap);
        let cost = block.chars().count() + 1;
        if used + cost + marker_reserve > body_budget {
            break;
        }
        chosen[index] = true;
        rendered[index] = Some(block);
        used += cost;
        tail_added += 1;
    }

    for index in (0..turns.len()).rev() {
        if chosen[index] {
            continue;
        }
        let has_outcome = turns[index]
            .command_outcome
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty());
        if !has_outcome {
            continue;
        }
        let block = turns[index].render(per_turn_cap.min(600));
        let cost = block.chars().count() + 1;
        if used + cost + marker_reserve > body_budget {
            continue;
        }
        chosen[index] = true;
        rendered[index] = Some(block);
        used += cost;
    }

    for index in (0..turns.len()).rev() {
        if chosen[index] {
            continue;
        }
        let block = turns[index].render(per_turn_cap);
        let cost = block.chars().count() + 1;
        if used + cost + marker_reserve > body_budget {
            break;
        }
        chosen[index] = true;
        rendered[index] = Some(block);
        used += cost;
    }

    let mut output = String::with_capacity(used + header_len + marker_reserve);
    output.push_str(header);
    output.push_str("\n\n");

    let mut index = 0;
    while index < turns.len() {
        if chosen[index] {
            if let Some(block) = &rendered[index] {
                output.push_str(block);
                output.push('\n');
            }
            index += 1;
        } else {
            let start = index;
            while index < turns.len() && !chosen[index] {
                index += 1;
            }
            let omitted = index - start;
            let first_seq = turns[start].seq;
            let last_seq = turns[index - 1].seq;
            output.push_str(&format!(
                "[... {omitted} turns omitted (#{first_seq}..=#{last_seq}); use read_worker or search_history for details ...]\n"
            ));
        }
    }

    if output.chars().count() > char_budget {
        truncate_chars(&output, char_budget)
    } else {
        output.trim_end().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "ac-soul-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn soul_store_initializes_once_and_preserves_user_edits() {
        let scratch = Scratch::new("init");
        let store = SoulStore::new(scratch.0.join(SOUL_FILE_NAME));
        assert!(store.ensure_initialized().unwrap());
        assert!(!store.ensure_initialized().unwrap());
        let initial = store.read().unwrap();
        assert!(initial.contains("# Commander Soul"));
        assert!(initial.contains(LEARNINGS_HEADING));

        let custom =
            "# My Custom Commander\nAlways use pnpm.\n\n## Learnings\n- Prefer terse diffs\n";
        fs::write(store.path(), custom).unwrap();
        assert!(!store.ensure_initialized().unwrap());
        assert_eq!(store.read().unwrap(), custom);
    }

    #[test]
    fn append_learning_bounds_lines_deduplicates_and_refuses_secrets() {
        let scratch = Scratch::new("learnings");
        let store = SoulStore::new(scratch.0.join(SOUL_FILE_NAME));
        store
            .append_learning("Use cargo nextest when installed")
            .unwrap();
        store
            .append_learning("use cargo nextest when installed")
            .unwrap();
        assert_eq!(store.search_learnings("nextest").unwrap().len(), 1);

        for secret in [
            "Use sk-proj-0123456789abcdefghijklmnop for openai",
            "GitHub token ghp_1234567890abcdef1234567890abcdef1234",
            "Bearer abcdefghijklmnopqrstuvwxyz123456",
            "api_key=supersecretvalue123",
            "-----BEGIN RSA PRIVATE KEY----- data",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let error = store.append_learning(secret).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{secret}");
        }

        for index in 0..120 {
            store
                .append_learning(&format!("Durable preference number {index}"))
                .unwrap();
        }
        let content = store.read().unwrap();
        let (_, learnings) = split_learnings(&content);
        assert_eq!(learnings.len(), MAX_LEARNING_LINES);
        assert_eq!(learnings.first().unwrap(), "Durable preference number 20");
        assert_eq!(learnings.last().unwrap(), "Durable preference number 119");
        assert!(store.search_learnings("number 119").unwrap().len() == 1);
    }

    #[test]
    fn excerpt_is_bounded_and_prioritizes_identity_before_learnings() {
        let content = format!(
            "# Commander Soul\nDirect operator.\n\n## Learnings\n{}\n",
            "- extra learning line\n".repeat(100)
        );
        let short = excerpt_text(&content, LIGHTWEIGHT_EXCERPT_CHARS);
        assert!(short.chars().count() <= LIGHTWEIGHT_EXCERPT_CHARS);
        assert!(short.contains("Direct operator."));
        assert!(!short.contains("extra learning line"));

        let huge = format!("# Soul\n{}", "λ ".repeat(500));
        let clipped = excerpt_text(&huge, 60);
        assert!(clipped.chars().count() <= 60);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn commander_system_prompt_enforces_non_arguing_charter_and_strips_stall_wording() {
        let soul = "Prefer short replies.\nStay read-only until confirmed.\nUse Rust idioms.";
        let prompt = commander_system_prompt(
            soul,
            Path::new("C:\\repo"),
            Some("gh authenticated as @dev on github.com"),
            Some("- nova (builder, RUNNING)"),
            Some("spawn_worker, github_pr_create"),
        );
        assert!(prompt.contains("Act immediately on reversible work"));
        assert!(prompt.contains("Own delegated work end-to-end"));
        assert!(prompt.contains("Never argue by citing policy or rules"));
        assert!(prompt.contains("spending real money"));
        assert!(prompt.contains("ProdGate"));
        assert!(prompt.contains("plain-English results first"));
        assert!(prompt.contains("Environment & Tool Routing:"));
        assert!(prompt.contains("agent_command"));
        assert!(prompt.contains("env_status"));
        assert!(prompt.contains("inbox_submit_decision"));
        assert!(prompt.contains("ledger_read_doc"));
        assert!(prompt.contains("ui_post_table"));
        assert!(prompt.contains("vault_request"));
        assert!(prompt.contains("memory_search"));
        assert!(prompt.contains("bot_forge"));
        assert!(prompt.contains("bot_decommission"));
        assert!(prompt.contains("@dev"));
        assert!(prompt.contains("nova (builder, RUNNING)"));
        assert!(prompt.contains("spawn_worker"));
        assert!(prompt.contains("Use Rust idioms."));
        assert!(!prompt
            .to_ascii_lowercase()
            .contains("read-only until confirmed"));
    }

    #[test]
    fn portable_handoff_keeps_goal_recent_turns_and_outcomes_within_budget() {
        let mut turns = vec![HandoffTurn {
            seq: 1,
            role: TurnRole::User,
            content: "Original goal: migrate auth to passive detection.".into(),
            command_outcome: None,
        }];
        for seq in 2..=40 {
            turns.push(HandoffTurn {
                seq,
                role: if seq % 2 == 0 {
                    TurnRole::Assistant
                } else {
                    TurnRole::Tool
                },
                content: format!("Intermediate detail for step {seq} {}", "x".repeat(120)),
                command_outcome: (seq == 15).then(|| "cargo test failed: 1 auth test".into()),
            });
        }
        turns.push(HandoffTurn {
            seq: 41,
            role: TurnRole::User,
            content: "Latest follow-up: switch to Codex and finish fixing the test.".into(),
            command_outcome: None,
        });

        let budget_tokens = 450;
        let handoff = build_portable_handoff(&turns, budget_tokens);
        assert!(handoff.chars().count() <= budget_tokens * CHARS_PER_TOKEN_APPROX);
        assert!(handoff.contains("Original goal: migrate auth to passive detection."));
        assert!(handoff.contains("Latest follow-up: switch to Codex"));
        assert!(handoff.contains("turns omitted"));
        assert!(handoff.contains("read_worker"));
        assert!(handoff.contains("search_history"));

        let generous = build_portable_handoff(&turns, 900);
        assert!(generous.contains("cargo test failed: 1 auth test"));
    }

    #[test]
    fn redact_secrets_scrubs_credentials_while_preserving_plain_text() {
        assert!(!contains_secret_like("Use cargo fmt before committing"));
        let scrubbed = redact_secrets("Auth header Bearer supersecrettoken123456 failed");
        assert!(!scrubbed.contains("supersecrettoken123456"));
        assert!(scrubbed.contains("Bearer [REDACTED]"));
    }
}
