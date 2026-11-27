use crate::{
    auth,
    memory::{
        self, DistilledLesson, MemoryKind, MemoryOrigin, MemoryScope, MemoryStore, WriteMemoryInput,
    },
    permissions::PermissionMode,
    sim, soul, specialists,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const STATE_DIRECTORY: &str = "agent-command-evolution-v1";
pub const EVOLUTION_SCHEMA: &str = "agent-command-evolution-v1";
pub const DEFAULT_PROBATION_RUNS: u32 = 3;
pub const MIN_PROBATION_RUNS: u32 = 2;
pub const MAX_PROBATION_RUNS: u32 = 5;
pub const MAX_MUTATION_ROUNDS: u32 = 2;
pub const ROLLING_WINDOW_SIZE: usize = 10;
pub const MAX_REVISIONS_PER_BOT: usize = 16;
pub const MAX_REFLECTIONS_PER_BOT: usize = 32;
pub const MAX_DURATION_SAMPLES: usize = 32;
pub const CAPABILITY_GAP_THRESHOLD: u32 = 3;
pub const CAPABILITY_GAP_WINDOW_SECS: i64 = 14 * 86_400;
pub const WILSON_Z_95: f64 = 1.959963984540054;
pub const HEALTH_MUTATION_THRESHOLD: f64 = 0.45;
pub const PROBATION_PASS_THRESHOLD: f64 = 0.70;
pub const DECOMMISSION_SUSTAINED_FLOOR: f64 = 0.30;

const REGISTRY_FILE: &str = "registry.json";
const LOCK_FILE: &str = ".evolution.lock";
const LOCK_WAIT_MS: u64 = 10;
const LOCK_ATTEMPTS: usize = 500;
const LOCK_STALE_SECS: u64 = 10;
const MAX_JOB_CHARS: usize = 240;
const MAX_ANTI_JOB_CHARS: usize = 240;
const MAX_ANTI_JOBS: usize = 12;
const MAX_VOICE_CHARS: usize = 300;
const MAX_VERIFICATION_CHARS: usize = 400;
const MAX_ALLOWED_TOOLS: usize = 32;
const MAX_TOOL_NAME_CHARS: usize = 64;
const MAX_INSTRUCTIONS_CHARS: usize = 4_000;
const MAX_ROLE_CHARS: usize = 40;
const MAX_NOTE_CHARS: usize = 400;
const MAX_CAPABILITY_GAPS: usize = 48;
const BETA_PRIOR_ALPHA: f64 = 2.0;
const BETA_PRIOR_BETA: f64 = 2.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Probation,
    Active,
    Mutating,
    UnderReview,
    Decommissioned,
}

impl LifecycleState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Probation => "probation",
            Self::Active => "active",
            Self::Mutating => "mutating",
            Self::UnderReview => "under_review",
            Self::Decommissioned => "decommissioned",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::Probation => "PROB",
            Self::Active => "ACT",
            Self::Mutating => "MUT",
            Self::UnderReview => "REV",
            Self::Decommissioned => "RET",
        }
    }

    pub fn is_routable(self) -> bool {
        self != Self::Decommissioned
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkClass {
    Coding,
    Review,
    Research,
    Docs,
    Ops,
    #[default]
    General,
}

impl WorkClass {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "coding" | "code" | "builder" => Ok(Self::Coding),
            "review" | "audit" | "qa" => Ok(Self::Review),
            "research" | "scout" => Ok(Self::Research),
            "docs" | "doc" | "writer" => Ok(Self::Docs),
            "ops" | "infra" => Ok(Self::Ops),
            "general" => Ok(Self::General),
            other => Err(format!(
                "Unknown work class {other}. Use coding, review, research, docs, ops, or general."
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Coding => "coding",
            Self::Review => "review",
            Self::Research => "research",
            Self::Docs => "docs",
            Self::Ops => "ops",
            Self::General => "general",
        }
    }

    pub fn default_permission(self) -> PermissionMode {
        match self {
            Self::Review | Self::Research => PermissionMode::ReadOnly,
            Self::Docs => PermissionMode::AutoAcceptEdits,
            Self::Coding | Self::Ops | Self::General => PermissionMode::WorktreeAuto,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotCharter {
    pub job: String,
    pub anti_jobs: Vec<String>,
    pub voice_and_style: String,
    pub verification_bar: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    pub instructions: String,
}

impl BotCharter {
    pub fn validate(&self) -> io::Result<()> {
        let _ = sanitize_required("job", &self.job, MAX_JOB_CHARS)?;
        if self.anti_jobs.is_empty() || self.anti_jobs.len() > MAX_ANTI_JOBS {
            return Err(invalid_input(format!(
                "anti_jobs must contain between 1 and {MAX_ANTI_JOBS} explicit boundaries."
            )));
        }
        for anti_job in &self.anti_jobs {
            let _ = sanitize_required("anti_job", anti_job, MAX_ANTI_JOB_CHARS)?;
        }
        let _ = sanitize_optional("voice_and_style", &self.voice_and_style, MAX_VOICE_CHARS)?;
        let _ = sanitize_required(
            "verification_bar",
            &self.verification_bar,
            MAX_VERIFICATION_CHARS,
        )?;
        if self.allowed_tools.len() > MAX_ALLOWED_TOOLS {
            return Err(invalid_input(format!(
                "allowed_tools must contain at most {MAX_ALLOWED_TOOLS} tools."
            )));
        }
        for tool in &self.allowed_tools {
            let _ = sanitize_required("allowed_tool", tool, MAX_TOOL_NAME_CHARS)?;
        }
        let _ = sanitize_required("instructions", &self.instructions, MAX_INSTRUCTIONS_CHARS)?;
        Ok(())
    }

    pub fn normalized(&self) -> io::Result<Self> {
        self.validate()?;
        let mut anti_jobs = Vec::with_capacity(self.anti_jobs.len());
        for anti_job in &self.anti_jobs {
            let cleaned = sanitize_required("anti_job", anti_job, MAX_ANTI_JOB_CHARS)?;
            if !anti_jobs
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(&cleaned))
            {
                anti_jobs.push(cleaned);
            }
        }
        let mut allowed_tools = Vec::with_capacity(self.allowed_tools.len());
        for tool in &self.allowed_tools {
            let cleaned = sanitize_required("allowed_tool", tool, MAX_TOOL_NAME_CHARS)?;
            if !allowed_tools.contains(&cleaned) {
                allowed_tools.push(cleaned);
            }
        }
        let voice = sanitize_optional("voice_and_style", &self.voice_and_style, MAX_VOICE_CHARS)?;
        Ok(Self {
            job: sanitize_required("job", &self.job, MAX_JOB_CHARS)?,
            anti_jobs,
            voice_and_style: if voice.is_empty() {
                "Plain prose, terse results, zero filler.".to_owned()
            } else {
                voice
            },
            verification_bar: sanitize_required(
                "verification_bar",
                &self.verification_bar,
                MAX_VERIFICATION_CHARS,
            )?,
            allowed_tools,
            instructions: sanitize_required(
                "instructions",
                &self.instructions,
                MAX_INSTRUCTIONS_CHARS,
            )?,
        })
    }

    pub fn sha256_short(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.job.as_bytes());
        hasher.update(b"\n");
        for anti_job in &self.anti_jobs {
            hasher.update(anti_job.as_bytes());
            hasher.update(b"\n");
        }
        hasher.update(self.voice_and_style.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.verification_bar.as_bytes());
        hasher.update(b"\n");
        for tool in &self.allowed_tools {
            hasher.update(tool.as_bytes());
            hasher.update(b"\n");
        }
        hasher.update(self.instructions.as_bytes());
        let digest = hasher.finalize();
        digest
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn render_prompt(
        &self,
        bot_id: &str,
        role: &str,
        version: u32,
        lifecycle: LifecycleState,
    ) -> String {
        let anti = self
            .anti_jobs
            .iter()
            .map(|item| format!("- DO NOT: {item}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tools = if self.allowed_tools.is_empty() {
            "role default minimal toolset".to_owned()
        } else {
            self.allowed_tools.join(", ")
        };
        format!(
            "Bot Charter: {bot_id} ({role} · rev v{version} · {})\n\
             Job: {}\n\
             Anti-Jobs:\n{anti}\n\
             Style: {}\n\
             Verification Bar: {}\n\
             Allowed Tools: {tools}\n\
             Instructions:\n{}",
            lifecycle.badge(),
            self.job,
            self.voice_and_style,
            self.verification_bar,
            self.instructions
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionStatus {
    Candidate,
    Promoted,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharterRevision {
    pub version: u32,
    pub rev_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sha: Option<String>,
    pub charter: BotCharter,
    pub rationale: String,
    #[serde(default)]
    pub diff_summary: Vec<String>,
    pub created_at: i64,
    pub status: RevisionStatus,
    pub probation_required: u32,
    pub probation_runs: u32,
    pub probation_passes: u32,
    pub probation_failures: u32,
    pub probation_disapprovals: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotBlueprint {
    pub bot_id: String,
    pub role: String,
    pub work_class: WorkClass,
    pub permission_mode: PermissionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,
    #[serde(default)]
    pub pinned_core: bool,
    pub lifecycle: LifecycleState,
    pub active_revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_known_good_revision: Option<u32>,
    pub revisions: Vec<CharterRevision>,
    #[serde(default)]
    pub mutation_rounds_failed: u32,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decommissioned_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decommission_reason: Option<String>,
}

impl BotBlueprint {
    pub fn active_charter_revision(&self) -> Option<&CharterRevision> {
        self.revisions
            .iter()
            .find(|revision| revision.version == self.active_revision)
    }

    fn active_charter_revision_mut(&mut self) -> Option<&mut CharterRevision> {
        let active = self.active_revision;
        self.revisions
            .iter_mut()
            .find(|revision| revision.version == active)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSignal {
    UserApproved,
    UserDisapproved,
    UserReverted,
    FirstTryPass,
    VerificationFailure,
    ReviewRejected,
    Stall,
    Escalation,
}

impl OutcomeSignal {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "user_approved" | "approved" | "approve" => Ok(Self::UserApproved),
            "user_disapproved" | "disapproved" | "disapprove" => Ok(Self::UserDisapproved),
            "user_reverted" | "reverted" | "revert" => Ok(Self::UserReverted),
            "first_try_pass" | "pass" => Ok(Self::FirstTryPass),
            "verification_failure" | "verify_fail" | "fail" => Ok(Self::VerificationFailure),
            "review_rejected" | "rejected" => Ok(Self::ReviewRejected),
            "stall" | "stalled" => Ok(Self::Stall),
            "escalation" | "escalated" => Ok(Self::Escalation),
            other => Err(format!(
                "Unknown feedback signal {other}. Use user_approved, user_disapproved, user_reverted, first_try_pass, verification_failure, review_rejected, stall, or escalation."
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::UserApproved => "user_approved",
            Self::UserDisapproved => "user_disapproved",
            Self::UserReverted => "user_reverted",
            Self::FirstTryPass => "first_try_pass",
            Self::VerificationFailure => "verification_failure",
            Self::ReviewRejected => "review_rejected",
            Self::Stall => "stall",
            Self::Escalation => "escalation",
        }
    }

    pub fn weights(self) -> (f64, f64) {
        match self {
            Self::UserApproved => (1.5, 0.0),
            Self::FirstTryPass => (1.0, 0.0),
            Self::UserDisapproved => (0.0, 2.0),
            Self::UserReverted => (0.0, 2.0),
            Self::VerificationFailure => (0.0, 1.0),
            Self::ReviewRejected => (0.0, 0.75),
            Self::Stall => (0.0, 0.75),
            Self::Escalation => (0.0, 0.25),
        }
    }

    pub fn is_positive(self) -> bool {
        matches!(self, Self::UserApproved | Self::FirstTryPass)
    }

    pub fn is_user_negative(self) -> bool {
        matches!(self, Self::UserDisapproved | Self::UserReverted)
    }

    pub fn is_user_feedback(self) -> bool {
        matches!(
            self,
            Self::UserApproved | Self::UserDisapproved | Self::UserReverted
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutcome {
    pub task_id: String,
    pub revision: u32,
    pub signal: OutcomeSignal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct QualityMetrics {
    pub wilson_lower_bound: f64,
    pub bayesian_mean: f64,
    pub positive_weight: f64,
    pub negative_weight: f64,
    pub sample_count: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotScorecard {
    pub user_approvals: u64,
    pub user_disapprovals: u64,
    pub user_reverts: u64,
    pub first_try_passes: u64,
    pub verification_failures: u64,
    pub review_rejections: u64,
    pub stalls: u64,
    pub escalations: u64,
    pub total_runs: u64,
    #[serde(default)]
    pub recent_durations_ms: VecDeque<u64>,
    #[serde(default)]
    pub rolling_window: VecDeque<TaskOutcome>,
}

impl BotScorecard {
    pub fn median_duration_ms(&self) -> Option<u64> {
        if self.recent_durations_ms.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.recent_durations_ms.iter().copied().collect();
        sorted.sort_unstable();
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 1 {
            Some(sorted[mid])
        } else {
            Some(sorted[mid - 1].midpoint(sorted[mid]))
        }
    }

    pub fn lifetime_metrics(&self) -> QualityMetrics {
        let pos = (self.user_approvals as f64) * 1.5 + (self.first_try_passes as f64);
        let neg = (self.user_disapprovals as f64) * 2.0
            + (self.user_reverts as f64) * 2.0
            + (self.verification_failures as f64)
            + (self.review_rejections as f64) * 0.75
            + (self.stalls as f64) * 0.75
            + (self.escalations as f64) * 0.25;
        let sample_count = usize::try_from(self.total_runs).unwrap_or(usize::MAX);
        metrics_from_weights(pos, neg, sample_count)
    }

    pub fn rolling_metrics(&self) -> QualityMetrics {
        metrics_from_outcomes(self.rolling_window.iter())
    }

    pub fn revision_metrics(&self, revision: u32) -> QualityMetrics {
        metrics_from_outcomes(
            self.rolling_window
                .iter()
                .filter(|outcome| outcome.revision == revision),
        )
    }

    pub fn rolling_user_disapprovals(&self) -> usize {
        self.rolling_window
            .iter()
            .filter(|outcome| outcome.signal.is_user_negative())
            .count()
    }

    fn record_outcome(&mut self, outcome: TaskOutcome) -> bool {
        let already_recorded_task = !outcome.task_id.is_empty()
            && self
                .rolling_window
                .iter()
                .any(|existing| existing.task_id == outcome.task_id);
        match outcome.signal {
            OutcomeSignal::UserApproved => self.user_approvals += 1,
            OutcomeSignal::UserDisapproved => self.user_disapprovals += 1,
            OutcomeSignal::UserReverted => self.user_reverts += 1,
            OutcomeSignal::FirstTryPass => self.first_try_passes += 1,
            OutcomeSignal::VerificationFailure => self.verification_failures += 1,
            OutcomeSignal::ReviewRejected => self.review_rejections += 1,
            OutcomeSignal::Stall => self.stalls += 1,
            OutcomeSignal::Escalation => self.escalations += 1,
        }
        let is_new_run = !(already_recorded_task && outcome.signal.is_user_feedback());
        if is_new_run {
            self.total_runs += 1;
        }
        if let Some(duration) = outcome.duration_ms {
            self.recent_durations_ms.push_back(duration);
            while self.recent_durations_ms.len() > MAX_DURATION_SAMPLES {
                self.recent_durations_ms.pop_front();
            }
        }
        self.rolling_window.push_back(outcome);
        while self.rolling_window.len() > ROLLING_WINDOW_SIZE {
            self.rolling_window.pop_front();
        }
        is_new_run
    }
}

pub fn wilson_lower_bound(positive: f64, total: f64, z: f64) -> f64 {
    if !positive.is_finite() || !total.is_finite() || !z.is_finite() || total <= 0.0 {
        return 0.0;
    }
    let p = (positive / total).clamp(0.0, 1.0);
    let z2 = z * z;
    let denominator = 1.0 + z2 / total;
    let center = p + z2 / (2.0 * total);
    let spread = ((p * (1.0 - p) + z2 / (4.0 * total)) / total)
        .max(0.0)
        .sqrt();
    ((center - z * spread) / denominator).clamp(0.0, 1.0)
}

pub fn bayesian_mean(positive: f64, negative: f64, alpha_prior: f64, beta_prior: f64) -> f64 {
    if !positive.is_finite()
        || !negative.is_finite()
        || !alpha_prior.is_finite()
        || !beta_prior.is_finite()
    {
        return 0.5;
    }
    let alpha = (alpha_prior + positive.max(0.0)).max(0.0);
    let beta = (beta_prior + negative.max(0.0)).max(0.0);
    let total = alpha + beta;
    if total <= 0.0 {
        0.5
    } else {
        (alpha / total).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMode {
    WrongOutput,
    ScopeCreep,
    VerificationMissed,
    Stall,
    UserRejected,
    ToolMisuse,
}

impl FailureMode {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "wrong_output" => Ok(Self::WrongOutput),
            "scope_creep" => Ok(Self::ScopeCreep),
            "verification_missed" => Ok(Self::VerificationMissed),
            "stall" => Ok(Self::Stall),
            "user_rejected" => Ok(Self::UserRejected),
            "tool_misuse" => Ok(Self::ToolMisuse),
            other => Err(format!(
                "Unknown failure mode {other}. Use wrong_output, scope_creep, verification_missed, stall, user_rejected, or tool_misuse."
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::WrongOutput => "wrong_output",
            Self::ScopeCreep => "scope_creep",
            Self::VerificationMissed => "verification_missed",
            Self::Stall => "stall",
            Self::UserRejected => "user_rejected",
            Self::ToolMisuse => "tool_misuse",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureReflection {
    pub id: String,
    pub bot_id: String,
    pub revision: u32,
    pub task_summary: String,
    pub failure_mode: FailureMode,
    pub root_cause: String,
    #[serde(default)]
    pub charter_deltas: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_correction: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityGap {
    pub signature: String,
    pub sample_task: String,
    pub suggested_role: String,
    pub occurrences: u32,
    pub first_seen: i64,
    pub last_seen: i64,
}

impl CapabilityGap {
    pub fn should_forge(&self) -> bool {
        self.occurrences >= CAPABILITY_GAP_THRESHOLD
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotRecord {
    pub blueprint: BotBlueprint,
    pub scorecard: BotScorecard,
    #[serde(default)]
    pub reflections: Vec<FailureReflection>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HealthAction {
    Healthy,
    Watch {
        note: String,
    },
    PromoteProbation {
        revision: u32,
    },
    RollbackProbation {
        from_rev: u32,
        to_rev: u32,
        reason: String,
    },
    TriggerMutation {
        reason: String,
    },
    RecommendDecommission {
        reason: String,
        requires_user_confirm: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedbackReport {
    pub bot_id: String,
    pub lifecycle: LifecycleState,
    pub active_revision: u32,
    pub rolling_metrics: QualityMetrics,
    pub lifetime_metrics: QualityMetrics,
    pub action: HealthAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BotScorecardSummary {
    pub bot_id: String,
    pub role: String,
    pub work_class: WorkClass,
    pub lifecycle: LifecycleState,
    pub badge: &'static str,
    pub pinned_core: bool,
    pub active_revision: u32,
    pub last_known_good_revision: Option<u32>,
    pub mutation_rounds_failed: u32,
    pub rolling_metrics: QualityMetrics,
    pub lifetime_metrics: QualityMetrics,
    pub median_duration_ms: Option<u64>,
    pub user_approvals: u64,
    pub user_disapprovals: u64,
    pub user_reverts: u64,
    pub first_try_passes: u64,
    pub verification_failures: u64,
    pub stalls: u64,
    pub escalations: u64,
    pub action: HealthAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DecommissionOutcome {
    NeedsConfirmation {
        bot_id: String,
        role: String,
        reason: String,
        wilson_lcb: f64,
        total_runs: u64,
        mutation_rounds_failed: u32,
    },
    Decommissioned {
        bot_id: String,
        distilled_memory_ids: Vec<String>,
        ledger_summary: String,
    },
    AlreadyDecommissioned {
        bot_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HealthcheckFinding {
    pub bot_id: String,
    pub lifecycle: LifecycleState,
    pub action: HealthAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HealthcheckReport {
    pub findings: Vec<HealthcheckFinding>,
    pub ready_gaps: Vec<CapabilityGap>,
}

impl HealthcheckReport {
    pub fn is_quiet(&self) -> bool {
        self.findings.is_empty() && self.ready_gaps.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForgeBotRequest {
    pub bot_id: String,
    pub role: String,
    pub work_class: WorkClass,
    pub charter: BotCharter,
    pub permission_mode: Option<PermissionMode>,
    pub preferred_model: Option<String>,
    pub pinned_core: bool,
    pub initial_memory_seeds: Vec<String>,
    pub probation_runs: Option<u32>,
    pub rationale: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MutateBotRequest {
    pub bot_id: String,
    pub failure_mode: FailureMode,
    pub task_summary: String,
    pub root_cause: String,
    pub patched_charter: BotCharter,
    pub diff_summary: Vec<String>,
    pub memory_correction: Option<String>,
    pub probation_runs: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    schema: String,
    bots: Vec<BotRecord>,
    #[serde(default)]
    capability_gaps: Vec<CapabilityGap>,
}

#[derive(Clone, Debug)]
pub struct EvolutionStore {
    root: PathBuf,
    workspace_hash: String,
}

impl EvolutionStore {
    pub fn new(root: PathBuf, workspace_hash: impl Into<String>) -> io::Result<Self> {
        let hash = normalize_workspace_hash(&workspace_hash.into())?;
        Ok(Self {
            root,
            workspace_hash: hash,
        })
    }

    pub fn for_workspace(root: PathBuf, workspace: &Path) -> Self {
        Self {
            root,
            workspace_hash: specialists::workspace_hash(workspace),
        }
    }

    pub fn registry_path(&self) -> io::Result<PathBuf> {
        Ok(self.workspace_dir()?.join(REGISTRY_FILE))
    }

    pub fn forge_bot(
        &self,
        memory_store: &MemoryStore,
        request: ForgeBotRequest,
        now: i64,
    ) -> io::Result<BotRecord> {
        let bot_id = memory::normalize_bot_id(&request.bot_id)?;
        if bot_id.eq_ignore_ascii_case(specialists::COMMANDER) {
            return Err(invalid_input("Cannot forge a bot named Commander."));
        }
        let role = sanitize_required("role", &request.role, MAX_ROLE_CHARS)?;
        let charter = request.charter.normalized()?;
        let rationale = sanitize_required("rationale", &request.rationale, MAX_NOTE_CHARS)?;
        let preferred_model = match request.preferred_model {
            Some(model) => Some(sanitize_required("preferred_model", &model, 80)?),
            None => None,
        };
        let probation_required = request
            .probation_runs
            .unwrap_or(DEFAULT_PROBATION_RUNS)
            .clamp(MIN_PROBATION_RUNS, MAX_PROBATION_RUNS);
        let permission_mode = request
            .permission_mode
            .unwrap_or_else(|| request.work_class.default_permission());

        for seed in &request.initial_memory_seeds {
            if soul::contains_secret_like(seed) {
                return Err(invalid_input(
                    "Refused secret-like material in initial_memory_seeds.",
                ));
            }
        }

        let dir = self.workspace_dir()?;
        let _lock = StoreLock::acquire(&dir)?;
        let mut registry = load_registry(&dir)?;
        if registry
            .bots
            .iter()
            .any(|record| record.blueprint.bot_id == bot_id)
        {
            return Err(invalid_input(format!(
                "Bot {bot_id} already exists in evolution registry."
            )));
        }

        let rev_sha = charter.sha256_short();
        let revision = CharterRevision {
            version: 1,
            rev_sha,
            parent_sha: None,
            charter,
            rationale,
            diff_summary: vec!["+ Initial forged charter".to_owned()],
            created_at: now,
            status: RevisionStatus::Candidate,
            probation_required,
            probation_runs: 0,
            probation_passes: 0,
            probation_failures: 0,
            probation_disapprovals: 0,
        };
        let record = BotRecord {
            blueprint: BotBlueprint {
                bot_id: bot_id.clone(),
                role,
                work_class: request.work_class,
                permission_mode,
                preferred_model,
                pinned_core: request.pinned_core,
                lifecycle: LifecycleState::Probation,
                active_revision: 1,
                last_known_good_revision: None,
                revisions: vec![revision],
                mutation_rounds_failed: 0,
                created_at: now,
                updated_at: now,
                decommissioned_at: None,
                decommission_reason: None,
            },
            scorecard: BotScorecard::default(),
            reflections: Vec::new(),
        };
        registry.bots.push(record.clone());
        save_registry(&dir, &registry)?;

        for (index, seed) in request.initial_memory_seeds.into_iter().enumerate() {
            let trimmed = seed.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = memory_store.write(
                WriteMemoryInput {
                    scope: MemoryScope::Bot(bot_id.clone()),
                    kind: MemoryKind::Playbook,
                    title: format!("Initial seed #{}", index + 1),
                    body: trimmed.to_owned(),
                    tags: vec!["seed".to_owned(), request.work_class.label().to_owned()],
                    author: "forge".to_owned(),
                    origin: MemoryOrigin::Commander,
                    salience: Some(0.85),
                    confidence: Some(0.90),
                    pinned: false,
                    ttl_secs: None,
                },
                now,
            )?;
        }
        Ok(record)
    }

    pub fn record_capability_gap(
        &self,
        signature: &str,
        sample_task: &str,
        suggested_role: &str,
        now: i64,
    ) -> io::Result<Option<CapabilityGap>> {
        let sig = sanitize_required("signature", signature, 80)?.to_ascii_lowercase();
        let sample = sanitize_required("sample_task", sample_task, MAX_NOTE_CHARS)?;
        let role = sanitize_required("suggested_role", suggested_role, MAX_ROLE_CHARS)?;
        let dir = self.workspace_dir()?;
        let _lock = StoreLock::acquire(&dir)?;
        let mut registry = load_registry(&dir)?;
        registry
            .capability_gaps
            .retain(|gap| now.saturating_sub(gap.last_seen) <= CAPABILITY_GAP_WINDOW_SECS);
        let updated = if let Some(gap) = registry
            .capability_gaps
            .iter_mut()
            .find(|gap| gap.signature == sig)
        {
            gap.occurrences = gap.occurrences.saturating_add(1);
            gap.last_seen = now;
            gap.sample_task = sample;
            gap.clone()
        } else {
            let gap = CapabilityGap {
                signature: sig,
                sample_task: sample,
                suggested_role: role,
                occurrences: 1,
                first_seen: now,
                last_seen: now,
            };
            registry.capability_gaps.push(gap.clone());
            if registry.capability_gaps.len() > MAX_CAPABILITY_GAPS {
                registry
                    .capability_gaps
                    .sort_by_key(|gap| std::cmp::Reverse(gap.last_seen));
                registry.capability_gaps.truncate(MAX_CAPABILITY_GAPS);
            }
            gap
        };
        save_registry(&dir, &registry)?;
        Ok(updated.should_forge().then_some(updated))
    }

    pub fn record_feedback(
        &self,
        bot_id: &str,
        signal: OutcomeSignal,
        task_id: Option<&str>,
        duration_ms: Option<u64>,
        note: Option<&str>,
        now: i64,
    ) -> io::Result<FeedbackReport> {
        let normalized_id = memory::normalize_bot_id(bot_id)?;
        let cleaned_note = match note {
            Some(text) if !text.trim().is_empty() => {
                Some(sanitize_required("note", text, MAX_NOTE_CHARS)?)
            }
            _ => None,
        };
        let cleaned_task_id = match task_id {
            Some(id) if !id.trim().is_empty() => sanitize_required("task_id", id, 64)?,
            _ => format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
        };

        let dir = self.workspace_dir()?;
        let _lock = StoreLock::acquire(&dir)?;
        let mut registry = load_registry(&dir)?;
        let record = registry
            .bots
            .iter_mut()
            .find(|record| record.blueprint.bot_id == normalized_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("No forged bot named {normalized_id}."),
                )
            })?;
        if record.blueprint.lifecycle == LifecycleState::Decommissioned {
            return Err(invalid_input(format!(
                "Bot {normalized_id} is decommissioned and cannot accept new feedback."
            )));
        }

        let revision = record.blueprint.active_revision;
        let is_new_run = record.scorecard.record_outcome(TaskOutcome {
            task_id: cleaned_task_id,
            revision,
            signal,
            duration_ms,
            note: cleaned_note,
            at: now,
        });
        record.blueprint.updated_at = now;
        let action = evaluate_and_transition(record, signal, is_new_run);
        let report = FeedbackReport {
            bot_id: record.blueprint.bot_id.clone(),
            lifecycle: record.blueprint.lifecycle,
            active_revision: record.blueprint.active_revision,
            rolling_metrics: record.scorecard.rolling_metrics(),
            lifetime_metrics: record.scorecard.lifetime_metrics(),
            action,
        };
        save_registry(&dir, &registry)?;
        Ok(report)
    }

    pub fn propose_mutation(
        &self,
        memory_store: &MemoryStore,
        request: MutateBotRequest,
        now: i64,
    ) -> io::Result<CharterRevision> {
        let bot_id = memory::normalize_bot_id(&request.bot_id)?;
        let task_summary =
            sanitize_required("task_summary", &request.task_summary, MAX_NOTE_CHARS)?;
        let root_cause = sanitize_required("root_cause", &request.root_cause, MAX_NOTE_CHARS)?;
        let patched = request.patched_charter.normalized()?;
        if request.diff_summary.is_empty() {
            return Err(invalid_input(
                "diff_summary must contain at least one concrete charter delta.",
            ));
        }
        let mut diff_summary = Vec::with_capacity(request.diff_summary.len());
        for delta in &request.diff_summary {
            diff_summary.push(sanitize_required("diff_summary", delta, 240)?);
        }
        let correction = match request.memory_correction {
            Some(text) if !text.trim().is_empty() => {
                Some(sanitize_required("memory_correction", &text, 600)?)
            }
            _ => None,
        };
        let probation_required = request
            .probation_runs
            .unwrap_or(DEFAULT_PROBATION_RUNS)
            .clamp(MIN_PROBATION_RUNS, MAX_PROBATION_RUNS);

        let dir = self.workspace_dir()?;
        let _lock = StoreLock::acquire(&dir)?;
        let mut registry = load_registry(&dir)?;
        let record = registry
            .bots
            .iter_mut()
            .find(|record| record.blueprint.bot_id == bot_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("No forged bot named {bot_id}."),
                )
            })?;
        if record.blueprint.lifecycle == LifecycleState::Decommissioned {
            return Err(invalid_input(format!(
                "Bot {bot_id} is decommissioned and cannot be mutated."
            )));
        }

        let parent_rev = record.blueprint.active_revision;
        let parent_sha = record
            .blueprint
            .active_charter_revision()
            .map(|rev| rev.rev_sha.clone());
        let next_version = record
            .blueprint
            .revisions
            .iter()
            .map(|rev| rev.version)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let reflection = FailureReflection {
            id: format!("ref-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
            bot_id: bot_id.clone(),
            revision: parent_rev,
            task_summary,
            failure_mode: request.failure_mode,
            root_cause: root_cause.clone(),
            charter_deltas: diff_summary.clone(),
            memory_correction: correction.clone(),
            created_at: now,
        };
        record.reflections.push(reflection);
        if record.reflections.len() > MAX_REFLECTIONS_PER_BOT {
            let excess = record.reflections.len() - MAX_REFLECTIONS_PER_BOT;
            record.reflections.drain(..excess);
        }

        let rev = CharterRevision {
            version: next_version,
            rev_sha: patched.sha256_short(),
            parent_sha,
            charter: patched,
            rationale: format!("{}: {root_cause}", request.failure_mode.label()),
            diff_summary,
            created_at: now,
            status: RevisionStatus::Candidate,
            probation_required,
            probation_runs: 0,
            probation_passes: 0,
            probation_failures: 0,
            probation_disapprovals: 0,
        };
        record.blueprint.revisions.push(rev.clone());
        if record.blueprint.revisions.len() > MAX_REVISIONS_PER_BOT {
            let keep_lkg = record.blueprint.last_known_good_revision;
            if let Some(pos) = record
                .blueprint
                .revisions
                .iter()
                .position(|item| Some(item.version) != keep_lkg && item.version != next_version)
            {
                record.blueprint.revisions.remove(pos);
            }
        }
        record.blueprint.active_revision = next_version;
        record.blueprint.lifecycle = LifecycleState::Probation;
        record.blueprint.updated_at = now;
        save_registry(&dir, &registry)?;

        if let Some(correction_body) = correction {
            let _ = memory_store.write(
                WriteMemoryInput {
                    scope: MemoryScope::Bot(bot_id.clone()),
                    kind: MemoryKind::Correction,
                    title: format!("v{next_version} {}", request.failure_mode.label()),
                    body: correction_body,
                    tags: vec![
                        "reflection".to_owned(),
                        request.failure_mode.label().to_owned(),
                    ],
                    author: bot_id,
                    origin: MemoryOrigin::Reflection,
                    salience: Some(0.90),
                    confidence: Some(0.95),
                    pinned: false,
                    ttl_secs: None,
                },
                now,
            )?;
        }
        Ok(rev)
    }

    pub fn decommission_bot(
        &self,
        memory_store: &MemoryStore,
        bot_id: &str,
        reason: &str,
        confirmed: bool,
        now: i64,
    ) -> io::Result<DecommissionOutcome> {
        let normalized_id = memory::normalize_bot_id(bot_id)?;
        let reason = sanitize_required("reason", reason, MAX_NOTE_CHARS)?;
        let dir = self.workspace_dir()?;
        let _lock = StoreLock::acquire(&dir)?;
        let mut registry = load_registry(&dir)?;
        let record = registry
            .bots
            .iter_mut()
            .find(|record| record.blueprint.bot_id == normalized_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("No forged bot named {normalized_id}."),
                )
            })?;
        if record.blueprint.lifecycle == LifecycleState::Decommissioned {
            return Ok(DecommissionOutcome::AlreadyDecommissioned {
                bot_id: normalized_id,
            });
        }
        if record.blueprint.pinned_core && !confirmed {
            let rolling = record.scorecard.rolling_metrics();
            return Ok(DecommissionOutcome::NeedsConfirmation {
                bot_id: normalized_id,
                role: record.blueprint.role.clone(),
                reason,
                wilson_lcb: rolling.wilson_lower_bound,
                total_runs: record.scorecard.total_runs,
                mutation_rounds_failed: record.blueprint.mutation_rounds_failed,
            });
        }

        let extra_lessons: Vec<DistilledLesson> = record
            .reflections
            .iter()
            .rev()
            .take(4)
            .map(|reflection| DistilledLesson {
                kind: MemoryKind::Warning,
                title: format!(
                    "{} failure ({})",
                    record.blueprint.role,
                    reflection.failure_mode.label()
                ),
                body: reflection.root_cause.clone(),
                tags: vec![
                    format!("role:{}", record.blueprint.role.to_ascii_lowercase()),
                    reflection.failure_mode.label().to_owned(),
                ],
                salience: 0.85,
            })
            .collect();
        let distilled = memory_store.distill_bot_lessons(&normalized_id, &extra_lessons, now)?;
        let distilled_memory_ids: Vec<String> =
            distilled.into_iter().map(|entry| entry.id).collect();

        record.blueprint.lifecycle = LifecycleState::Decommissioned;
        record.blueprint.decommissioned_at = Some(now);
        record.blueprint.decommission_reason = Some(reason.clone());
        record.blueprint.updated_at = now;
        let ledger_summary = format!(
            "Decommissioned {} ({}) after {} runs ({} distilled lessons): {reason}",
            normalized_id,
            record.blueprint.role,
            record.scorecard.total_runs,
            distilled_memory_ids.len()
        );
        save_registry(&dir, &registry)?;
        Ok(DecommissionOutcome::Decommissioned {
            bot_id: normalized_id,
            distilled_memory_ids,
            ledger_summary,
        })
    }

    pub fn active_bots(&self) -> io::Result<Vec<BotRecord>> {
        let Some(dir) = self.existing_workspace_dir()? else {
            return Ok(Vec::new());
        };
        let registry = load_registry(&dir)?;
        Ok(registry
            .bots
            .into_iter()
            .filter(|record| record.blueprint.lifecycle.is_routable())
            .collect())
    }

    pub fn get_bot(&self, bot_id: &str) -> io::Result<Option<BotRecord>> {
        let normalized_id = memory::normalize_bot_id(bot_id)?;
        let Some(dir) = self.existing_workspace_dir()? else {
            return Ok(None);
        };
        let registry = load_registry(&dir)?;
        Ok(registry
            .bots
            .into_iter()
            .find(|record| record.blueprint.bot_id == normalized_id))
    }

    pub fn render_bot_charter_prompt(&self, bot_id: &str) -> io::Result<Option<String>> {
        let Some(record) = self.get_bot(bot_id)? else {
            return Ok(None);
        };
        if !record.blueprint.lifecycle.is_routable() {
            return Ok(None);
        }
        Ok(record.blueprint.active_charter_revision().map(|rev| {
            rev.charter.render_prompt(
                &record.blueprint.bot_id,
                &record.blueprint.role,
                rev.version,
                record.blueprint.lifecycle,
            )
        }))
    }

    pub fn scorecard_report(&self, bot_id: Option<&str>) -> io::Result<Vec<BotScorecardSummary>> {
        let Some(dir) = self.existing_workspace_dir()? else {
            return Ok(Vec::new());
        };
        let registry = load_registry(&dir)?;
        let filter = match bot_id {
            Some(raw) => Some(memory::normalize_bot_id(raw)?),
            None => None,
        };
        let mut summaries = Vec::new();
        for record in &registry.bots {
            if let Some(target) = &filter {
                if &record.blueprint.bot_id != target {
                    continue;
                }
            }
            summaries.push(summarize_record(record));
        }
        Ok(summaries)
    }

    pub fn healthcheck_audit(&self, now: i64) -> io::Result<HealthcheckReport> {
        let Some(dir) = self.existing_workspace_dir()? else {
            return Ok(HealthcheckReport {
                findings: Vec::new(),
                ready_gaps: Vec::new(),
            });
        };
        let registry = load_registry(&dir)?;
        let mut findings = Vec::new();
        for record in &registry.bots {
            if !record.blueprint.lifecycle.is_routable() {
                continue;
            }
            let action = inspect_health(record);
            if !matches!(action, HealthAction::Healthy) {
                findings.push(HealthcheckFinding {
                    bot_id: record.blueprint.bot_id.clone(),
                    lifecycle: record.blueprint.lifecycle,
                    action,
                });
            }
        }
        let ready_gaps = registry
            .capability_gaps
            .into_iter()
            .filter(|gap| {
                gap.should_forge()
                    && now.saturating_sub(gap.last_seen) <= CAPABILITY_GAP_WINDOW_SECS
            })
            .collect();
        Ok(HealthcheckReport {
            findings,
            ready_gaps,
        })
    }

    fn workspace_dir(&self) -> io::Result<PathBuf> {
        let base = auth::private_state_directory(&self.root, STATE_DIRECTORY)?;
        auth::private_state_directory(&base, &self.workspace_hash)
    }

    fn existing_workspace_dir(&self) -> io::Result<Option<PathBuf>> {
        let Some(base) = auth::existing_private_state(&self.root, STATE_DIRECTORY)? else {
            return Ok(None);
        };
        auth::existing_private_state(&base, &self.workspace_hash)
    }
}

fn evaluate_and_transition(
    record: &mut BotRecord,
    signal: OutcomeSignal,
    is_new_run: bool,
) -> HealthAction {
    if record.blueprint.lifecycle == LifecycleState::Probation {
        return evaluate_probation_step(record, signal, is_new_run);
    }

    let rolling = record.scorecard.rolling_metrics();
    let disapprovals = record.scorecard.rolling_user_disapprovals();
    if record.blueprint.mutation_rounds_failed >= MAX_MUTATION_ROUNDS {
        record.blueprint.lifecycle = LifecycleState::UnderReview;
        return HealthAction::RecommendDecommission {
            reason: format!("Exhausted {MAX_MUTATION_ROUNDS} failed charter mutation rounds."),
            requires_user_confirm: record.blueprint.pinned_core,
        };
    }
    if disapprovals >= 3
        || (rolling.sample_count >= 6
            && rolling.wilson_lower_bound < DECOMMISSION_SUSTAINED_FLOOR
            && record.blueprint.mutation_rounds_failed >= 1)
    {
        record.blueprint.lifecycle = LifecycleState::UnderReview;
        return HealthAction::RecommendDecommission {
            reason: format!(
                "Sustained low rolling quality (Wilson LCB {:.2}, {disapprovals} recent disapprovals).",
                rolling.wilson_lower_bound
            ),
            requires_user_confirm: record.blueprint.pinned_core,
        };
    }
    if signal.is_user_negative() {
        record.blueprint.lifecycle = LifecycleState::UnderReview;
        return HealthAction::TriggerMutation {
            reason: format!("Received {} feedback from operator.", signal.label()),
        };
    }
    if rolling.sample_count >= 4 && rolling.wilson_lower_bound < HEALTH_MUTATION_THRESHOLD {
        record.blueprint.lifecycle = LifecycleState::UnderReview;
        return HealthAction::TriggerMutation {
            reason: format!(
                "Rolling Wilson LCB {:.2} fell below {:.2}.",
                rolling.wilson_lower_bound, HEALTH_MUTATION_THRESHOLD
            ),
        };
    }
    if rolling.sample_count >= 3 && rolling.wilson_lower_bound < 0.60 {
        return HealthAction::Watch {
            note: format!(
                "Rolling Wilson LCB {:.2} is trending below target.",
                rolling.wilson_lower_bound
            ),
        };
    }
    record.blueprint.lifecycle = LifecycleState::Active;
    HealthAction::Healthy
}

fn evaluate_probation_step(
    record: &mut BotRecord,
    signal: OutcomeSignal,
    is_new_run: bool,
) -> HealthAction {
    let active_version = record.blueprint.active_revision;
    let lkg = record.blueprint.last_known_good_revision;
    let pinned_core = record.blueprint.pinned_core;

    let Some(rev) = record.blueprint.active_charter_revision_mut() else {
        return HealthAction::TriggerMutation {
            reason: "Active charter revision missing.".to_owned(),
        };
    };

    if is_new_run {
        rev.probation_runs = rev.probation_runs.saturating_add(1);
    }
    if signal.is_positive() {
        if is_new_run {
            rev.probation_passes = rev.probation_passes.saturating_add(1);
        }
    } else if signal.is_user_negative() {
        rev.probation_disapprovals = rev.probation_disapprovals.saturating_add(1);
        rev.probation_failures = rev.probation_failures.saturating_add(1);
        if !is_new_run {
            rev.probation_passes = rev.probation_passes.saturating_sub(1);
        }
    } else {
        rev.probation_failures = rev.probation_failures.saturating_add(1);
    }

    let fast_fail = rev.probation_disapprovals >= 1 || rev.probation_failures >= 2;
    let completed = rev.probation_runs >= rev.probation_required;
    let pass_ratio = if rev.probation_runs == 0 {
        0.0
    } else {
        f64::from(rev.probation_passes) / f64::from(rev.probation_runs)
    };

    if !fast_fail && completed && pass_ratio >= PROBATION_PASS_THRESHOLD {
        rev.status = RevisionStatus::Promoted;
        record.blueprint.last_known_good_revision = Some(active_version);
        record.blueprint.lifecycle = LifecycleState::Active;
        record.blueprint.mutation_rounds_failed = 0;
        return HealthAction::PromoteProbation {
            revision: active_version,
        };
    }

    if fast_fail || completed {
        let reason = if rev.probation_disapprovals >= 1 {
            format!(
                "Probation v{active_version} received {} user disapproval(s).",
                rev.probation_disapprovals
            )
        } else {
            format!(
                "Probation v{active_version} failed ({}/{} passes, {} failures).",
                rev.probation_passes, rev.probation_runs, rev.probation_failures
            )
        };
        rev.status = RevisionStatus::RolledBack;
        record.blueprint.lifecycle = LifecycleState::UnderReview;
        if let Some(target_rev) = lkg {
            record.blueprint.active_revision = target_rev;
            record.blueprint.mutation_rounds_failed =
                record.blueprint.mutation_rounds_failed.saturating_add(1);
            if record.blueprint.mutation_rounds_failed >= MAX_MUTATION_ROUNDS {
                return HealthAction::RecommendDecommission {
                    reason: format!(
                        "{reason} Rolled back to v{target_rev}; reached {MAX_MUTATION_ROUNDS} failed mutation rounds."
                    ),
                    requires_user_confirm: pinned_core,
                };
            }
            return HealthAction::RollbackProbation {
                from_rev: active_version,
                to_rev: target_rev,
                reason,
            };
        }
        record.blueprint.mutation_rounds_failed =
            record.blueprint.mutation_rounds_failed.saturating_add(1);
        if record.blueprint.mutation_rounds_failed >= MAX_MUTATION_ROUNDS {
            return HealthAction::RecommendDecommission {
                reason: format!(
                    "{reason} Initial probation and retry mutations did not stabilize."
                ),
                requires_user_confirm: pinned_core,
            };
        }
        return HealthAction::TriggerMutation { reason };
    }

    HealthAction::Healthy
}

fn inspect_health(record: &BotRecord) -> HealthAction {
    let rolling = record.scorecard.rolling_metrics();
    let disapprovals = record.scorecard.rolling_user_disapprovals();
    if record.blueprint.mutation_rounds_failed >= MAX_MUTATION_ROUNDS {
        return HealthAction::RecommendDecommission {
            reason: format!("Exhausted {MAX_MUTATION_ROUNDS} failed charter mutation rounds."),
            requires_user_confirm: record.blueprint.pinned_core,
        };
    }
    if disapprovals >= 3
        || (rolling.sample_count >= 6
            && rolling.wilson_lower_bound < DECOMMISSION_SUSTAINED_FLOOR
            && record.blueprint.mutation_rounds_failed >= 1)
    {
        return HealthAction::RecommendDecommission {
            reason: format!(
                "Sustained low rolling quality (Wilson LCB {:.2}, {disapprovals} recent disapprovals).",
                rolling.wilson_lower_bound
            ),
            requires_user_confirm: record.blueprint.pinned_core,
        };
    }
    if record.blueprint.lifecycle == LifecycleState::UnderReview {
        return HealthAction::TriggerMutation {
            reason: "Bot is under review after prior failure or rollback.".to_owned(),
        };
    }
    if rolling.sample_count >= 4 && rolling.wilson_lower_bound < HEALTH_MUTATION_THRESHOLD {
        return HealthAction::TriggerMutation {
            reason: format!(
                "Rolling Wilson LCB {:.2} fell below {:.2}.",
                rolling.wilson_lower_bound, HEALTH_MUTATION_THRESHOLD
            ),
        };
    }
    HealthAction::Healthy
}

fn summarize_record(record: &BotRecord) -> BotScorecardSummary {
    BotScorecardSummary {
        bot_id: record.blueprint.bot_id.clone(),
        role: record.blueprint.role.clone(),
        work_class: record.blueprint.work_class,
        lifecycle: record.blueprint.lifecycle,
        badge: record.blueprint.lifecycle.badge(),
        pinned_core: record.blueprint.pinned_core,
        active_revision: record.blueprint.active_revision,
        last_known_good_revision: record.blueprint.last_known_good_revision,
        mutation_rounds_failed: record.blueprint.mutation_rounds_failed,
        rolling_metrics: record.scorecard.rolling_metrics(),
        lifetime_metrics: record.scorecard.lifetime_metrics(),
        median_duration_ms: record.scorecard.median_duration_ms(),
        user_approvals: record.scorecard.user_approvals,
        user_disapprovals: record.scorecard.user_disapprovals,
        user_reverts: record.scorecard.user_reverts,
        first_try_passes: record.scorecard.first_try_passes,
        verification_failures: record.scorecard.verification_failures,
        stalls: record.scorecard.stalls,
        escalations: record.scorecard.escalations,
        action: if record.blueprint.lifecycle == LifecycleState::Decommissioned {
            HealthAction::Healthy
        } else {
            inspect_health(record)
        },
    }
}

fn metrics_from_outcomes<'a>(outcomes: impl Iterator<Item = &'a TaskOutcome>) -> QualityMetrics {
    let mut pos = 0.0;
    let mut neg = 0.0;
    let mut count = 0usize;
    for outcome in outcomes {
        let (p, n) = outcome.signal.weights();
        pos += p;
        neg += n;
        count += 1;
    }
    metrics_from_weights(pos, neg, count)
}

fn metrics_from_weights(pos: f64, neg: f64, sample_count: usize) -> QualityMetrics {
    let total = pos + neg;
    QualityMetrics {
        wilson_lower_bound: wilson_lower_bound(pos, total, WILSON_Z_95),
        bayesian_mean: bayesian_mean(pos, neg, BETA_PRIOR_ALPHA, BETA_PRIOR_BETA),
        positive_weight: pos,
        negative_weight: neg,
        sample_count,
    }
}

fn load_registry(workspace_dir: &Path) -> io::Result<RegistryFile> {
    let path = workspace_dir.join(REGISTRY_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RegistryFile {
                schema: EVOLUTION_SCHEMA.to_owned(),
                bots: Vec::new(),
                capability_gaps: Vec::new(),
            });
        }
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "Failed reading evolution registry {}: {error}",
                    path.display()
                ),
            ));
        }
    };
    let parsed: RegistryFile = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Corrupt evolution registry {}: {error}", path.display()),
        )
    })?;
    if parsed.schema != EVOLUTION_SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Unsupported evolution schema {} in {}",
                parsed.schema,
                path.display()
            ),
        ));
    }
    Ok(parsed)
}

fn save_registry(workspace_dir: &Path, registry: &RegistryFile) -> io::Result<()> {
    let path = workspace_dir.join(REGISTRY_FILE);
    let bytes = serde_json::to_vec_pretty(registry).map_err(io::Error::other)?;
    write_atomic(&path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let write_result = (|| -> io::Result<()> {
        let mut file = fs::File::create_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    fn acquire(directory: &Path) -> io::Result<Self> {
        let path = directory.join(LOCK_FILE);
        for _ in 0..LOCK_ATTEMPTS {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age >= Duration::from_secs(LOCK_STALE_SECS));
                    if stale {
                        let _ = fs::remove_file(&path);
                    } else {
                        std::thread::sleep(Duration::from_millis(LOCK_WAIT_MS));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Timed out waiting for evolution lock {}", path.display()),
        ))
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn normalize_workspace_hash(raw: &str) -> io::Result<String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    let valid = (1..=64).contains(&trimmed.len())
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if !valid {
        return Err(invalid_input(
            "workspace_hash must use 1-64 ASCII alphanumeric, - or _ characters.",
        ));
    }
    Ok(trimmed)
}

fn sanitize_required(label: &str, text: &str, max_chars: usize) -> io::Result<String> {
    if text.contains('\0') {
        return Err(invalid_input(format!(
            "{label} must not contain NUL bytes."
        )));
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err(invalid_input(format!("{label} must not be empty.")));
    }
    if collapsed.chars().count() > max_chars {
        return Err(invalid_input(format!(
            "{label} must be at most {max_chars} characters."
        )));
    }
    if soul::contains_secret_like(&collapsed) {
        return Err(invalid_input(format!(
            "Refused secret-like material in {label}."
        )));
    }
    Ok(collapsed)
}

fn sanitize_optional(label: &str, text: &str, max_chars: usize) -> io::Result<String> {
    if text.contains('\0') {
        return Err(invalid_input(format!(
            "{label} must not contain NUL bytes."
        )));
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max_chars {
        return Err(invalid_input(format!(
            "{label} must be at most {max_chars} characters."
        )));
    }
    if soul::contains_secret_like(&collapsed) {
        return Err(invalid_input(format!(
            "Refused secret-like material in {label}."
        )));
    }
    Ok(collapsed)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub const SIM_EVOLUTION_DOMAIN: &str = "bot-evolution";

pub fn sim_attempt_for_mutation(
    record: &BotRecord,
    request: &MutateBotRequest,
    revision: &CharterRevision,
) -> sim::LogAttemptInput {
    let bot_id = record.blueprint.bot_id.clone();
    let failure_mode = request.failure_mode.label();
    let avoid_guidance = request
        .memory_correction
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| {
            format!(
                "Do not repeat the approach that charter v{} replaced: {}. Keep the charter anti-jobs and verification bar; they exist to block {failure_mode}.",
                revision.version,
                request.root_cause.trim()
            )
        });
    sim::LogAttemptInput {
        title: clip_chars(request.root_cause.trim(), sim::MAX_TITLE_CHARS),
        component: Some(record.blueprint.work_class.label().to_owned()),
        domain: Some(SIM_EVOLUTION_DOMAIN.to_owned()),
        problem_tags: vec!["bot-evolve".to_owned(), failure_mode.to_owned()],
        outcome: Some(sim::SimOutcome::Failed),
        attempt_summary: clip_chars(
            &format!(
                "{bot_id} failed ({failure_mode}): {}",
                request.task_summary.trim()
            ),
            sim::MAX_TEXT_CHARS,
        ),
        rationale: clip_chars(
            &format!(
                "Proposed charter v{}: {}",
                revision.version,
                revision.diff_summary.join("; ")
            ),
            sim::MAX_TEXT_CHARS,
        ),
        predicted_outcome: format!(
            "Charter v{} passes {} probation run(s).",
            revision.version, revision.probation_required
        ),
        observed_outcome: clip_chars(request.root_cause.trim(), sim::MAX_TEXT_CHARS),
        avoid_guidance: clip_chars(&avoid_guidance, sim::MAX_TEXT_CHARS),
        bot_id: Some(bot_id),
        model: sim_model_label(record),
        charter_revision: Some(revision.version),
        tags: vec!["charter-mutation".to_owned(), failure_mode.to_owned()],
        ..sim::LogAttemptInput::default()
    }
}

pub fn sim_attempt_for_feedback(
    record: &BotRecord,
    report: &FeedbackReport,
    task_id: Option<&str>,
    note: Option<&str>,
) -> Option<sim::LogAttemptInput> {
    let bot_id = record.blueprint.bot_id.clone();
    let observed = note
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| clip_chars(text, sim::MAX_TEXT_CHARS))
        .unwrap_or_default();
    let (outcome, revision, summary, avoid_guidance, tag) = match &report.action {
        HealthAction::PromoteProbation { revision } => (
            sim::SimOutcome::Worked,
            Some(*revision),
            format!("Charter v{revision} of {bot_id} passed probation and was promoted."),
            String::new(),
            "probation-promoted",
        ),
        HealthAction::RollbackProbation {
            from_rev,
            to_rev,
            reason,
        } => (
            sim::SimOutcome::Reverted,
            Some(*from_rev),
            format!("Charter v{from_rev} of {bot_id} was rolled back to v{to_rev}: {reason}"),
            format!(
                "Do not reapply charter v{from_rev} unchanged ({reason}). Keep the last known good v{to_rev} safeguards that the rollback restored."
            ),
            "probation-rollback",
        ),
        HealthAction::RecommendDecommission { reason, .. }
            if record.blueprint.mutation_rounds_failed > 0 =>
        {
            (
                sim::SimOutcome::Regressed,
                None,
                format!(
                    "{bot_id} recommended for decommission after {} failed mutation round(s): {reason}",
                    record.blueprint.mutation_rounds_failed
                ),
                format!(
                    "Stop mutating {bot_id} along the same line ({reason}). Address the recorded root causes before forging a replacement charter."
                ),
                "decommission-recommended",
            )
        }
        _ => return None,
    };
    let fallback_revision = revision.unwrap_or(report.active_revision);
    Some(sim::LogAttemptInput {
        title: format!("{bot_id} charter v{fallback_revision} probation"),
        component: Some(record.blueprint.work_class.label().to_owned()),
        domain: Some(SIM_EVOLUTION_DOMAIN.to_owned()),
        problem_tags: vec!["bot-evolve".to_owned()],
        outcome: Some(outcome),
        attempt_summary: clip_chars(&summary, sim::MAX_TEXT_CHARS),
        observed_outcome: observed,
        avoid_guidance: clip_chars(&avoid_guidance, sim::MAX_TEXT_CHARS),
        bot_id: Some(bot_id),
        model: sim_model_label(record),
        charter_revision: revision,
        task_id: task_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        tags: vec![tag.to_owned()],
        ..sim::LogAttemptInput::default()
    })
}

fn sim_model_label(record: &BotRecord) -> Option<String> {
    record.blueprint.preferred_model.clone().filter(|model| {
        model
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '/' | '@'))
    })
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ac-evolution-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn stores(&self) -> (MemoryStore, EvolutionStore) {
            let state = self.0.join("state");
            (
                MemoryStore::new(state.clone(), "ws-evo-01").unwrap(),
                EvolutionStore::new(state, "ws-evo-01").unwrap(),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_charter(instructions: &str) -> BotCharter {
        BotCharter {
            job: "Audit Rust pull requests for safety and style.".to_owned(),
            anti_jobs: vec![
                "Never modify repository files directly.".to_owned(),
                "Never merge or push branches.".to_owned(),
            ],
            voice_and_style: "Plain prose, concise actionable findings.".to_owned(),
            verification_bar: "Cite exact file paths and reproducible checks.".to_owned(),
            allowed_tools: vec!["read_file".to_owned(), "memory_search".to_owned()],
            instructions: instructions.to_owned(),
        }
    }

    fn forge_request(id: &str, pinned_core: bool) -> ForgeBotRequest {
        ForgeBotRequest {
            bot_id: id.to_owned(),
            role: "reviewer".to_owned(),
            work_class: WorkClass::Review,
            charter: sample_charter("Review diffs against .review-rules markdown files."),
            permission_mode: None,
            preferred_model: None,
            pinned_core,
            initial_memory_seeds: vec!["Check for unwrap() on external input.".to_owned()],
            probation_runs: Some(3),
            rationale: "Recurring Rust review gap.".to_owned(),
        }
    }

    #[test]
    fn sim_feedback_mapping_covers_promotion_rollback_and_decommission() {
        let scratch = Scratch::new("sim-mapping");
        let (memory, evolution) = scratch.stores();
        let now = 1_700_000_000;
        evolution
            .forge_bot(&memory, forge_request("mapper", false), now)
            .unwrap();
        let mut record = evolution.get_bot("mapper").unwrap().unwrap();
        let report = |action: HealthAction| FeedbackReport {
            bot_id: "mapper".to_owned(),
            lifecycle: LifecycleState::UnderReview,
            active_revision: 1,
            rolling_metrics: record_metrics(),
            lifetime_metrics: record_metrics(),
            action,
        };
        let promoted = sim_attempt_for_feedback(
            &record,
            &report(HealthAction::PromoteProbation { revision: 2 }),
            Some("T-1"),
            None,
        )
        .unwrap();
        assert_eq!(promoted.outcome, Some(sim::SimOutcome::Worked));
        assert_eq!(promoted.charter_revision, Some(2));
        assert_eq!(promoted.task_id.as_deref(), Some("T-1"));
        let rolled_back = sim_attempt_for_feedback(
            &record,
            &report(HealthAction::RollbackProbation {
                from_rev: 3,
                to_rev: 2,
                reason: "Two failures.".to_owned(),
            }),
            None,
            Some("Broke Windows paths"),
        )
        .unwrap();
        assert_eq!(rolled_back.outcome, Some(sim::SimOutcome::Reverted));
        assert!(rolled_back.avoid_guidance.contains("last known good v2"));
        assert_eq!(rolled_back.observed_outcome, "Broke Windows paths");
        let decommission = HealthAction::RecommendDecommission {
            reason: "Exhausted rounds.".to_owned(),
            requires_user_confirm: false,
        };
        assert!(
            sim_attempt_for_feedback(&record, &report(decommission.clone()), None, None).is_none()
        );
        record.blueprint.mutation_rounds_failed = 2;
        let regressed =
            sim_attempt_for_feedback(&record, &report(decommission), None, None).unwrap();
        assert_eq!(regressed.outcome, Some(sim::SimOutcome::Regressed));
        assert_eq!(regressed.charter_revision, None);
        assert!(!regressed.avoid_guidance.is_empty());
        assert!(
            sim_attempt_for_feedback(&record, &report(HealthAction::Healthy), None, None).is_none()
        );
    }

    fn record_metrics() -> QualityMetrics {
        QualityMetrics {
            wilson_lower_bound: 0.0,
            bayesian_mean: 0.5,
            positive_weight: 0.0,
            negative_weight: 0.0,
            sample_count: 0,
        }
    }

    #[test]
    fn dr_eggbot_charter_validation_enforces_anti_jobs_and_secret_safety() {
        let scratch = Scratch::new("charter");
        let (memory, evolution) = scratch.stores();
        let now = 1_700_000_000;

        let mut no_anti = forge_request("SloppyBot", false);
        no_anti.charter.anti_jobs.clear();
        assert_eq!(
            evolution
                .forge_bot(&memory, no_anti, now)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let mut leaky = forge_request("LeakyBot", false);
        leaky.charter.instructions =
            "Use token ghp_1234567890abcdef1234567890abcdef1234".to_owned();
        assert_eq!(
            evolution.forge_bot(&memory, leaky, now).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let forged = evolution
            .forge_bot(&memory, forge_request("Sentinel", false), now)
            .unwrap();
        assert_eq!(forged.blueprint.lifecycle, LifecycleState::Probation);
        assert_eq!(forged.blueprint.permission_mode, PermissionMode::ReadOnly);
        let seeds = memory
            .list_active(&MemoryScope::bot("sentinel").unwrap(), now)
            .unwrap();
        assert_eq!(seeds.len(), 1);
        let prompt = evolution
            .render_bot_charter_prompt("sentinel")
            .unwrap()
            .unwrap();
        assert!(prompt.contains("DO NOT: Never modify repository files directly."));
    }

    #[test]
    fn wilson_and_bayesian_scoring_favor_proven_volume_over_single_pass() {
        let one_of_one = wilson_lower_bound(1.0, 1.0, WILSON_Z_95);
        let nineteen_of_twenty = wilson_lower_bound(19.0, 20.0, WILSON_Z_95);
        assert!(nineteen_of_twenty > one_of_one);
        assert_eq!(wilson_lower_bound(0.0, 0.0, WILSON_Z_95), 0.0);
        assert_eq!(wilson_lower_bound(f64::NAN, 10.0, WILSON_Z_95), 0.0);
        assert!((bayesian_mean(0.0, 0.0, 2.0, 2.0) - 0.5).abs() < 1e-9);
        assert!(bayesian_mean(10.0, 0.0, 2.0, 2.0) > 0.80);
    }

    #[test]
    fn probation_promotes_on_pass_and_auto_rolls_back_on_mutation_regression() {
        let scratch = Scratch::new("probation-rollback");
        let (memory, evolution) = scratch.stores();
        let now = 1_700_000_000;

        evolution
            .forge_bot(&memory, forge_request("AuditOwl", false), now)
            .unwrap();

        for step in 1..=2 {
            let report = evolution
                .record_feedback(
                    "AuditOwl",
                    OutcomeSignal::FirstTryPass,
                    Some(&format!("t-{step}")),
                    Some(1_000 * step),
                    None,
                    now + (step as i64) * 10,
                )
                .unwrap();
            assert_eq!(report.lifecycle, LifecycleState::Probation);
            assert_eq!(report.action, HealthAction::Healthy);
        }

        let promoted = evolution
            .record_feedback(
                "AuditOwl",
                OutcomeSignal::FirstTryPass,
                Some("t-3"),
                Some(3_000),
                None,
                now + 30,
            )
            .unwrap();
        assert_eq!(promoted.lifecycle, LifecycleState::Active);
        assert_eq!(
            promoted.action,
            HealthAction::PromoteProbation { revision: 1 }
        );
        let audit = evolution.healthcheck_audit(now + 35).unwrap();
        assert!(audit.is_quiet());

        let v2 = evolution
            .propose_mutation(
                &memory,
                MutateBotRequest {
                    bot_id: "AuditOwl".to_owned(),
                    failure_mode: FailureMode::VerificationMissed,
                    task_summary: "Missed edge-case check".to_owned(),
                    root_cause: "Skipped Unicode boundary audit.".to_owned(),
                    patched_charter: sample_charter(
                        "Review diffs and explicitly verify Unicode boundaries.",
                    ),
                    diff_summary: vec!["+ Added mandatory Unicode boundary audit".to_owned()],
                    memory_correction: Some(
                        "Always check char vs byte slicing in Rust diffs.".to_owned(),
                    ),
                    probation_runs: Some(3),
                },
                now + 40,
            )
            .unwrap();
        assert_eq!(v2.version, 2);
        assert!(v2.parent_sha.is_some());

        let rolled_back = evolution
            .record_feedback(
                "AuditOwl",
                OutcomeSignal::UserDisapproved,
                Some("t-4"),
                Some(2_000),
                Some("Too noisy"),
                now + 50,
            )
            .unwrap();
        assert_eq!(rolled_back.active_revision, 1);
        assert_eq!(rolled_back.lifecycle, LifecycleState::UnderReview);
        assert!(matches!(
            rolled_back.action,
            HealthAction::RollbackProbation {
                from_rev: 2,
                to_rev: 1,
                ..
            }
        ));

        let v3 = evolution
            .propose_mutation(
                &memory,
                MutateBotRequest {
                    bot_id: "AuditOwl".to_owned(),
                    failure_mode: FailureMode::UserRejected,
                    task_summary: "Second mutation attempt".to_owned(),
                    root_cause: "Over-reported minor style nits.".to_owned(),
                    patched_charter: sample_charter("Report blocking defects only."),
                    diff_summary: vec!["~ Restrict findings to blocking defects".to_owned()],
                    memory_correction: None,
                    probation_runs: Some(3),
                },
                now + 60,
            )
            .unwrap();
        assert_eq!(v3.version, 3);

        let recommend_retire = evolution
            .record_feedback(
                "AuditOwl",
                OutcomeSignal::UserDisapproved,
                Some("t-5"),
                None,
                Some("Still missed real bug"),
                now + 70,
            )
            .unwrap();
        assert_eq!(recommend_retire.active_revision, 1);
        assert!(matches!(
            recommend_retire.action,
            HealthAction::RecommendDecommission {
                requires_user_confirm: false,
                ..
            }
        ));

        let summary = &evolution.scorecard_report(Some("AuditOwl")).unwrap()[0];
        assert_eq!(summary.median_duration_ms, Some(2_000));
        assert_eq!(summary.mutation_rounds_failed, 2);
    }

    #[test]
    fn capability_gap_threshold_and_pinned_core_decommission_gate_work() {
        let scratch = Scratch::new("gap-decommission");
        let (memory, evolution) = scratch.stores();
        let now = 1_700_000_000;

        assert!(evolution
            .record_capability_gap("sixel-rendering", "Draw usage rings", "graphics", now)
            .unwrap()
            .is_none());
        assert!(evolution
            .record_capability_gap("sixel-rendering", "Fix band height", "graphics", now + 60)
            .unwrap()
            .is_none());
        let gap = evolution
            .record_capability_gap("sixel-rendering", "Tune palette", "graphics", now + 120)
            .unwrap()
            .unwrap();
        assert_eq!(gap.occurrences, 3);
        assert!(gap.should_forge());

        let audit = evolution.healthcheck_audit(now + 150).unwrap();
        assert_eq!(audit.ready_gaps.len(), 1);

        evolution
            .forge_bot(&memory, forge_request("CoreGuard", true), now)
            .unwrap();
        let gate = evolution
            .decommission_bot(
                &memory,
                "CoreGuard",
                "Superseded by v2 reviewer",
                false,
                now + 200,
            )
            .unwrap();
        assert!(matches!(
            gate,
            DecommissionOutcome::NeedsConfirmation { .. }
        ));
        assert_eq!(evolution.active_bots().unwrap().len(), 1);

        let retired = evolution
            .decommission_bot(
                &memory,
                "CoreGuard",
                "Operator approved retirement",
                true,
                now + 210,
            )
            .unwrap();
        match retired {
            DecommissionOutcome::Decommissioned {
                bot_id,
                distilled_memory_ids,
                ledger_summary,
            } => {
                assert_eq!(bot_id, "coreguard");
                assert!(!distilled_memory_ids.is_empty());
                assert!(ledger_summary.contains("Decommissioned coreguard"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert!(evolution.active_bots().unwrap().is_empty());
        assert!(evolution
            .render_bot_charter_prompt("CoreGuard")
            .unwrap()
            .is_none());
        assert_eq!(
            evolution
                .get_bot("CoreGuard")
                .unwrap()
                .unwrap()
                .blueprint
                .lifecycle,
            LifecycleState::Decommissioned
        );

        let shared = memory.list_active(&MemoryScope::Shared, now + 220).unwrap();
        assert!(shared
            .iter()
            .any(|entry| entry.tags.iter().any(|tag| tag == "former:coreguard")));

        assert_eq!(
            evolution
                .record_feedback(
                    "CoreGuard",
                    OutcomeSignal::FirstTryPass,
                    None,
                    None,
                    None,
                    now + 230,
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn corrupt_evolution_registry_fails_closed() {
        let scratch = Scratch::new("corrupt");
        let (_, evolution) = scratch.stores();
        let path = evolution.registry_path().unwrap();
        fs::write(&path, b"{corrupt").unwrap();
        let err = evolution.active_bots().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("Corrupt evolution registry"));
    }

    #[test]
    fn same_task_user_disapproval_overrides_pass_and_rolling_window_recovers() {
        let scratch = Scratch::new("override-and-recover");
        let (memory, evolution) = scratch.stores();
        let now = 1_700_000_000;

        evolution
            .forge_bot(&memory, forge_request("BuilderBee", false), now)
            .unwrap();

        let pass = evolution
            .record_feedback(
                "BuilderBee",
                OutcomeSignal::FirstTryPass,
                Some("task-42"),
                Some(900),
                None,
                now + 10,
            )
            .unwrap();
        assert_eq!(pass.lifecycle, LifecycleState::Probation);

        let overridden = evolution
            .record_feedback(
                "BuilderBee",
                OutcomeSignal::UserDisapproved,
                Some("task-42"),
                None,
                Some("Missed requirement"),
                now + 20,
            )
            .unwrap();
        assert_eq!(overridden.lifecycle, LifecycleState::UnderReview);
        assert!(matches!(
            overridden.action,
            HealthAction::TriggerMutation { .. }
        ));
        let bot = evolution.get_bot("BuilderBee").unwrap().unwrap();
        assert_eq!(bot.scorecard.total_runs, 1);
        assert_eq!(bot.blueprint.revisions[0].probation_runs, 1);
        assert_eq!(bot.blueprint.revisions[0].probation_passes, 0);

        evolution
            .propose_mutation(
                &memory,
                MutateBotRequest {
                    bot_id: "BuilderBee".to_owned(),
                    failure_mode: FailureMode::WrongOutput,
                    task_summary: "Missed requirement on task-42".to_owned(),
                    root_cause: "Did not verify acceptance criteria checklist.".to_owned(),
                    patched_charter: sample_charter("Verify every requirement before finishing."),
                    diff_summary: vec!["+ Added requirement verification pass".to_owned()],
                    memory_correction: Some("Check every requirement line by line.".to_owned()),
                    probation_runs: Some(2),
                },
                now + 30,
            )
            .unwrap();

        for idx in 1..=10 {
            let _ = evolution
                .record_feedback(
                    "BuilderBee",
                    OutcomeSignal::UserApproved,
                    Some(&format!("recovered-{idx}")),
                    Some(800),
                    None,
                    now + 40 + (idx as i64) * 5,
                )
                .unwrap();
        }
        let summary = &evolution.scorecard_report(Some("BuilderBee")).unwrap()[0];
        assert_eq!(summary.lifecycle, LifecycleState::Active);
        assert_eq!(summary.active_revision, 2);
        assert_eq!(summary.mutation_rounds_failed, 0);
        assert!(summary.rolling_metrics.wilson_lower_bound > 0.75);
        assert_eq!(summary.action, HealthAction::Healthy);
    }
}
