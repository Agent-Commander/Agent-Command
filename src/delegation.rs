use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use crate::catalog::{
    extract_component_scores, BenchmarkComponent, ComponentCatalog, ComponentEntry, ComponentScores,
};
use crate::inbox::{
    DecisionInput, DecisionKind, DecisionOption, DecisionReadiness, InboxError, InboxStore,
    StuckAttemptRef, StuckDeclaration,
};
use crate::ledger::{Ledger, LedgerDocument, TaskKind};
use crate::recovery::{
    decide_recovery, escalation_gate, refusal_fingerprint, retry_prompt_suffix, AttemptLog,
    AttemptOutcome, FailedAttemptContext, FailureCause, RecoveryDecision, RefusalBackoffStore,
    RefusalKind,
};
pub use crate::routing::{
    burn_score, classify_task, pick_next_task, ActiveUsageHold, Complexity, PacedWindow,
    QueuePriority, QueuedTask, QuotaPacer, TaskClassification, TaskType, WindowAllowance,
    WorkClass, BOUNDED_MECHANICAL_SPEED_DELTA,
};

pub const DEFAULT_STAGE_TIMEOUT_SECS: u64 = 1_800;
pub const DISPATCH_GUARD_SECS: u64 = 300;
pub const INTERRUPTED_SETTLE_SECS: i64 = 900;
pub const HEARTBEAT_INTERVAL_SECS: u64 = 2;
pub const HEARTBEAT_DEAD_SECS: i64 = 30;
pub const COMPLETION_WINDOW_SECS: i64 = 900;
pub const MAX_INLINE_PATCH_BYTES: usize = 48 * 1024;
pub const AGY_FALLBACK_COOLDOWN_SECS: i64 = 1_800;

const RESERVATIONS_FILE: &str = "reservations.json";
const RESERVATIONS_SCHEMA: &str = "agent-command-reservations-v1";
const WAKE_OUTBOX_FILE: &str = "outbox.jsonl";
const WAKE_CURSOR_FILE: &str = "cursor.json";
const WAKE_CURSOR_SCHEMA: &str = "agent-command-wake-cursor-v1";
const COMPLETE_MARKER: &str = "TASK_FLOW_COMPLETE";
const BLOCKED_MARKER: &str = "TASK_FLOW_BLOCKED";
const NEW_FILE_MARKER: &str = "TASK_FLOW_NEW_FILE";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    #[default]
    Implementation,
    Verification,
}

impl StageKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Implementation => "implementation",
            Self::Verification => "verification",
        }
    }

    pub fn is_verification(self) -> bool {
        self == Self::Verification
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompletionStatus {
    Complete {
        new_files: Vec<String>,
    },
    Blocked {
        reason: String,
        new_files: Vec<String>,
    },
    MissingMarker {
        new_files: Vec<String>,
    },
}

impl CompletionStatus {
    pub fn new_files(&self) -> &[String] {
        match self {
            Self::Complete { new_files }
            | Self::Blocked { new_files, .. }
            | Self::MissingMarker { new_files } => new_files,
        }
    }
}

pub fn parse_completion_contract(output: &str) -> CompletionStatus {
    let mut new_files = Vec::new();
    let mut complete = false;
    let mut blocked_reason: Option<String> = None;
    for raw_line in output.lines() {
        let trimmed = raw_line.trim();
        if trimmed == COMPLETE_MARKER {
            complete = true;
            blocked_reason = None;
        } else if let Some(rest) = trimmed.strip_prefix(BLOCKED_MARKER) {
            let reason = rest.trim();
            if !reason.is_empty() {
                blocked_reason = Some(reason.to_owned());
                complete = false;
            }
        } else if let Some(rest) = trimmed.strip_prefix(NEW_FILE_MARKER) {
            let rel = rest.trim();
            if is_safe_relative_path(rel) && !new_files.iter().any(|existing| existing == rel) {
                new_files.push(rel.to_owned());
            }
        }
    }
    if let Some(reason) = blocked_reason {
        CompletionStatus::Blocked { reason, new_files }
    } else if complete {
        CompletionStatus::Complete { new_files }
    } else {
        CompletionStatus::MissingMarker { new_files }
    }
}

pub fn is_rate_limit_signal(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "resource_exhausted",
        "quota exceeded",
        "quota exhausted",
        "quota limit",
        "rate limit",
        "http 429",
        "429 too many",
    ];
    PATTERNS.iter().any(|needle| lowered.contains(needle))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceBinding {
    pub repo: String,
    pub source_revision: String,
    pub target_revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StageCheckpoint {
    pub reservation_id: String,
    pub stage: StageKind,
    pub head: String,
    pub branch: String,
    pub patch_sha256: String,
    #[serde(default)]
    pub checks: Vec<String>,
    pub summary: String,
    pub completed_at: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StagePromptInput {
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub stage: StageKind,
    pub prior_checkpoint: Option<StageCheckpoint>,
    pub decision_answer: Option<String>,
    pub team_directions: Vec<String>,
    pub source_patch: Option<String>,
    pub source_patch_sha256: Option<String>,
    pub retry_context: Option<FailedAttemptContext>,
}

pub fn build_stage_prompt(input: &StagePromptInput) -> String {
    let mut sections = vec![format!(
        "TASK {} [{}]: {}\n\n{}",
        input.task_id.trim(),
        input.stage.label(),
        input.title.trim(),
        input.body.trim()
    )];
    if let Some(checkpoint) = &input.prior_checkpoint {
        if let Ok(json) = serde_json::to_string_pretty(checkpoint) {
            sections.push(format!("Prior CLI handoff:\n```json\n{json}\n```"));
        }
    }
    if let Some(answer) = input
        .decision_answer
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        sections.push(format!("Operator decision answer:\n{answer}"));
    }
    if !input.team_directions.is_empty() {
        let bullets = input
            .team_directions
            .iter()
            .map(|dir| format!("- {}", dir.trim()))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("Standing team directions:\n{bullets}"));
    }
    if input.stage == StageKind::Verification {
        let mut verify_section =
            "Verification rules:\n- Read-only verification only. Do NOT modify tracked or untracked files.".to_owned();
        if let Some(hash) = &input.source_patch_sha256 {
            verify_section.push_str(&format!("\n- Expected source.patch SHA-256: {hash}"));
        }
        if let Some(patch) = &input.source_patch {
            if patch.len() <= MAX_INLINE_PATCH_BYTES {
                verify_section.push_str(&format!("\n\n```diff\n{patch}\n```"));
            } else {
                verify_section.push_str(&format!(
                    "\n- Patch size ({} bytes) exceeds inline cap; inspect workspace git diff directly.",
                    patch.len()
                ));
            }
        }
        sections.push(verify_section);
    }
    if let Some(retry) = &input.retry_context {
        sections.push(retry_prompt_suffix(retry));
    }
    sections.push(format!(
        "Completion contract:\n- Declare every newly created file on its own line as `{NEW_FILE_MARKER} <relative/path>`.\n- End your final response with `{COMPLETE_MARKER}` on its own line when finished, or `{BLOCKED_MARKER} <concrete reason>` if blocked."
    ));
    sections.join("\n\n")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchVerificationError {
    EmptyImplementationDiff,
    VerificationMutatedWorkspace { before: String, after: String },
    UndeclaredOrInvalidNewFile(String),
    GitFailed(String),
}

impl std::fmt::Display for PatchVerificationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyImplementationDiff => {
                formatter.write_str("Implementation stage finished with zero workspace diff.")
            }
            Self::VerificationMutatedWorkspace { before, after } => write!(
                formatter,
                "Verification stage mutated workspace patch hash ({before} -> {after})."
            ),
            Self::UndeclaredOrInvalidNewFile(path) => {
                write!(
                    formatter,
                    "Invalid or untracked declared new file '{path}'."
                )
            }
            Self::GitFailed(msg) => write!(formatter, "Git operation failed: {msg}"),
        }
    }
}

impl std::error::Error for PatchVerificationError {}

pub fn patch_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn verify_stage_patch_invariant(
    stage: StageKind,
    before_patch_sha256: &str,
    after_patch_sha256: &str,
) -> Result<(), PatchVerificationError> {
    match stage {
        StageKind::Implementation => {
            if before_patch_sha256 == after_patch_sha256 {
                return Err(PatchVerificationError::EmptyImplementationDiff);
            }
            Ok(())
        }
        StageKind::Verification => {
            if before_patch_sha256 != after_patch_sha256 {
                return Err(PatchVerificationError::VerificationMutatedWorkspace {
                    before: before_patch_sha256.to_owned(),
                    after: after_patch_sha256.to_owned(),
                });
            }
            Ok(())
        }
    }
}

pub fn validate_declared_new_files(
    declared: &[String],
    untracked_files: &[String],
) -> Result<Vec<String>, PatchVerificationError> {
    let mut validated = Vec::with_capacity(declared.len());
    for candidate in declared {
        let normalized = candidate.trim().replace('\\', "/");
        if !is_safe_relative_path(&normalized) {
            return Err(PatchVerificationError::UndeclaredOrInvalidNewFile(
                candidate.clone(),
            ));
        }
        let exists = untracked_files
            .iter()
            .any(|path| path.replace('\\', "/") == normalized);
        if !exists {
            return Err(PatchVerificationError::UndeclaredOrInvalidNewFile(
                normalized,
            ));
        }
        validated.push(normalized);
    }
    Ok(validated)
}

pub fn list_untracked_files(workspace: &Path) -> Result<Vec<String>, PatchVerificationError> {
    let output = Command::new("git")
        .current_dir(workspace)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .map_err(|error| PatchVerificationError::GitFailed(error.to_string()))?;
    if !output.status.success() {
        return Err(PatchVerificationError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let files = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|slice| !slice.is_empty())
        .map(|slice| String::from_utf8_lossy(slice).into_owned())
        .collect();
    Ok(files)
}

pub fn stage_declared_new_files(
    workspace: &Path,
    verified_files: &[String],
) -> Result<(), PatchVerificationError> {
    if verified_files.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git")
        .current_dir(workspace)
        .args([
            "--literal-pathspecs",
            "add",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| PatchVerificationError::GitFailed(error.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        for path in verified_files {
            stdin
                .write_all(path.as_bytes())
                .and_then(|_| stdin.write_all(&[0]))
                .map_err(|error| PatchVerificationError::GitFailed(error.to_string()))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| PatchVerificationError::GitFailed(error.to_string()))?;
    if !output.status.success() {
        return Err(PatchVerificationError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(())
}

pub fn workspace_patch_snapshot(
    workspace: &Path,
) -> Result<(String, String), PatchVerificationError> {
    let output = Command::new("git")
        .current_dir(workspace)
        .args(["diff", "HEAD"])
        .output()
        .map_err(|error| PatchVerificationError::GitFailed(error.to_string()))?;
    if !output.status.success() {
        return Err(PatchVerificationError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let hash = patch_sha256(&output.stdout);
    let patch = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok((patch, hash))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunSpec {
    pub reservation_id: String,
    pub task_id: String,
    pub worker_name: String,
    pub stage: StageKind,
    pub work_class: WorkClass,
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<String>,
    pub workspace: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_binding: Option<SourceBinding>,
    pub timeout_secs: u64,
    pub created_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    Runner,
    Refusal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClaimRecord {
    pub kind: ClaimKind,
    pub claimed_at: i64,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRecord {
    pub at: i64,
    pub phase: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelInvocationReceipt {
    pub requested_provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub recorded_at: i64,
}

impl ModelInvocationReceipt {
    pub fn matches_request(&self) -> bool {
        let model_ok = match (&self.requested_model, &self.reported_model) {
            (Some(req), Some(rep)) => req.eq_ignore_ascii_case(rep),
            (Some(_), None) => false,
            (None, _) => true,
        };
        let effort_ok = match (&self.requested_effort, &self.reported_effort) {
            (Some(req), Some(rep)) => req.eq_ignore_ascii_case(rep),
            (Some(_), None) => false,
            (None, _) => true,
        };
        model_ok && effort_ok
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Succeeded,
    Blocked,
    Failed,
    TimedOut,
    Stalled,
    Interrupted,
    Refused,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub outcome: RunOutcome,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_sha256: Option<String>,
    pub completed_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    Dispatching,
    Running,
    Unknown,
    Completed,
    Failed,
    Refused,
}

impl ReservationState {
    pub fn holds_slot(self) -> bool {
        matches!(self, Self::Dispatching | Self::Running | Self::Unknown)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReservationRecord {
    pub reservation_id: String,
    pub task_id: String,
    pub worker_name: String,
    pub profile: String,
    pub stage: StageKind,
    pub state: ReservationState,
    pub usage_hold: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_reset_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InspectResolution {
    StillActive,
    SettledCompleted,
    SettledFailed(String),
    SettledRefused(String),
}

#[derive(Serialize, Deserialize)]
struct ReservationsFile {
    schema: String,
    records: BTreeMap<String, ReservationRecord>,
}

#[derive(Clone, Debug)]
pub struct ReservationLedger {
    root: PathBuf,
    records: BTreeMap<String, ReservationRecord>,
}

impl ReservationLedger {
    pub fn open(root: PathBuf) -> io::Result<Self> {
        let path = root.join(RESERVATIONS_FILE);
        let records = match fs::read(&path) {
            Ok(bytes) => {
                let file: ReservationsFile = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Invalid reservations file {}: {error}", path.display()),
                    )
                })?;
                if file.schema != RESERVATIONS_SCHEMA {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Unsupported reservations schema {}", file.schema),
                    ));
                }
                file.records
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { root, records })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run_dir(&self, reservation_id: &str) -> PathBuf {
        self.root.join("runs").join(reservation_id)
    }

    pub fn records(&self) -> Vec<ReservationRecord> {
        self.records.values().cloned().collect()
    }

    pub fn get(&self, reservation_id: &str) -> Option<&ReservationRecord> {
        self.records.get(reservation_id)
    }

    pub fn active_for_task(&self, task_id: &str) -> Option<&ReservationRecord> {
        self.records
            .values()
            .find(|record| record.task_id == task_id && record.state.holds_slot())
    }

    pub fn active_usage_holds(&self) -> Vec<ActiveUsageHold> {
        self.records
            .values()
            .filter(|record| record.state.holds_slot() && record.usage_hold > 0.0)
            .filter_map(|record| {
                Some(ActiveUsageHold {
                    profile: record.profile.clone(),
                    window_id: record.window_id.clone()?,
                    reset_at: record.window_reset_at?,
                    created_at: record.created_at,
                    usage_units: record.usage_hold,
                })
            })
            .collect()
    }

    pub fn bind_workspace_source(
        &self,
        workspace: &Path,
        expected: &SourceBinding,
    ) -> io::Result<()> {
        let binding_path = self.binding_path(workspace);
        if let Ok(bytes) = fs::read(&binding_path) {
            let existing: SourceBinding = serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Corrupt source binding {}: {error}", binding_path.display()),
                )
            })?;
            if &existing != expected {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Workspace {} is bound to {}:{} (expected {}:{}).",
                        workspace.display(),
                        existing.repo,
                        existing.source_revision,
                        expected.repo,
                        expected.source_revision
                    ),
                ));
            }
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(expected).map_err(io::Error::other)?;
        write_atomic(&binding_path, &bytes)
    }

    fn binding_path(&self, workspace: &Path) -> PathBuf {
        let canonical = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let hash = patch_sha256(canonical.to_string_lossy().as_bytes());
        self.root
            .join("bindings")
            .join(format!("{}.json", &hash[..16]))
    }

    pub fn reserve(
        &mut self,
        spec: &RunSpec,
        usage_hold: f64,
        window: Option<(String, i64)>,
        now: i64,
    ) -> io::Result<ReservationRecord> {
        if let Some(existing) = self.active_for_task(&spec.task_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Task {} already holds active reservation {} ({:?}); refusing blind relaunch.",
                    spec.task_id, existing.reservation_id, existing.state
                ),
            ));
        }
        if let Some(binding) = &spec.source_binding {
            self.bind_workspace_source(&spec.workspace, binding)?;
        }
        let run_dir = self.run_dir(&spec.reservation_id);
        fs::create_dir_all(&run_dir)?;
        let spec_bytes = serde_json::to_vec_pretty(spec).map_err(io::Error::other)?;
        write_atomic(&run_dir.join("spec.json"), &spec_bytes)?;
        let (window_id, window_reset_at) = match window {
            Some((id, reset)) => (Some(id), Some(reset)),
            None => (None, None),
        };
        let record = ReservationRecord {
            reservation_id: spec.reservation_id.clone(),
            task_id: spec.task_id.clone(),
            worker_name: spec.worker_name.clone(),
            profile: spec.profile.clone(),
            stage: spec.stage,
            state: ReservationState::Dispatching,
            usage_hold,
            window_id,
            window_reset_at,
            created_at: now,
            updated_at: now,
            last_heartbeat_at: None,
        };
        self.records
            .insert(spec.reservation_id.clone(), record.clone());
        self.save()?;
        Ok(record)
    }

    pub fn claim_once(
        &mut self,
        reservation_id: &str,
        kind: ClaimKind,
        detail: &str,
        now: i64,
    ) -> io::Result<bool> {
        let claim_path = self.run_dir(reservation_id).join("claim");
        let payload = ClaimRecord {
            kind,
            claimed_at: now,
            detail: detail.trim().to_owned(),
        };
        let bytes = serde_json::to_vec_pretty(&payload).map_err(io::Error::other)?;
        match fs::File::create_new(&claim_path) {
            Ok(mut file) => {
                file.write_all(&bytes)?;
                file.sync_all()?;
                if let Some(record) = self.records.get_mut(reservation_id) {
                    record.state = match kind {
                        ClaimKind::Runner => ReservationState::Running,
                        ClaimKind::Refusal => ReservationState::Refused,
                    };
                    record.updated_at = now;
                    if kind == ClaimKind::Runner {
                        record.last_heartbeat_at = Some(now);
                    }
                }
                self.save()?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn write_pid(&self, reservation_id: &str, pid: u32, started_at: i64) -> io::Result<()> {
        let bytes =
            serde_json::to_vec_pretty(&PidRecord { pid, started_at }).map_err(io::Error::other)?;
        write_atomic(&self.run_dir(reservation_id).join("pid.json"), &bytes)
    }

    pub fn write_heartbeat(
        &mut self,
        reservation_id: &str,
        at: i64,
        phase: &str,
    ) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&HeartbeatRecord {
            at,
            phase: phase.to_owned(),
        })
        .map_err(io::Error::other)?;
        write_atomic(&self.run_dir(reservation_id).join("heartbeat"), &bytes)?;
        if let Some(record) = self.records.get_mut(reservation_id) {
            record.last_heartbeat_at = Some(at);
            record.updated_at = at;
        }
        Ok(())
    }

    pub fn read_heartbeat(&self, reservation_id: &str) -> io::Result<Option<HeartbeatRecord>> {
        let path = self.run_dir(reservation_id).join("heartbeat");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn is_heartbeat_dead(&self, reservation_id: &str, now: i64) -> bool {
        let last = self
            .read_heartbeat(reservation_id)
            .ok()
            .flatten()
            .map(|hb| hb.at)
            .or_else(|| {
                self.records
                    .get(reservation_id)
                    .and_then(|record| record.last_heartbeat_at)
            });
        last.is_some_and(|at| now.saturating_sub(at) >= HEARTBEAT_DEAD_SECS)
    }

    pub fn write_model_receipt(
        &self,
        reservation_id: &str,
        receipt: &ModelInvocationReceipt,
    ) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(receipt).map_err(io::Error::other)?;
        write_atomic(
            &self.run_dir(reservation_id).join("model-invocation.json"),
            &bytes,
        )
    }

    pub fn write_checkpoint(
        &self,
        reservation_id: &str,
        checkpoint: &StageCheckpoint,
    ) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(checkpoint).map_err(io::Error::other)?;
        write_atomic(
            &self.run_dir(reservation_id).join("checkpoint.json"),
            &bytes,
        )
    }

    pub fn mark_unknown(&mut self, reservation_id: &str, now: i64) -> io::Result<()> {
        if let Some(record) = self.records.get_mut(reservation_id) {
            record.state = ReservationState::Unknown;
            record.updated_at = now;
        }
        self.save()
    }

    pub fn finalize(&mut self, reservation_id: &str, exit: &ExitRecord) -> io::Result<()> {
        let run_dir = self.run_dir(reservation_id);
        fs::create_dir_all(&run_dir)?;
        let bytes = serde_json::to_vec_pretty(exit).map_err(io::Error::other)?;
        write_atomic(&run_dir.join("exit.json"), &bytes)?;
        if let Some(record) = self.records.get_mut(reservation_id) {
            record.state = match exit.outcome {
                RunOutcome::Succeeded => ReservationState::Completed,
                RunOutcome::Refused => ReservationState::Refused,
                RunOutcome::Blocked
                | RunOutcome::Failed
                | RunOutcome::TimedOut
                | RunOutcome::Stalled
                | RunOutcome::Interrupted => ReservationState::Failed,
            };
            record.updated_at = exit.completed_at;
        }
        self.save()
    }

    pub fn inspect(&mut self, reservation_id: &str, now: i64) -> io::Result<InspectResolution> {
        let run_dir = self.run_dir(reservation_id);
        if let Ok(bytes) = fs::read(run_dir.join("exit.json")) {
            let exit: ExitRecord = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            self.finalize(reservation_id, &exit)?;
            return Ok(match exit.outcome {
                RunOutcome::Succeeded => InspectResolution::SettledCompleted,
                RunOutcome::Refused => InspectResolution::SettledRefused(exit.summary),
                _ => InspectResolution::SettledFailed(exit.summary),
            });
        }
        if let Ok(bytes) = fs::read(run_dir.join("claim")) {
            if let Ok(claim) = serde_json::from_slice::<ClaimRecord>(&bytes) {
                if claim.kind == ClaimKind::Refusal {
                    let exit = ExitRecord {
                        outcome: RunOutcome::Refused,
                        summary: claim.detail.clone(),
                        patch_sha256: None,
                        completed_at: now,
                    };
                    self.finalize(reservation_id, &exit)?;
                    return Ok(InspectResolution::SettledRefused(claim.detail));
                }
            }
        }
        if self.is_heartbeat_dead(reservation_id, now) {
            let reason = format!(
                "Worker heartbeat silent for at least {HEARTBEAT_DEAD_SECS}s; settled as stalled."
            );
            let exit = ExitRecord {
                outcome: RunOutcome::Stalled,
                summary: reason.clone(),
                patch_sha256: None,
                completed_at: now,
            };
            self.finalize(reservation_id, &exit)?;
            return Ok(InspectResolution::SettledFailed(reason));
        }
        let elapsed_since_update = self
            .records
            .get(reservation_id)
            .map(|r| now.saturating_sub(r.updated_at))
            .unwrap_or(0);
        if elapsed_since_update >= INTERRUPTED_SETTLE_SECS {
            let reason = format!(
                "Reservation unsettled past {INTERRUPTED_SETTLE_SECS}s bound; settled as failed."
            );
            let exit = ExitRecord {
                outcome: RunOutcome::Failed,
                summary: reason.clone(),
                patch_sha256: None,
                completed_at: now,
            };
            self.finalize(reservation_id, &exit)?;
            return Ok(InspectResolution::SettledFailed(reason));
        }
        Ok(InspectResolution::StillActive)
    }

    fn save(&self) -> io::Result<()> {
        let payload = ReservationsFile {
            schema: RESERVATIONS_SCHEMA.into(),
            records: self.records.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&payload).map_err(io::Error::other)?;
        write_atomic(&self.root.join(RESERVATIONS_FILE), &bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionEventKind {
    RunReport,
    SlotReleased,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompletionEvent {
    pub kind: CompletionEventKind,
    pub task_id: String,
    pub reservation_id: String,
    pub state: String,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefillStatus {
    Pending,
    Consumed,
    Unfilled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefillJournalRow {
    pub run_key: String,
    pub task_id: String,
    pub reservation_id: String,
    pub at: i64,
    pub status: RefillStatus,
    pub signals: BTreeMap<String, CompletionEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WakeCursorState {
    schema: String,
    byte_offset: u64,
    seen: BTreeMap<String, i64>,
    journal: BTreeMap<String, RefillJournalRow>,
}

impl Default for WakeCursorState {
    fn default() -> Self {
        Self {
            schema: WAKE_CURSOR_SCHEMA.into(),
            byte_offset: 0,
            seen: BTreeMap::new(),
            journal: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompletionWakeJournal {
    root: PathBuf,
    state: WakeCursorState,
}

impl CompletionWakeJournal {
    pub fn open(root: PathBuf) -> io::Result<Self> {
        let cursor_path = root.join(WAKE_CURSOR_FILE);
        let state = match fs::read(&cursor_path) {
            Ok(bytes) => {
                let parsed: WakeCursorState =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if parsed.schema != WAKE_CURSOR_SCHEMA {
                    WakeCursorState::default()
                } else {
                    parsed
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => WakeCursorState::default(),
            Err(error) => return Err(error),
        };
        Ok(Self { root, state })
    }

    pub fn record_event(&self, event: &CompletionEvent) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let outbox_path = self.root.join(WAKE_OUTBOX_FILE);
        let mut line = serde_json::to_vec(event).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&outbox_path)?;
        file.write_all(&line)?;
        file.sync_all()
    }

    pub fn poll_new_events(&mut self, now: i64) -> io::Result<Vec<CompletionEvent>> {
        self.state
            .seen
            .retain(|_, seen_at| now.saturating_sub(*seen_at) <= COMPLETION_WINDOW_SECS);
        let outbox_path = self.root.join(WAKE_OUTBOX_FILE);
        let file = match fs::File::open(&outbox_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let file_len = file.metadata()?.len();
        let start_offset = if self.state.byte_offset <= file_len {
            self.state.byte_offset
        } else {
            0
        };
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(start_offset))?;
        let mut events = Vec::new();
        let mut line = String::new();
        for _ in 0..128 {
            line.clear();
            let bytes_read = reader.read_line(&mut line)?;
            if bytes_read == 0 || !line.ends_with('\n') {
                break;
            }
            self.state.byte_offset = reader.stream_position()?;
            let Ok(event) = serde_json::from_str::<CompletionEvent>(&line) else {
                continue;
            };
            if event.at <= 0 || event.at > now {
                continue;
            }
            let base_id =
                patch_sha256(format!("{}:{}", event.task_id, event.reservation_id).as_bytes());
            let identity = match event.kind {
                CompletionEventKind::RunReport => base_id.clone(),
                CompletionEventKind::SlotReleased => {
                    self.state.seen.insert(base_id.clone(), now);
                    format!("release:{base_id}")
                }
            };
            let already_recorded = self.state.seen.contains_key(&identity)
                || self
                    .state
                    .journal
                    .get(&base_id)
                    .is_some_and(|row| row.signals.contains_key(&identity));
            if already_recorded {
                continue;
            }
            self.state.seen.insert(identity.clone(), now);
            let row = self
                .state
                .journal
                .entry(base_id.clone())
                .or_insert_with(|| RefillJournalRow {
                    run_key: base_id,
                    task_id: event.task_id.clone(),
                    reservation_id: event.reservation_id.clone(),
                    at: event.at,
                    status: RefillStatus::Pending,
                    signals: BTreeMap::new(),
                });
            if event.kind == CompletionEventKind::SlotReleased
                && matches!(row.status, RefillStatus::Pending | RefillStatus::Unfilled)
            {
                row.at = event.at;
                row.status = RefillStatus::Pending;
            }
            row.signals.insert(identity, event.clone());
            events.push(event);
        }
        self.save_cursor()?;
        Ok(events)
    }

    pub fn take_pending_refill(&mut self, now: i64) -> io::Result<Option<RefillJournalRow>> {
        for row in self.state.journal.values_mut() {
            if row.status == RefillStatus::Pending
                && now.saturating_sub(row.at) > COMPLETION_WINDOW_SECS
            {
                row.status = RefillStatus::Unfilled;
            }
        }
        let next_key = self
            .state
            .journal
            .values()
            .filter(|row| row.status == RefillStatus::Pending)
            .min_by(|left, right| {
                left.at
                    .cmp(&right.at)
                    .then_with(|| left.run_key.cmp(&right.run_key))
            })
            .map(|row| row.run_key.clone());
        let Some(key) = next_key else {
            self.save_cursor()?;
            return Ok(None);
        };
        let row = self.state.journal.get_mut(&key).cloned();
        if let Some(entry) = self.state.journal.get_mut(&key) {
            entry.status = RefillStatus::Consumed;
        }
        self.save_cursor()?;
        Ok(row)
    }

    fn save_cursor(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.state).map_err(io::Error::other)?;
        write_atomic(&self.root.join(WAKE_CURSOR_FILE), &bytes)
    }
}

pub struct DelegationScheduler {
    pub reservations: ReservationLedger,
    pub wake_journal: CompletionWakeJournal,
    pub pacer: QuotaPacer,
    pub attempt_log: AttemptLog,
    pub backoff: RefusalBackoffStore,
    state_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscalationContext<'a> {
    pub ledger_entry_id: &'a str,
    pub task_key: &'a str,
    pub cause: FailureCause,
    pub summary: &'a str,
    pub now: i64,
}

impl DelegationScheduler {
    pub fn open(state_dir: PathBuf) -> io::Result<Self> {
        let reservations = ReservationLedger::open(state_dir.join("delegation"))?;
        let wake_journal = CompletionWakeJournal::open(state_dir.join("delegation-wake"))?;
        let pacer = QuotaPacer::load(&state_dir)?;
        let recovery_dir = state_dir.join("stuck-recovery");
        let attempt_log = AttemptLog::new(recovery_dir.join("attempts.jsonl"));
        let backoff = RefusalBackoffStore::load(&recovery_dir.join("refusal-backoff.json"))?;
        Ok(Self {
            reservations,
            wake_journal,
            pacer,
            attempt_log,
            backoff,
            state_dir,
        })
    }

    pub fn select_next_task<'a>(
        &mut self,
        tasks: &'a [QueuedTask],
        served_by_project: &HashMap<String, u64>,
        fingerprints: &HashMap<String, String>,
        now: i64,
    ) -> Option<&'a QueuedTask> {
        let eligible: Vec<QueuedTask> = tasks
            .iter()
            .filter(|task| {
                if task.blocked {
                    return false;
                }
                if self.reservations.active_for_task(&task.id).is_some() {
                    return false;
                }
                let fp = fingerprints
                    .get(&task.id)
                    .map(String::as_str)
                    .unwrap_or(task.id.as_str());
                self.backoff.active_backoff(&task.id, fp, now).is_none()
            })
            .cloned()
            .collect();
        let chosen_id = pick_next_task(&eligible, served_by_project)?.id.clone();
        tasks.iter().find(|task| task.id == chosen_id)
    }

    pub fn record_refusal(
        &mut self,
        task_id: &str,
        content: &str,
        route: &str,
        kind: RefusalKind,
        reason: &str,
        now: i64,
    ) -> io::Result<()> {
        let fp = refusal_fingerprint(task_id, content, route);
        self.backoff.record_refusal(task_id, &fp, kind, reason, now);
        self.backoff.save(
            &self
                .state_dir
                .join("stuck-recovery")
                .join("refusal-backoff.json"),
        )
    }

    pub fn record_slot_released(
        &mut self,
        task_id: &str,
        reservation_id: &str,
        state: &str,
        pid: Option<u32>,
        now: i64,
    ) -> io::Result<()> {
        self.backoff.clear_on_slot_released();
        self.backoff.save(
            &self
                .state_dir
                .join("stuck-recovery")
                .join("refusal-backoff.json"),
        )?;
        self.wake_journal.record_event(&CompletionEvent {
            kind: CompletionEventKind::SlotReleased,
            task_id: task_id.to_owned(),
            reservation_id: reservation_id.to_owned(),
            state: state.to_owned(),
            at: now,
            pid,
        })
    }

    pub fn record_task_doc(
        &self,
        ledger_path: &Path,
        owner: &str,
        title: &str,
        summary: &str,
        brief: &str,
        now: i64,
    ) -> io::Result<String> {
        let mut ledger = Ledger::load(ledger_path);
        let id = ledger.record_with_summary(TaskKind::Spawn, owner, title, summary, now);
        if let Some(entry) = ledger.get(&id).cloned() {
            let doc = LedgerDocument::from_entry(&entry, brief);
            ledger.write_document(&self.state_dir, &id, &doc)?;
        }
        ledger.save(ledger_path)?;
        Ok(id)
    }

    pub fn append_task_doc(
        &self,
        ledger_path: &Path,
        entry_id: &str,
        heading: &str,
        body: &str,
        now: i64,
    ) -> io::Result<PathBuf> {
        let ledger = Ledger::load(ledger_path);
        ledger.append_document(&self.state_dir, entry_id, heading, body, now)
    }

    pub fn read_task_doc(&self, ledger_path: &Path, entry_id: &str) -> io::Result<String> {
        let ledger = Ledger::load(ledger_path);
        ledger.read_document(&self.state_dir, entry_id)
    }

    pub fn set_task_waiting_for(
        &self,
        ledger_path: &Path,
        entry_id: &str,
        waiting_for: Option<&str>,
        now: i64,
    ) -> io::Result<bool> {
        let mut ledger = Ledger::load(ledger_path);
        let updated = ledger.set_waiting_for(entry_id, waiting_for, now);
        if updated {
            ledger.save(ledger_path)?;
        }
        Ok(updated)
    }

    pub fn evaluate_and_escalate_to_inbox(
        &self,
        inbox: &mut InboxStore,
        ledger_path: &Path,
        ctx: &EscalationContext<'_>,
    ) -> Result<RecoveryDecision, InboxError> {
        let mut ledger = Ledger::load(ledger_path);
        let attempts = self
            .attempt_log
            .for_key(ctx.task_key)
            .map_err(|error| InboxError::Io(error.to_string()))?;
        let decision = decide_recovery(ctx.cause, &attempts);
        let gate = escalation_gate(ctx.cause, &attempts);
        if matches!(decision, RecoveryDecision::Escalate(_)) && gate.ready {
            let card_id = format!(
                "ESC-{}",
                ctx.ledger_entry_id
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() {
                        ch.to_ascii_uppercase()
                    } else {
                        '-'
                    })
                    .collect::<String>()
            );
            let stuck_refs = attempts
                .iter()
                .map(|attempt| StuckAttemptRef {
                    recovery: attempt.recovery.clone(),
                    outcome: attempt.outcome.label().into(),
                    detail: attempt.detail.clone(),
                    at: attempt.at,
                })
                .collect();
            let input = DecisionInput {
                id: card_id.clone(),
                title: format!(
                    "Escalation for {} ({})",
                    ctx.ledger_entry_id,
                    ctx.cause.label()
                ),
                question: format!(
                    "How should {} proceed after {}?",
                    ctx.ledger_entry_id,
                    ctx.cause.label()
                ),
                current_state: ctx.summary.trim().to_owned(),
                why_it_matters:
                    "Bounded automatic recoveries are exhausted; operator direction is required."
                        .into(),
                recommendation: "retry — inspect failure details and re-run with guidance".into(),
                source_row: ctx.ledger_entry_id.to_owned(),
                owner_agent_id: "delegation-scheduler".into(),
                owner_name: "Delegation Scheduler".into(),
                project: "Agent-Command".into(),
                group: "recovery".into(),
                readiness: DecisionReadiness::Ready,
                alternatives: vec![
                    DecisionOption {
                        id: "retry".into(),
                        label: "Retry with operator guidance".into(),
                    },
                    DecisionOption {
                        id: "retire".into(),
                        label: "Stop and close task".into(),
                    },
                ],
                kind: DecisionKind::Standard,
                constraints: None,
                example: None,
                detail: None,
                preparation: None,
                canonical_sources: vec![ledger
                    .get(ctx.ledger_entry_id)
                    .map(|entry| entry.doc_rel_path.clone())
                    .unwrap_or_default()],
                is_current_blocker: true,
                self_serve: None,
                stuck: Some(StuckDeclaration {
                    cause: ctx.cause.label().into(),
                    attempts: stuck_refs,
                }),
            };
            inbox.upsert_card(input, ctx.now)?;
            ledger.link_decision(ctx.ledger_entry_id, &card_id, ctx.now);
            ledger
                .save(ledger_path)
                .map_err(|error| InboxError::Io(error.to_string()))?;
        }
        Ok(decision)
    }

    pub fn sync_inbox_answer_to_ledger(
        &self,
        ledger_path: &Path,
        entry_id: &str,
        decision_id: &str,
        answer_text: &str,
        now: i64,
    ) -> io::Result<bool> {
        let mut ledger = Ledger::load(ledger_path);
        let updated = ledger.record_decision_answer(
            Some(&self.state_dir),
            entry_id,
            decision_id,
            answer_text,
            now,
        )?;
        if updated {
            ledger.save(ledger_path)?;
        }
        Ok(updated)
    }

    pub fn compact_ledger(
        &self,
        ledger_path: &Path,
        now: i64,
        max_age_secs: i64,
    ) -> io::Result<(usize, usize)> {
        let mut ledger = Ledger::load(ledger_path);
        let result = ledger.compact_weekly_archive(&self.state_dir, now, max_age_secs)?;
        let overflow_count = ledger.archived_overflow().len();
        ledger.save(ledger_path)?;
        Ok((result.archived_ids.len(), overflow_count))
    }

    pub fn rank_profiles_for_work_class(
        &self,
        profiles: &[crate::auth::Profile],
        work_class: WorkClass,
        urgent: bool,
    ) -> Result<Vec<String>, String> {
        let catalog = crate::catalog::Catalog::load(Some(&self.state_dir));
        let components = ComponentCatalog::load(Some(&self.state_dir));
        let pins = crate::native::Pins::default();
        let (candidates, _) = crate::routing::candidates_with_components(
            profiles,
            &catalog,
            Some(&components),
            &pins,
            crate::usage::now(),
        );
        let policy = if urgent {
            crate::routing::Policy::Urgent
        } else {
            crate::routing::Policy::Default
        };
        let ranked = crate::routing::rank_for_work_class(
            candidates,
            policy,
            crate::routing::Difficulty::Any,
            work_class,
        )?;
        Ok(ranked.into_iter().map(|c| c.label).collect())
    }

    pub fn record_recovery_attempt(
        &self,
        key: &str,
        cause: FailureCause,
        recovery: &str,
        outcome: AttemptOutcome,
        detail: &str,
        at: i64,
    ) -> io::Result<crate::recovery::RecoveryAttempt> {
        self.attempt_log
            .record(key, cause, recovery, outcome, detail, at)
    }
}

fn is_safe_relative_path(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed.contains('\0') || trimmed.starts_with(['/', '\\']) {
        return false;
    }
    let candidate = Path::new(trimmed);
    let mut has_normal = false;
    for component in candidate.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            _ => return false,
        }
    }
    has_normal
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
            "agent-command-delegation-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn completion_contract_parses_complete_blocked_and_safe_new_files() {
        let out = "Implemented feature.\nTASK_FLOW_NEW_FILE src/inbox.rs\nTASK_FLOW_NEW_FILE ../escape.rs\nTASK_FLOW_NEW_FILE src/inbox.rs\nTASK_FLOW_COMPLETE\n";
        assert_eq!(
            parse_completion_contract(out),
            CompletionStatus::Complete {
                new_files: vec!["src/inbox.rs".into()]
            }
        );

        let blocked = "Working...\nTASK_FLOW_BLOCKED Waiting on schema approval\n";
        assert_eq!(
            parse_completion_contract(blocked),
            CompletionStatus::Blocked {
                reason: "Waiting on schema approval".into(),
                new_files: Vec::new()
            }
        );

        assert_eq!(
            parse_completion_contract("Talk only without marker"),
            CompletionStatus::MissingMarker {
                new_files: Vec::new()
            }
        );
        assert!(is_rate_limit_signal("HTTP 429 Too Many Requests"));
        assert!(is_rate_limit_signal("RESOURCE_EXHAUSTED on model"));
        assert!(!is_rate_limit_signal("All tests passed cleanly."));
    }

    #[test]
    fn stage_prompt_and_patch_invariants_enforce_implementation_vs_verification() {
        let checkpoint = StageCheckpoint {
            reservation_id: "res-1".into(),
            stage: StageKind::Implementation,
            head: "1111111".into(),
            branch: "ac/forge-inbox".into(),
            patch_sha256: "abc123".into(),
            checks: vec!["cargo check".into()],
            summary: "Added inbox module".into(),
            completed_at: 100,
        };
        let prompt = build_stage_prompt(&StagePromptInput {
            task_id: "M-001".into(),
            title: "Verify Decision Inbox".into(),
            body: "Check invariants and tests.".into(),
            stage: StageKind::Verification,
            prior_checkpoint: Some(checkpoint),
            decision_answer: Some("Keep zero new crates.".into()),
            team_directions: vec!["No source comments.".into()],
            source_patch: Some("diff --git a/src/inbox.rs b/src/inbox.rs".into()),
            source_patch_sha256: Some("abc123".into()),
            retry_context: None,
        });
        assert!(prompt.contains("Prior CLI handoff:"));
        assert!(prompt.contains("Keep zero new crates."));
        assert!(prompt.contains("No source comments."));
        assert!(prompt.contains("Read-only verification only"));
        assert!(prompt.contains("TASK_FLOW_COMPLETE"));

        assert_eq!(
            verify_stage_patch_invariant(StageKind::Implementation, "aaa", "aaa"),
            Err(PatchVerificationError::EmptyImplementationDiff)
        );
        assert!(verify_stage_patch_invariant(StageKind::Implementation, "aaa", "bbb").is_ok());
        assert!(verify_stage_patch_invariant(StageKind::Verification, "bbb", "bbb").is_ok());
        assert!(matches!(
            verify_stage_patch_invariant(StageKind::Verification, "bbb", "ccc"),
            Err(PatchVerificationError::VerificationMutatedWorkspace { .. })
        ));

        let untracked = vec!["src/inbox.rs".to_owned(), "src/recovery.rs".to_owned()];
        assert_eq!(
            validate_declared_new_files(&["src/inbox.rs".into()], &untracked).unwrap(),
            vec!["src/inbox.rs"]
        );
        assert!(validate_declared_new_files(&["../bad.rs".into()], &untracked).is_err());
        assert!(validate_declared_new_files(&["src/missing.rs".into()], &untracked).is_err());
    }

    #[test]
    fn reservation_ledger_enforces_exact_once_claims_heartbeat_and_no_blind_retry() {
        let dir = scratch_dir("reservations");
        let mut ledger = ReservationLedger::open(dir.clone()).unwrap();
        let workspace = dir.join("worktree");
        fs::create_dir_all(&workspace).unwrap();
        let spec = RunSpec {
            reservation_id: "res-001".into(),
            task_id: "T-100".into(),
            worker_name: "forge".into(),
            stage: StageKind::Implementation,
            work_class: WorkClass::Implementation,
            profile: "codex/main".into(),
            requested_model: Some("gpt-6.2-sol".into()),
            requested_effort: Some("high".into()),
            workspace: workspace.clone(),
            source_binding: Some(SourceBinding {
                repo: "Agent-Commander/Agent-Command".into(),
                source_revision: "rev-1".into(),
                target_revision: "main".into(),
            }),
            timeout_secs: DEFAULT_STAGE_TIMEOUT_SECS,
            created_at: 1_000,
        };
        ledger
            .reserve(&spec, 5.0, Some(("five_hour".into(), 5_000)), 1_000)
            .unwrap();
        assert_eq!(ledger.active_usage_holds().len(), 1);
        assert!(ledger.reserve(&spec, 5.0, None, 1_001).is_err());

        assert!(ledger
            .claim_once("res-001", ClaimKind::Runner, "started", 1_002)
            .unwrap());
        assert!(!ledger
            .claim_once("res-001", ClaimKind::Refusal, "late refusal", 1_003)
            .unwrap());

        ledger.write_pid("res-001", 4242, 1_002).unwrap();
        ledger.write_heartbeat("res-001", 1_010, "running").unwrap();
        assert!(!ledger.is_heartbeat_dead("res-001", 1_025));
        assert!(ledger.is_heartbeat_dead("res-001", 1_040));

        let receipt = ModelInvocationReceipt {
            requested_provider: "codex".into(),
            requested_model: Some("gpt-6.2-sol".into()),
            requested_effort: Some("high".into()),
            reported_model: Some("gpt-6.2-sol".into()),
            reported_effort: Some("high".into()),
            conversation_id: Some("conv-1".into()),
            recorded_at: 1_005,
        };
        assert!(receipt.matches_request());
        ledger.write_model_receipt("res-001", &receipt).unwrap();

        ledger.mark_unknown("res-001", 1_015).unwrap();
        assert!(ledger.active_for_task("T-100").is_some());
        assert_eq!(
            ledger.inspect("res-001", 1_020).unwrap(),
            InspectResolution::StillActive
        );
        assert!(matches!(
            ledger.inspect("res-001", 1_045).unwrap(),
            InspectResolution::SettledFailed(_)
        ));
        assert!(ledger.active_for_task("T-100").is_none());

        let mismatch = SourceBinding {
            repo: "Agent-Commander/Agent-Command".into(),
            source_revision: "rev-2".into(),
            target_revision: "main".into(),
        };
        assert!(ledger.bind_workspace_source(&workspace, &mismatch).is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn completion_wake_journal_deduplicates_and_funds_at_most_one_refill() {
        let dir = scratch_dir("wake");
        let mut journal = CompletionWakeJournal::open(dir.clone()).unwrap();
        journal
            .record_event(&CompletionEvent {
                kind: CompletionEventKind::RunReport,
                task_id: "T-1".into(),
                reservation_id: "res-1".into(),
                state: "done".into(),
                at: 100,
                pid: None,
            })
            .unwrap();
        journal
            .record_event(&CompletionEvent {
                kind: CompletionEventKind::SlotReleased,
                task_id: "T-1".into(),
                reservation_id: "res-1".into(),
                state: "done".into(),
                at: 101,
                pid: Some(1234),
            })
            .unwrap();

        let first = journal.poll_new_events(110).unwrap();
        assert_eq!(first.len(), 2);
        let refill = journal.take_pending_refill(110).unwrap().unwrap();
        assert_eq!(refill.task_id, "T-1");
        assert!(journal.take_pending_refill(111).unwrap().is_none());

        let mut reopened = CompletionWakeJournal::open(dir.clone()).unwrap();
        assert!(reopened.poll_new_events(120).unwrap().is_empty());
        assert!(reopened.take_pending_refill(120).unwrap().is_none());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scheduler_coordinates_recovery_inbox_escalation_and_ledger_docs() {
        let dir = scratch_dir("scheduler");
        let ledger_path = dir.join("ledger.json");
        let scheduler = DelegationScheduler::open(dir.clone()).unwrap();
        let mut inbox = InboxStore::default();
        let id = scheduler
            .record_task_doc(
                &ledger_path,
                "forge",
                "Port delegation engine",
                "Port TaskFlow CLI delegation into Rust.",
                "Implement atomic reservations and multi-stage handoffs.",
                100,
            )
            .unwrap();
        let key = crate::recovery::recovery_key("agent-command", &id, "rev-1");

        let first = scheduler
            .evaluate_and_escalate_to_inbox(
                &mut inbox,
                &ledger_path,
                &EscalationContext {
                    ledger_entry_id: &id,
                    task_key: &key,
                    cause: FailureCause::ExecutionFailed,
                    summary: "Worker exited with status 1",
                    now: 110,
                },
            )
            .unwrap();
        assert!(matches!(first, RecoveryDecision::Retry(_)));
        assert!(inbox.cards().is_empty());

        scheduler
            .record_recovery_attempt(
                &key,
                FailureCause::ExecutionFailed,
                "requeue",
                AttemptOutcome::Failed,
                "Requeue failed again with exit code 1",
                120,
            )
            .unwrap();
        let second = scheduler
            .evaluate_and_escalate_to_inbox(
                &mut inbox,
                &ledger_path,
                &EscalationContext {
                    ledger_entry_id: &id,
                    task_key: &key,
                    cause: FailureCause::ExecutionFailed,
                    summary: "Retry failed again",
                    now: 130,
                },
            )
            .unwrap();
        assert!(matches!(second, RecoveryDecision::Escalate(_)));
        assert_eq!(inbox.cards().len(), 1);
        assert_eq!(
            Ledger::load(&ledger_path).get(&id).unwrap().status,
            crate::ledger::TaskStatus::NeedsApproval
        );

        let card_id = inbox.cards()[0].id.clone();
        scheduler
            .sync_inbox_answer_to_ledger(
                &ledger_path,
                &id,
                &card_id,
                "Proceed with isolated retry after fixing tests.",
                140,
            )
            .unwrap();
        scheduler
            .append_task_doc(&ledger_path, &id, "Verification", "All checks green.", 145)
            .unwrap();
        assert!(scheduler
            .set_task_waiting_for(&ledger_path, &id, Some("2026-11-27: merge"), 146)
            .unwrap());
        let doc = scheduler.read_task_doc(&ledger_path, &id).unwrap();
        assert!(doc.contains("Proceed with isolated retry"));
        assert!(doc.contains("All checks green."));

        let _ = fs::remove_dir_all(dir);
    }
}
