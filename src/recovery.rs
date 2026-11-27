use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const BACKOFF_BUSY_SECS: i64 = 15;
pub const BACKOFF_LEDGER_UNAVAILABLE_SECS: i64 = 60;
pub const BACKOFF_CONFLICT_SECS: i64 = 300;
pub const BACKOFF_CAPACITY_SECS: i64 = 900;
pub const BACKOFF_POLICY_SECS: i64 = 900;
const BRIEF_FIELD_LIMIT: usize = 300;
const BRIEF_FAILURE_LIMIT: usize = 20;
const MIN_DETAIL_CHARS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCause {
    LaunchRefused,
    ExecutionFailed,
    Stalled,
    NoDiffProduced,
    VerificationMutated,
    ChecksFailed,
    AuthOrPolicy,
    NetworkOrDisconnected,
}

impl FailureCause {
    pub fn label(self) -> &'static str {
        match self {
            Self::LaunchRefused => "launch_refused",
            Self::ExecutionFailed => "execution_failed",
            Self::Stalled => "stalled",
            Self::NoDiffProduced => "no_diff_produced",
            Self::VerificationMutated => "verification_mutated",
            Self::ChecksFailed => "checks_failed",
            Self::AuthOrPolicy => "auth_or_policy",
            Self::NetworkOrDisconnected => "network_or_disconnected",
        }
    }

    pub fn escalates_immediately(self) -> bool {
        self == Self::AuthOrPolicy
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    Requeue,
    ExtraFinishRun,
    RetryIsolatedWorker,
    Escalate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryRule {
    pub action: RecoveryAction,
    pub name: &'static str,
    pub budget: u32,
    pub description: &'static str,
}

pub const REQUEUE_RULE: RecoveryRule = RecoveryRule {
    action: RecoveryAction::Requeue,
    name: "requeue",
    budget: 1,
    description: "Run the same task revision once more on its admitted, connected route",
};

pub const EXTRA_FINISH_RULE: RecoveryRule = RecoveryRule {
    action: RecoveryAction::ExtraFinishRun,
    name: "extra_finish_run",
    budget: 1,
    description: "Run one bounded repair pass on the existing isolated branch",
};

pub const RETRY_ISOLATED_RULE: RecoveryRule = RecoveryRule {
    action: RecoveryAction::RetryIsolatedWorker,
    name: "retry_isolated_worker",
    budget: 1,
    description:
        "Retry once in an isolated workspace after transient network or disconnect failure",
};

pub fn recoveries_for(cause: FailureCause) -> &'static [RecoveryRule] {
    match cause {
        FailureCause::LaunchRefused
        | FailureCause::ExecutionFailed
        | FailureCause::Stalled
        | FailureCause::NoDiffProduced
        | FailureCause::ChecksFailed => &[REQUEUE_RULE],
        FailureCause::VerificationMutated => &[EXTRA_FINISH_RULE],
        FailureCause::NetworkOrDisconnected => &[RETRY_ISOLATED_RULE],
        FailureCause::AuthOrPolicy => &[],
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Started,
    Failed,
    Recovered,
    NotApplicable,
}

impl AttemptOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Failed => "failed",
            Self::Recovered => "recovered",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryAttempt {
    pub key: String,
    pub cause: FailureCause,
    pub recovery: String,
    pub number: u32,
    pub attempt_id: String,
    pub outcome: AttemptOutcome,
    pub detail: String,
    pub at: i64,
}

impl RecoveryAttempt {
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.recovery.trim().is_empty() {
            problems.push("attempt recovery must name the recovery tried".into());
        }
        if self.detail.trim().chars().count() < MIN_DETAIL_CHARS {
            problems.push("attempt detail must say concretely what was tried and happened".into());
        }
        if self.key.trim().is_empty() {
            problems.push("attempt key must not be empty".into());
        }
        problems
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscalationGate {
    pub ready: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    Retry(RecoveryRule),
    Escalate(EscalationGate),
    WaitInProgress,
    AlreadyRecovered,
}

pub fn deterministic_attempt_id(key: &str, recovery: &str, number: u32) -> String {
    let payload = serde_json::json!([key, recovery, number]);
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let hex: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    hex[..24].to_owned()
}

pub fn next_recovery(
    cause: FailureCause,
    attempts: &[RecoveryAttempt],
) -> Option<&'static RecoveryRule> {
    if attempts.iter().any(|attempt| {
        matches!(
            attempt.outcome,
            AttemptOutcome::Started | AttemptOutcome::Recovered
        )
    }) {
        return None;
    }
    for rule in recoveries_for(cause) {
        let mine: Vec<&RecoveryAttempt> = attempts
            .iter()
            .filter(|attempt| attempt.recovery == rule.name)
            .collect();
        let not_applicable = mine
            .iter()
            .filter(|attempt| attempt.outcome == AttemptOutcome::NotApplicable)
            .count();
        let failed = mine
            .iter()
            .filter(|attempt| attempt.outcome == AttemptOutcome::Failed)
            .count() as u32;
        if not_applicable == 0 && failed < rule.budget {
            return Some(rule);
        }
    }
    None
}

pub fn escalation_gate(cause: FailureCause, attempts: &[RecoveryAttempt]) -> EscalationGate {
    if attempts
        .iter()
        .any(|attempt| attempt.outcome == AttemptOutcome::Recovered)
    {
        return EscalationGate {
            ready: false,
            reasons: vec!["a recovery worked; nothing to escalate".into()],
        };
    }
    if attempts
        .iter()
        .any(|attempt| attempt.outcome == AttemptOutcome::Started)
    {
        return EscalationGate {
            ready: false,
            reasons: vec!["a recovery attempt is still running".into()],
        };
    }
    let mut missing = Vec::new();
    for rule in recoveries_for(cause) {
        let mine: Vec<&RecoveryAttempt> = attempts
            .iter()
            .filter(|attempt| attempt.recovery == rule.name)
            .collect();
        let not_applicable = mine
            .iter()
            .filter(|attempt| attempt.outcome == AttemptOutcome::NotApplicable)
            .count();
        let failed = mine
            .iter()
            .filter(|attempt| attempt.outcome == AttemptOutcome::Failed)
            .count() as u32;
        if not_applicable == 0 && failed < rule.budget {
            missing.push(format!(
                "{} ({}): {} of {} attempts recorded",
                rule.name, rule.description, failed, rule.budget
            ));
        }
    }
    EscalationGate {
        ready: missing.is_empty(),
        reasons: missing,
    }
}

pub fn decide_recovery(cause: FailureCause, attempts: &[RecoveryAttempt]) -> RecoveryDecision {
    if attempts
        .iter()
        .any(|attempt| attempt.outcome == AttemptOutcome::Recovered)
    {
        return RecoveryDecision::AlreadyRecovered;
    }
    if attempts
        .iter()
        .any(|attempt| attempt.outcome == AttemptOutcome::Started)
    {
        return RecoveryDecision::WaitInProgress;
    }
    if let Some(rule) = next_recovery(cause, attempts) {
        return RecoveryDecision::Retry(*rule);
    }
    RecoveryDecision::Escalate(escalation_gate(cause, attempts))
}

pub fn summarize_attempts(attempts: &[RecoveryAttempt]) -> String {
    attempts
        .iter()
        .map(|attempt| {
            let trimmed: String = attempt.detail.trim().chars().take(160).collect();
            format!(
                "{} {}: {}",
                attempt.recovery,
                attempt.outcome.label(),
                trimmed
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailedAttemptContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_summary: Option<String>,
    #[serde(default)]
    pub merged: bool,
}

pub fn retry_prompt_suffix(context: &FailedAttemptContext) -> String {
    let mut lines = vec!["Retry after prior failed attempt, recorded by Agent-Command:".to_owned()];
    if let Some(branch) = &context.branch {
        lines.push(format!("- Prior branch: {}", one_line(branch)));
    }
    if let Some(head) = &context.head {
        lines.push(format!("- Prior checked head: {}", one_line(head)));
    }
    lines.push(format!("- Failure reason: {}", one_line(&context.reason)));
    if let Some(summary) = &context.prior_summary {
        if !summary.trim().is_empty() {
            lines.push(format!("- Prior summary: {}", one_line(summary)));
        }
    }
    for check in context.failed_checks.iter().take(BRIEF_FAILURE_LIMIT) {
        lines.push(format!("- Failed: {}", one_line(check)));
    }
    let instruction = if context.merged {
        "The earlier branch was already merged: create a clean follow-up branch without mutating merged history."
    } else {
        "Reuse that branch and existing isolated checkout: build on top of it without force-pushing. Do not start over."
    };
    format!("{}\n\n{instruction}", lines.join("\n"))
}

pub const RETRY_SIM_BUDGET_TOKENS: usize = 400;

pub fn retry_prompt_suffix_with_sim(
    context: &FailedAttemptContext,
    task: &str,
    sim: Option<&crate::sim::SimStore>,
) -> String {
    let suffix = retry_prompt_suffix(context);
    let query = [task, context.reason.as_str()]
        .into_iter()
        .chain(context.failed_checks.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let block = sim
        .and_then(|store| {
            store
                .render_failure_block(&query, RETRY_SIM_BUDGET_TOKENS)
                .ok()
        })
        .filter(|block| !block.trim().is_empty());
    match block {
        Some(block) => format!("{suffix}\n\n{block}"),
        None => suffix,
    }
}

pub fn recovery_key(workspace: &str, task_id: &str, revision: &str) -> String {
    format!("{workspace}:{task_id}:{revision}")
}

#[derive(Clone, Debug)]
pub struct AttemptLog {
    path: PathBuf,
}

impl AttemptLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn all(&self) -> io::Result<Vec<RecoveryAttempt>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut records = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<RecoveryAttempt>(line) {
                if !record.key.is_empty() {
                    records.push(record);
                }
            }
        }
        Ok(records)
    }

    pub fn for_key(&self, key: &str) -> io::Result<Vec<RecoveryAttempt>> {
        let mut latest: BTreeMap<String, RecoveryAttempt> = BTreeMap::new();
        for record in self.all()? {
            if record.key == key {
                latest.insert(record.attempt_id.clone(), record);
            }
        }
        let mut ordered: Vec<RecoveryAttempt> = latest.into_values().collect();
        ordered.sort_by(|left, right| {
            left.at
                .cmp(&right.at)
                .then_with(|| left.attempt_id.cmp(&right.attempt_id))
        });
        Ok(ordered)
    }

    pub fn record(
        &self,
        key: &str,
        cause: FailureCause,
        recovery: &str,
        outcome: AttemptOutcome,
        detail: &str,
        at: i64,
    ) -> io::Result<RecoveryAttempt> {
        let existing = self.for_key(key)?;
        let number = 1 + existing
            .iter()
            .filter(|entry| entry.recovery == recovery && entry.outcome != AttemptOutcome::Started)
            .count() as u32;
        self.write_numbered(key, cause, recovery, outcome, detail, at, number)
    }

    pub fn settle(
        &self,
        started: &RecoveryAttempt,
        outcome: AttemptOutcome,
        detail: &str,
        at: i64,
    ) -> io::Result<RecoveryAttempt> {
        self.write_numbered(
            &started.key,
            started.cause,
            &started.recovery,
            outcome,
            detail,
            at,
            started.number,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_numbered(
        &self,
        key: &str,
        cause: FailureCause,
        recovery: &str,
        outcome: AttemptOutcome,
        detail: &str,
        at: i64,
        number: u32,
    ) -> io::Result<RecoveryAttempt> {
        let entry = RecoveryAttempt {
            key: key.to_owned(),
            cause,
            recovery: recovery.to_owned(),
            number,
            attempt_id: deterministic_attempt_id(key, recovery, number),
            outcome,
            detail: detail.trim().to_owned(),
            at,
        };
        self.append(&entry)?;
        Ok(entry)
    }

    fn append(&self, entry: &RecoveryAttempt) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&line)?;
        file.sync_all()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    Busy,
    LedgerUnavailable,
    Conflict,
    Capacity,
    TaskPolicy,
}

impl RefusalKind {
    pub fn backoff_secs(self) -> i64 {
        match self {
            Self::Busy => BACKOFF_BUSY_SECS,
            Self::LedgerUnavailable => BACKOFF_LEDGER_UNAVAILABLE_SECS,
            Self::Conflict => BACKOFF_CONFLICT_SECS,
            Self::Capacity => BACKOFF_CAPACITY_SECS,
            Self::TaskPolicy => BACKOFF_POLICY_SECS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefusalBackoffEntry {
    pub task_id: String,
    pub fingerprint: String,
    pub kind: RefusalKind,
    pub reason: String,
    pub until: i64,
    pub recorded_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefusalReceipt {
    pub receipt_id: String,
    pub task_id: String,
    pub fingerprint: String,
    pub kind: RefusalKind,
    pub reason: String,
    pub retry_after_secs: i64,
    pub recorded_at: i64,
}

const BACKOFF_SCHEMA: &str = "agent-command-refusal-backoff-v1";

#[derive(Serialize, Deserialize)]
struct BackoffFile {
    schema: String,
    entries: BTreeMap<String, RefusalBackoffEntry>,
    #[serde(default)]
    receipts: Vec<RefusalReceipt>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefusalBackoffStore {
    entries: BTreeMap<String, RefusalBackoffEntry>,
    receipts: Vec<RefusalReceipt>,
}

impl RefusalBackoffStore {
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let file: BackoffFile = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid refusal backoff state {}: {error}", path.display()),
            )
        })?;
        if file.schema != BACKOFF_SCHEMA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Unsupported refusal backoff schema {} in {}",
                    file.schema,
                    path.display()
                ),
            ));
        }
        Ok(Self {
            entries: file.entries,
            receipts: file.receipts,
        })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let file = BackoffFile {
            schema: BACKOFF_SCHEMA.into(),
            entries: self.entries.clone(),
            receipts: self.receipts.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        write_atomic(path, &bytes)
    }

    pub fn record_refusal(
        &mut self,
        task_id: &str,
        fingerprint: &str,
        kind: RefusalKind,
        reason: &str,
        now: i64,
    ) -> RefusalReceipt {
        let retry_after = kind.backoff_secs();
        let entry = RefusalBackoffEntry {
            task_id: task_id.to_owned(),
            fingerprint: fingerprint.to_owned(),
            kind,
            reason: one_line(reason),
            until: now.saturating_add(retry_after),
            recorded_at: now,
        };
        self.entries.insert(task_id.to_owned(), entry);
        let receipt = RefusalReceipt {
            receipt_id: format!("ref-{}", uuid::Uuid::new_v4().simple()),
            task_id: task_id.to_owned(),
            fingerprint: fingerprint.to_owned(),
            kind,
            reason: one_line(reason),
            retry_after_secs: retry_after,
            recorded_at: now,
        };
        self.receipts.push(receipt.clone());
        if self.receipts.len() > 200 {
            let excess = self.receipts.len() - 200;
            self.receipts.drain(..excess);
        }
        receipt
    }

    pub fn active_backoff(
        &mut self,
        task_id: &str,
        current_fingerprint: &str,
        now: i64,
    ) -> Option<RefusalBackoffEntry> {
        let entry = self.entries.get(task_id)?.clone();
        if entry.fingerprint != current_fingerprint || now >= entry.until {
            self.entries.remove(task_id);
            return None;
        }
        Some(entry)
    }

    pub fn clear_on_slot_released(&mut self) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, entry| entry.kind != RefusalKind::Capacity);
        before.saturating_sub(self.entries.len())
    }

    pub fn clear_task(&mut self, task_id: &str) -> bool {
        self.entries.remove(task_id).is_some()
    }

    pub fn receipts(&self) -> &[RefusalReceipt] {
        &self.receipts
    }
}

pub fn refusal_fingerprint(task_id: &str, content: &str, route: &str) -> String {
    let payload = serde_json::json!([task_id, content, route]);
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let hex: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    hex[..24].to_owned()
}

fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(BRIEF_FIELD_LIMIT)
        .collect()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let mut file = fs::File::create_new(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if fs::rename(&temporary, path).is_err() {
        fs::write(path, bytes)?;
        let _ = fs::remove_file(&temporary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-command-recovery-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn escalation_gate_enforces_bounded_requeue_budget_and_immediate_auth_escalation() {
        let dir = scratch_dir("gate");
        let log = AttemptLog::new(dir.join("attempts.jsonl"));
        let key = recovery_key("agent-command", "M-001", "rev-1");

        let auth_gate = escalation_gate(FailureCause::AuthOrPolicy, &[]);
        assert!(auth_gate.ready);
        assert!(matches!(
            decide_recovery(FailureCause::AuthOrPolicy, &[]),
            RecoveryDecision::Escalate(_)
        ));

        let initial_gate = escalation_gate(FailureCause::ExecutionFailed, &[]);
        assert!(!initial_gate.ready);
        let rule = next_recovery(FailureCause::ExecutionFailed, &[]).unwrap();
        assert_eq!(rule.name, "requeue");
        assert_eq!(rule.budget, 1);

        let started = log
            .record(
                &key,
                FailureCause::ExecutionFailed,
                rule.name,
                AttemptOutcome::Started,
                "Retrying task revision on admitted worker",
                100,
            )
            .unwrap();
        let after_start = log.for_key(&key).unwrap();
        assert_eq!(
            decide_recovery(FailureCause::ExecutionFailed, &after_start),
            RecoveryDecision::WaitInProgress
        );

        let failed = log
            .settle(
                &started,
                AttemptOutcome::Failed,
                "Retry failed with exit code 1",
                120,
            )
            .unwrap();
        assert_eq!(started.attempt_id, failed.attempt_id);
        let after_fail = log.for_key(&key).unwrap();
        assert_eq!(after_fail.len(), 1);
        assert!(next_recovery(FailureCause::ExecutionFailed, &after_fail).is_none());
        let final_gate = escalation_gate(FailureCause::ExecutionFailed, &after_fail);
        assert!(final_gate.ready);
        assert!(summarize_attempts(&after_fail).contains("requeue failed"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn retry_prompt_suffix_preserves_branch_context_without_mutating_task() {
        let ctx = FailedAttemptContext {
            branch: Some("ac/forge-task".into()),
            head: Some("abcdef123456".into()),
            reason: "cargo test failed".into(),
            failed_checks: vec!["unit tests (failure)".into()],
            prior_summary: Some("Missing import in src/inbox.rs".into()),
            merged: false,
        };
        let suffix = retry_prompt_suffix(&ctx);
        assert!(suffix.contains("ac/forge-task"));
        assert!(suffix.contains("abcdef123456"));
        assert!(suffix.contains("unit tests (failure)"));
        assert!(suffix.contains("Reuse that branch"));
        assert!(suffix.contains("Do not start over"));
    }

    #[test]
    fn retry_prompt_suffix_with_sim_appends_matching_failure_history_only() {
        let dir = scratch_dir("sim-retry");
        let workspace = dir.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let store = crate::sim::SimStore::for_workspace(dir.join("state"), &workspace);
        let ctx = FailedAttemptContext {
            branch: Some("ac/forge-task".into()),
            reason: "cargo test failed in inbox parser".into(),
            failed_checks: vec!["unit tests (failure)".into()],
            ..FailedAttemptContext::default()
        };
        let plain = retry_prompt_suffix(&ctx);
        assert_eq!(retry_prompt_suffix_with_sim(&ctx, "Fix inbox", None), plain);
        assert_eq!(
            retry_prompt_suffix_with_sim(&ctx, "Fix inbox", Some(&store)),
            plain
        );
        store
            .log_attempt(
                crate::sim::LogAttemptInput {
                    title: "Inbox parser panics on CRLF".into(),
                    outcome: Some(crate::sim::SimOutcome::Failed),
                    attempt_summary: "Split lines on \\n only".into(),
                    avoid_guidance: "Keep the CRLF normalisation guard; Windows inboxes need it."
                        .into(),
                    ..crate::sim::LogAttemptInput::default()
                },
                10,
            )
            .unwrap();
        store
            .log_attempt(
                crate::sim::LogAttemptInput {
                    title: "Inbox parser panics on CRLF".into(),
                    outcome: Some(crate::sim::SimOutcome::Worked),
                    attempt_summary: "Normalise line endings first".into(),
                    ..crate::sim::LogAttemptInput::default()
                },
                20,
            )
            .unwrap();
        let enriched = retry_prompt_suffix_with_sim(&ctx, "Fix inbox", Some(&store));
        assert!(enriched.starts_with(&plain));
        assert!(enriched.contains("<sim-memory"));
        assert!(enriched.contains("Keep the CRLF normalisation guard"));
        assert!(!enriched.contains("VERIFIED FIX"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refusal_backoff_keys_by_fingerprint_and_clears_capacity_on_slot_release() {
        let dir = scratch_dir("backoff");
        let path = dir.join("refusal-backoff.json");
        let mut store = RefusalBackoffStore::default();
        let fp1 = refusal_fingerprint("T-1", "body v1", "codex");
        let fp2 = refusal_fingerprint("T-1", "body v2", "codex");
        assert_ne!(fp1, fp2);

        let busy_receipt =
            store.record_refusal("T-BUSY", &fp1, RefusalKind::Busy, "worker busy", 100);
        assert_eq!(busy_receipt.retry_after_secs, 15);
        assert!(store.active_backoff("T-BUSY", &fp1, 114).is_some());
        assert!(store.active_backoff("T-BUSY", &fp1, 115).is_none());

        let ledger_receipt =
            store.record_refusal("T-LED", &fp1, RefusalKind::LedgerUnavailable, "locked", 100);
        assert_eq!(ledger_receipt.retry_after_secs, 60);
        let conflict_receipt =
            store.record_refusal("T-CON", &fp1, RefusalKind::Conflict, "dirty", 100);
        assert_eq!(conflict_receipt.retry_after_secs, 300);

        let cap_receipt =
            store.record_refusal("T-1", &fp1, RefusalKind::Capacity, "no free slot", 100);
        assert_eq!(cap_receipt.retry_after_secs, 900);
        assert!(store.active_backoff("T-1", &fp1, 500).is_some());

        store.save(&path).unwrap();
        let mut reloaded = RefusalBackoffStore::load(&path).unwrap();
        assert_eq!(reloaded.clear_on_slot_released(), 1);
        assert!(reloaded.active_backoff("T-1", &fp1, 500).is_none());

        reloaded.record_refusal("T-1", &fp1, RefusalKind::TaskPolicy, "missing spec", 100);
        assert!(reloaded.active_backoff("T-1", &fp1, 500).is_some());
        assert!(reloaded.active_backoff("T-1", &fp2, 500).is_none());

        let _ = fs::remove_dir_all(dir);
    }
}
