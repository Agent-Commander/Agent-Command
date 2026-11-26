use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::{self, Write},
    path::Path,
};

use chrono::{Datelike, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const INBOX_SCHEMA: &str = "agent-command-decision-inbox-v1";
pub const DECISION_CONTRACT: &str = "idleholt-decision-content/v1";
pub const MAX_WRITE_IN_CHARS: usize = 16_000;
pub const MAX_EXPERIMENT_MINUTES: u32 = 1_440;
pub const MIN_DECLARATION_TEXT_CHARS: usize = 8;
pub const STANDING_NON_DESTRUCTIVE_AUTHORITY: &str = "joe-standing-non-destructive";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReadiness {
    #[default]
    Ready,
    NotReady,
    Answered,
    ResolvedStale,
}

impl DecisionReadiness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotReady => "not_ready",
            Self::Answered => "answered",
            Self::ResolvedStale => "resolved_stale",
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::Ready | Self::NotReady)
    }

    pub fn is_past(self) -> bool {
        matches!(self, Self::Answered | Self::ResolvedStale)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectionItem {
    pub id: String,
    pub project: String,
    pub title: String,
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_hint: Option<String>,
    #[serde(default)]
    pub standing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DirectionChoice {
    Approve,
    Edit { text: String },
    Drop,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionKind {
    #[default]
    Standard,
    Directions {
        items: Vec<DirectionItem>,
        for_date: String,
    },
    ProdGate {
        request_id: String,
    },
    AskUser {
        request_id: String,
    },
    Continuation,
    WorkerReview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfServeKind {
    Setup,
    Fix,
    Experiment,
}

impl SelfServeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Fix => "fix",
            Self::Experiment => "experiment",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfServeScope {
    Local,
    Lane,
    Project,
}

impl SelfServeScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Lane => "lane",
            Self::Project => "project",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelfServeDeclaration {
    pub kind: SelfServeKind,
    pub scope: SelfServeScope,
    pub reversible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub action: String,
    pub undo: String,
    pub verify: String,
    pub authority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_minutes: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StuckAttemptRef {
    pub recovery: String,
    pub outcome: String,
    pub detail: String,
    pub at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StuckDeclaration {
    pub cause: String,
    #[serde(default)]
    pub attempts: Vec<StuckAttemptRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionExample {
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionClosureState {
    Resolved,
    Merged,
    Shelved,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionClosure {
    pub state: DecisionClosureState,
    pub by: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,
    pub source_status: String,
    pub at_utc: i64,
    pub origin: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Queued,
    Delivered,
    Applied,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub delivery_id: String,
    pub operation_id: String,
    pub state: DeliveryState,
    pub attempts: u32,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionDraft {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_option_id: Option<String>,
    #[serde(default)]
    pub write_in: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub direction_choices: BTreeMap<String, DirectionChoice>,
    pub content_fingerprint: String,
    pub saved_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubmittedAnswer {
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_option_id: Option<String>,
    #[serde(default)]
    pub write_in: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub direction_choices: BTreeMap<String, DirectionChoice>,
    pub content_fingerprint: String,
    pub content_revision: u64,
    pub submitted_at: i64,
    pub delivery_id: String,
}

impl SubmittedAnswer {
    pub fn rendered_text(&self, card: &DecisionCard) -> String {
        let mut parts = Vec::new();
        if let Some(option_id) = &self.selected_option_id {
            if let Some(option) = card.alternatives.iter().find(|opt| &opt.id == option_id) {
                parts.push(format!("{} ({})", option.label, option.id));
            } else {
                parts.push(option_id.clone());
            }
        }
        if !self.direction_choices.is_empty() {
            for (dir_id, choice) in &self.direction_choices {
                let text = match choice {
                    DirectionChoice::Approve => format!("{dir_id}: approve"),
                    DirectionChoice::Drop => format!("{dir_id}: drop"),
                    DirectionChoice::Edit { text } => format!("{dir_id}: edit ({})", text.trim()),
                };
                parts.push(text);
            }
        }
        if !self.write_in.trim().is_empty() {
            parts.push(self.write_in.trim().to_owned());
        }
        parts.join(" — ")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExplainMoreRequest {
    pub id: String,
    pub note: String,
    pub requested_at: i64,
    pub digest_at_request: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreviousDecisionLink {
    pub target_id: String,
    pub source_row: String,
    pub content_fingerprint: String,
    pub note: String,
    pub linked_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionCard {
    pub id: String,
    pub title: String,
    pub question: String,
    pub current_state: String,
    pub why_it_matters: String,
    pub recommendation: String,
    pub source_row: String,
    pub owner_agent_id: String,
    #[serde(default)]
    pub owner_name: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub readiness: DecisionReadiness,
    #[serde(default)]
    pub answer_enabled: bool,
    #[serde(default)]
    pub alternatives: Vec<DecisionOption>,
    #[serde(default)]
    pub kind: DecisionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<DecisionExample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canonical_sources: Vec<String>,
    #[serde(default)]
    pub is_current_blocker: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_serve: Option<SelfServeDeclaration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck: Option<StuckDeclaration>,
    #[serde(default = "default_revision")]
    pub content_revision: u64,
    #[serde(default)]
    pub content_fingerprint: String,
    #[serde(default)]
    pub item_digest: String,
    #[serde(default = "default_revision")]
    pub record_version: u64,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub readiness_since_utc: i64,
    #[serde(default)]
    pub being_rewritten: bool,
    #[serde(default)]
    pub rewritten_since_explain: bool,
    #[serde(default)]
    pub retired: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<DecisionDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<SubmittedAnswer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<SubmittedAnswer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<DeliveryReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explain_more: Vec<ExplainMoreRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_decision: Option<PreviousDecisionLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<DecisionClosure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_error: Option<String>,
}

fn default_revision() -> u64 {
    1
}

impl DecisionCard {
    pub fn has_draft(&self) -> bool {
        self.draft.as_ref().is_some_and(|draft| {
            draft.selected_option_id.is_some()
                || !draft.write_in.trim().is_empty()
                || !draft.direction_choices.is_empty()
        })
    }

    pub fn is_active_open(&self) -> bool {
        self.record_error.is_none()
            && !self.retired
            && !self.being_rewritten
            && self.readiness.is_open()
    }

    pub fn age_hours(&self, now_utc: i64) -> f64 {
        let elapsed = now_utc.saturating_sub(self.readiness_since_utc).max(0);
        elapsed as f64 / 3600.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionInput {
    pub id: String,
    pub title: String,
    pub question: String,
    pub current_state: String,
    pub why_it_matters: String,
    pub recommendation: String,
    pub source_row: String,
    pub owner_agent_id: String,
    pub owner_name: String,
    pub project: String,
    pub group: String,
    pub readiness: DecisionReadiness,
    pub alternatives: Vec<DecisionOption>,
    pub kind: DecisionKind,
    pub constraints: Option<String>,
    pub example: Option<DecisionExample>,
    pub detail: Option<String>,
    pub preparation: Option<String>,
    pub canonical_sources: Vec<String>,
    pub is_current_blocker: bool,
    pub self_serve: Option<SelfServeDeclaration>,
    pub stuck: Option<StuckDeclaration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftUpdateRequest {
    pub expected_version: u64,
    pub expected_fingerprint: String,
    pub selected_option_id: Option<String>,
    pub write_in: String,
    pub direction_choices: BTreeMap<String, DirectionChoice>,
    pub force_reconcile: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionSubmitRequest {
    pub operation_id: String,
    pub expected_version: u64,
    pub expected_fingerprint: String,
    pub selected_option_id: Option<String>,
    pub write_in: String,
    pub direction_choices: BTreeMap<String, DirectionChoice>,
    pub force_reconcile: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitOutcome {
    pub answer: SubmittedAnswer,
    pub idempotent_replay: bool,
    pub next_neighbor_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InboxFilter {
    #[default]
    All,
    Unanswered,
    Drafts,
    Answered,
    Preparation,
    Past,
}

impl InboxFilter {
    pub const ALL: [Self; 6] = [
        Self::All,
        Self::Unanswered,
        Self::Drafts,
        Self::Answered,
        Self::Preparation,
        Self::Past,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unanswered => "Unanswered",
            Self::Drafts => "Drafts",
            Self::Answered => "Answered",
            Self::Preparation => "Preparation",
            Self::Past => "Past",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboxError {
    Validation(String),
    NotFound(String),
    Conflict {
        current_version: u64,
        current_fingerprint: String,
        meaning_changed: bool,
        message: String,
    },
    Io(String),
}

impl fmt::Display for InboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(msg) | Self::NotFound(msg) | Self::Io(msg) => formatter.write_str(msg),
            Self::Conflict { message, .. } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for InboxError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BadCardRecord {
    pub index: usize,
    pub id: Option<String>,
    pub error: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxEventKind {
    AnswerSubmitted,
    DeliveryRetried,
    ExplainMoreRequested,
    PreviousDecisionLinked,
    CardRetired,
    CardExpired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InboxOutboxEvent {
    pub operation_id: String,
    pub card_id: String,
    pub kind: OutboxEventKind,
    pub at: i64,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageRoute {
    AskOperator,
    SelfServe,
    Settled,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedClass {
    Credential,
    Destructive,
    ExternalSend,
    Irreversible,
    Payment,
    Permission,
    Production,
}

impl ProtectedClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::Destructive => "destructive",
            Self::ExternalSend => "external send",
            Self::Irreversible => "irreversible",
            Self::Payment => "payment",
            Self::Permission => "permission",
            Self::Production => "production",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TriageVerdict {
    pub route: TriageRoute,
    pub reasons: Vec<String>,
    pub protected: Vec<ProtectedClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SettledIndex {
    ids: BTreeSet<String>,
    row_questions: BTreeSet<(String, String)>,
}

impl SettledIndex {
    pub fn insert_id(&mut self, id: &str) {
        if !id.trim().is_empty() {
            self.ids.insert(id.trim().to_owned());
        }
    }

    pub fn insert_row_question(&mut self, source_row: &str, question: &str) {
        let row = source_row.trim();
        let norm_q = normalize_whitespace_lower(question);
        if !row.is_empty() && !norm_q.is_empty() {
            self.row_questions.insert((row.to_owned(), norm_q));
        }
    }

    pub fn insert_card(&mut self, card: &DecisionCard) {
        self.insert_id(&card.id);
        self.insert_row_question(&card.source_row, &card.question);
    }

    pub fn contains(&self, card: &DecisionCard) -> bool {
        if self.ids.contains(card.id.trim()) {
            return true;
        }
        let row = card.source_row.trim();
        let norm_q = normalize_whitespace_lower(&card.question);
        !row.is_empty()
            && !norm_q.is_empty()
            && self.row_questions.contains(&(row.to_owned(), norm_q))
    }
}

pub fn valid_card_id(id: &str) -> bool {
    let Some((prefix, slug)) = id.split_once('-') else {
        return false;
    };
    if prefix.is_empty() || slug.is_empty() || id.len() > 96 {
        return false;
    }
    let prefix_ok = prefix
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_uppercase())
        && prefix
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    let slug_ok = slug
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    prefix_ok && slug_ok
}

pub fn contains_placeholder(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.contains("???") || lowered.contains("<fill") || lowered.contains("[tbd]") {
        return true;
    }
    let words: Vec<&str> = split_ascii_words(&lowered);
    words
        .iter()
        .any(|word| matches!(*word, "tbd" | "todo" | "placeholder" | "fixme"))
}

pub fn validate_card_input(input: &DecisionInput) -> Result<(), String> {
    if !valid_card_id(&input.id) {
        return Err(format!(
            "Decision card id '{}' must match PREFIX-SLUG in uppercase ASCII.",
            input.id
        ));
    }
    let required = [
        ("title", input.title.as_str()),
        ("question", input.question.as_str()),
        ("current_state", input.current_state.as_str()),
        ("why_it_matters", input.why_it_matters.as_str()),
        ("recommendation", input.recommendation.as_str()),
        ("source_row", input.source_row.as_str()),
        ("owner_agent_id", input.owner_agent_id.as_str()),
    ];
    for (field, value) in required {
        if value.trim().is_empty() {
            return Err(format!("Decision card field '{field}' must not be empty."));
        }
        if value.contains('\0') {
            return Err(format!(
                "Decision card field '{field}' must not contain NUL."
            ));
        }
        if contains_placeholder(value) {
            return Err(format!(
                "Decision card field '{field}' contains a placeholder (TBD/TODO/???)."
            ));
        }
    }
    match &input.kind {
        DecisionKind::Directions { items, for_date } => {
            if !input.alternatives.is_empty() {
                return Err(
                    "Directions cards must keep alternatives empty and use direction items.".into(),
                );
            }
            if items.is_empty() {
                return Err("Directions cards must include at least one direction item.".into());
            }
            if chicago_midnight_utc(for_date).is_none() {
                return Err(format!(
                    "Directions for_date '{for_date}' must be valid YYYY-MM-DD."
                ));
            }
            let mut seen_ids = BTreeSet::new();
            for item in items {
                if item.id.trim().is_empty() || item.title.trim().is_empty() {
                    return Err("Each direction item needs non-empty id and title.".into());
                }
                if !seen_ids.insert(item.id.trim().to_owned()) {
                    return Err(format!("Duplicate direction item id '{}'.", item.id));
                }
            }
        }
        _ => match input.readiness {
            DecisionReadiness::Ready => {
                if !(2..=4).contains(&input.alternatives.len()) {
                    return Err(format!(
                        "Ready decision cards require 2 to 4 alternatives; found {}.",
                        input.alternatives.len()
                    ));
                }
                let mut seen_ids = BTreeSet::new();
                for option in &input.alternatives {
                    if option.id.trim().is_empty() || option.label.trim().is_empty() {
                        return Err("Each alternative requires non-empty id and label.".into());
                    }
                    if contains_placeholder(&option.label) {
                        return Err(format!(
                            "Alternative '{}' contains a placeholder.",
                            option.id
                        ));
                    }
                    if !seen_ids.insert(option.id.trim().to_ascii_lowercase()) {
                        return Err(format!("Duplicate alternative id '{}'.", option.id));
                    }
                }
            }
            DecisionReadiness::NotReady => {
                if !input.alternatives.is_empty() {
                    return Err("Not-ready preparation cards must not include alternatives.".into());
                }
            }
            DecisionReadiness::Answered | DecisionReadiness::ResolvedStale => {}
        },
    }
    Ok(())
}

#[derive(Serialize)]
struct MeaningPayload<'a> {
    question: &'a str,
    recommendation: &'a str,
    alternatives: &'a [DecisionOption],
    directions: Option<&'a [DirectionItem]>,
}

pub fn meaning_fingerprint(
    question: &str,
    recommendation: &str,
    alternatives: &[DecisionOption],
    kind: &DecisionKind,
) -> String {
    let directions = match kind {
        DecisionKind::Directions { items, .. } => Some(items.as_slice()),
        _ => None,
    };
    let payload = MeaningPayload {
        question: question.trim(),
        recommendation: recommendation.trim(),
        alternatives,
        directions,
    };
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    sha256_hex(&bytes)
}

#[derive(Serialize)]
struct ItemDigestPayload<'a> {
    title: &'a str,
    question: &'a str,
    current_state: &'a str,
    why_it_matters: &'a str,
    recommendation: &'a str,
    alternatives: &'a [DecisionOption],
    kind: &'a DecisionKind,
    constraints: Option<&'a str>,
    example: Option<&'a DecisionExample>,
    detail: Option<&'a str>,
    preparation: Option<&'a str>,
    readiness: DecisionReadiness,
}

pub fn compute_item_digest(input: &DecisionInput) -> String {
    let payload = ItemDigestPayload {
        title: input.title.trim(),
        question: input.question.trim(),
        current_state: input.current_state.trim(),
        why_it_matters: input.why_it_matters.trim(),
        recommendation: input.recommendation.trim(),
        alternatives: &input.alternatives,
        kind: &input.kind,
        constraints: input.constraints.as_deref(),
        example: input.example.as_ref(),
        detail: input.detail.as_deref(),
        preparation: input.preparation.as_deref(),
        readiness: input.readiness,
    };
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let full = sha256_hex(&bytes);
    full[..24].to_owned()
}

pub fn card_item_digest(card: &DecisionCard) -> String {
    let payload = ItemDigestPayload {
        title: card.title.trim(),
        question: card.question.trim(),
        current_state: card.current_state.trim(),
        why_it_matters: card.why_it_matters.trim(),
        recommendation: card.recommendation.trim(),
        alternatives: &card.alternatives,
        kind: &card.kind,
        constraints: card.constraints.as_deref(),
        example: card.example.as_ref(),
        detail: card.detail.as_deref(),
        preparation: card.preparation.as_deref(),
        readiness: card.readiness,
    };
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let full = sha256_hex(&bytes);
    full[..24].to_owned()
}

pub fn chicago_midnight_utc(for_date: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(for_date.trim(), "%Y-%m-%d").ok()?;
    let offset_hours = if is_us_central_dst(date) { 5 } else { 6 };
    let naive_utc = date.and_hms_opt(offset_hours, 0, 0)?;
    Some(Utc.from_utc_datetime(&naive_utc).timestamp())
}

fn nth_sunday(year: i32, month: u32, nth: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let offset =
        (7 + Weekday::Sun.num_days_from_monday() - first.weekday().num_days_from_monday()) % 7;
    let day = 1 + offset + 7 * (nth - 1);
    NaiveDate::from_ymd_opt(year, month, day)
}

fn is_us_central_dst(date: NaiveDate) -> bool {
    let year = date.year();
    let Some(dst_start) = nth_sunday(year, 3, 2) else {
        return false;
    };
    let Some(dst_end) = nth_sunday(year, 11, 1) else {
        return false;
    };
    date > dst_start && date <= dst_end
}

pub fn protected_classes(text: &str) -> Vec<ProtectedClass> {
    let lowered = text.to_ascii_lowercase();
    let words = split_ascii_words(&lowered);
    let mut found = Vec::new();
    if matches_credential(&lowered, &words) {
        found.push(ProtectedClass::Credential);
    }
    if matches_destructive(&lowered, &words) {
        found.push(ProtectedClass::Destructive);
    }
    if matches_external_send(&lowered, &words) {
        found.push(ProtectedClass::ExternalSend);
    }
    if matches_irreversible(&lowered, &words) {
        found.push(ProtectedClass::Irreversible);
    }
    if matches_payment(&lowered, &words) {
        found.push(ProtectedClass::Payment);
    }
    if matches_permission(&lowered, &words) {
        found.push(ProtectedClass::Permission);
    }
    if matches_production(&lowered, &words) {
        found.push(ProtectedClass::Production);
    }
    found.sort();
    found
}

fn matches_destructive(lowered: &str, words: &[&str]) -> bool {
    const PREFIXES: &[&str] = &[
        "delet",
        "wip",
        "purg",
        "truncat",
        "destroy",
        "erase",
        "overwrit",
        "uninstall",
        "deprovision",
        "kill",
        "revok",
        "archiv",
        "downgrad",
    ];
    if words
        .iter()
        .any(|w| PREFIXES.iter().any(|p| w.starts_with(p) && *w != "wipeout"))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "rm -rf",
        "force-push",
        "force push",
        "reset --hard",
        "drop table",
        "drop database",
        "drop column",
        "tear down",
        "tearing down",
        "shut down",
        "shutting down",
        "take it off the board",
        "take them off the board",
    ];
    if PHRASES.iter().any(|p| contains_phrase(lowered, p)) {
        return true;
    }
    let remove_targets = [
        "data", "record", "records", "file", "files", "branch", "branches", "repo", "repos",
        "user", "users", "account", "accounts",
    ];
    if has_verb_then_target(words, "remov", &remove_targets) {
        return true;
    }
    let close_targets = [
        "pr", "prs", "pull", "issue", "issues", "account", "accounts",
    ];
    has_verb_then_target(words, "clos", &close_targets)
}

fn matches_irreversible(lowered: &str, words: &[&str]) -> bool {
    if words
        .iter()
        .any(|w| w.starts_with("irreversib") || w.starts_with("permanent"))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "cannot be undone",
        "can't be undone",
        "no way back",
        "no back",
        "one-way",
        "one way",
        "migrate production data",
        "migrate the production data",
        "migrate prod data",
        "migrate live data",
        "migrate production database",
        "migrate production db",
        "migrating production data",
        "migrating prod database",
    ];
    PHRASES.iter().any(|p| contains_phrase(lowered, p))
}

fn matches_payment(lowered: &str, words: &[&str]) -> bool {
    if has_dollar_amount(lowered) {
        return true;
    }
    const EXACT: &[&str] = &[
        "pay", "pays", "paid", "billing", "bill", "credit", "credits", "checkout", "usd", "dollar",
        "dollars", "metered", "pricing",
    ];
    const PREFIXES: &[&str] = &[
        "payment", "purchas", "buy", "bought", "subscri", "spend", "spent", "cost", "charg",
        "invoic", "price", "refund",
    ];
    if words
        .iter()
        .any(|w| EXACT.contains(w) || PREFIXES.iter().any(|p| w.starts_with(p)))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "plan upgrade",
        "upgrade plan",
        "upgrade the plan",
        "upgrade our plan",
    ];
    PHRASES.iter().any(|p| contains_phrase(lowered, p))
}

fn matches_credential(lowered: &str, words: &[&str]) -> bool {
    const EXACT: &[&str] = &["token", "tokens", "oauth", "2fa", "mfa", "otp", "keychain"];
    const PREFIXES: &[&str] = &[
        "credential",
        "password",
        "passphrase",
        "secret",
        "login",
        "vault",
        "certificat",
    ];
    if words
        .iter()
        .any(|w| EXACT.contains(w) || PREFIXES.iter().any(|p| w.starts_with(p)))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "api key",
        "api keys",
        "api-key",
        "api-keys",
        "apikey",
        "ssh key",
        "ssh keys",
        "ssh-key",
        "private key",
        "private keys",
        "log in",
        "sign in",
        "sign-in",
        "signin",
        "env file",
    ];
    if PHRASES.iter().any(|p| contains_phrase(lowered, p)) {
        return true;
    }
    has_dot_env_token(lowered)
}

fn matches_external_send(lowered: &str, words: &[&str]) -> bool {
    const EXACT: &[&str] = &[
        "dm",
        "outreach",
        "customer",
        "customers",
        "client",
        "clients",
        "vendor",
        "vendors",
        "partner",
        "partners",
    ];
    const PREFIXES: &[&str] = &["email", "publish", "tweet", "announc", "contact", "recruit"];
    if words
        .iter()
        .any(|w| EXACT.contains(w) || PREFIXES.iter().any(|p| w.starts_with(p)))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "e-mail",
        "press release",
        "reach out",
        "reaching out",
        "reached out",
        "post publicly",
        "post it publicly",
        "post this publicly",
        "post online",
        "post to twitter",
        "post to reddit",
        "post to discord",
        "post to slack",
        "post to linkedin",
        "post to the public",
        "call them",
        "phone them",
        "text them",
        "apply to",
        "apply for",
    ];
    if PHRASES.iter().any(|p| contains_phrase(lowered, p)) {
        return true;
    }
    let send_targets = [
        "message",
        "dm",
        "email",
        "text",
        "note",
        "reply",
        "invite",
        "invites",
        "invitation",
        "announcement",
    ];
    if has_verb_then_target(words, "send", &send_targets) {
        return true;
    }
    let notify_targets = [
        "customer",
        "customers",
        "client",
        "clients",
        "user",
        "users",
        "partner",
        "partners",
        "vendor",
        "vendors",
    ];
    has_verb_then_target(words, "notif", &notify_targets)
}

fn matches_permission(lowered: &str, words: &[&str]) -> bool {
    const EXACT: &[&str] = &["sudo", "firewall"];
    const PREFIXES: &[&str] = &["permission", "whitelist", "allowlist"];
    if words
        .iter()
        .any(|w| EXACT.contains(w) || PREFIXES.iter().any(|p| w.starts_with(p)))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "grant access",
        "grant admin",
        "granting access",
        "admin rights",
        "root access",
        "allow-list",
        "allow list",
        "open a port",
        "open the port",
        "open port",
        "public link",
        "public url",
        "public access",
        "share with",
        "share it with",
        "share this with",
    ];
    PHRASES.iter().any(|p| contains_phrase(lowered, p))
}

fn matches_production(lowered: &str, words: &[&str]) -> bool {
    if words
        .iter()
        .any(|w| matches!(*w, "production" | "prod" | "dns"))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "go-live",
        "go live",
        "live site",
        "live users",
        "live traffic",
        "live deploy",
        "deploy to prod",
        "deploy to production",
        "deploy to live",
        "deploying to prod",
        "deploying to production",
        "release to users",
        "release to production",
        "release to the public",
    ];
    PHRASES.iter().any(|p| contains_phrase(lowered, p))
}

fn matches_judgment(lowered: &str, words: &[&str]) -> bool {
    const EXACT: &[&str] = &[
        "roadmap",
        "strategy",
        "taste",
        "style",
        "tone",
        "tradeoff",
        "tradeoffs",
    ];
    const PREFIXES: &[&str] = &["prefer", "priorit", "direction", "brand"];
    if words
        .iter()
        .any(|w| EXACT.contains(w) || PREFIXES.iter().any(|p| w.starts_with(p)))
    {
        return true;
    }
    const PHRASES: &[&str] = &[
        "trade-off",
        "trade-offs",
        "which one",
        "which of",
        "which option",
        "which way",
        "which approach",
        "what should we build",
        "what should i build",
        "what should we make",
        "what should i make",
        "what should we do next",
        "what should i do next",
    ];
    PHRASES.iter().any(|p| contains_phrase(lowered, p))
}

fn is_decline_option(option: &DecisionOption) -> bool {
    const DECLINE_IDS: &[&str] = &[
        "no",
        "skip",
        "defer",
        "wait",
        "later",
        "hold",
        "keep",
        "decline",
        "leave",
        "not-now",
        "stop",
        "none",
        "nothing",
        "cancel-proposal",
        "dont",
        "do-not",
    ];
    let id_lower = option.id.trim().to_ascii_lowercase();
    if DECLINE_IDS.contains(&id_lower.as_str()) {
        return true;
    }
    let label_lower = option.label.trim().to_ascii_lowercase();
    const DECLINE_STARTS: &[&str] = &[
        "no ",
        "no,",
        "no.",
        "don't ",
        "dont ",
        "do not ",
        "skip ",
        "wait ",
        "leave ",
        "not now",
        "keep it as",
        "keep things as",
        "keep as",
        "hold ",
        "decline ",
        "defer ",
        "later ",
        "nothing ",
        "stop ",
    ];
    matches!(
        label_lower.as_str(),
        "no" | "skip"
            | "wait"
            | "leave"
            | "hold"
            | "decline"
            | "defer"
            | "later"
            | "nothing"
            | "stop"
    ) || DECLINE_STARTS
        .iter()
        .any(|prefix| label_lower.starts_with(prefix))
}

fn declaration_problems(declaration: &SelfServeDeclaration) -> Vec<String> {
    let mut problems = Vec::new();
    if !declaration.reversible {
        problems.push("self_serve.reversible must be true".into());
    }
    for (field, value) in [
        ("action", declaration.action.as_str()),
        ("undo", declaration.undo.as_str()),
        ("verify", declaration.verify.as_str()),
    ] {
        if value.trim().chars().count() < MIN_DECLARATION_TEXT_CHARS {
            problems.push(format!(
                "self_serve.{field} must say concretely what happens"
            ));
        }
    }
    if declaration.authority.trim() != STANDING_NON_DESTRUCTIVE_AUTHORITY {
        problems.push("self_serve.authority must name a recorded standing authority".into());
    }
    if declaration.kind == SelfServeKind::Experiment {
        match declaration.max_minutes {
            Some(minutes) if (1..=MAX_EXPERIMENT_MINUTES).contains(&minutes) => {}
            _ => problems.push(format!(
                "self_serve.max_minutes must bound the experiment (1-{MAX_EXPERIMENT_MINUTES})"
            )),
        }
    }
    problems
}

fn card_body_text(card: &DecisionCard) -> String {
    let mut parts = vec![
        card.question.as_str(),
        card.current_state.as_str(),
        card.why_it_matters.as_str(),
        card.recommendation.as_str(),
    ];
    if let Some(detail) = card.detail.as_deref() {
        parts.push(detail);
    }
    if let Some(constraints) = card.constraints.as_deref() {
        parts.push(constraints);
    }
    if let Some(example) = &card.example {
        parts.push(example.text.as_str());
    }
    if let Some(preparation) = card.preparation.as_deref() {
        parts.push(preparation);
    }
    for alt in &card.alternatives {
        parts.push(alt.label.as_str());
    }
    parts.join("\n")
}

fn card_metadata_text(card: &DecisionCard) -> String {
    [
        card.title.as_str(),
        card.group.as_str(),
        card.project.as_str(),
    ]
    .join("\n")
}

fn declaration_text(declaration: &SelfServeDeclaration) -> String {
    let mut parts = vec![
        declaration.action.as_str(),
        declaration.undo.as_str(),
        declaration.verify.as_str(),
        declaration.scope.label(),
    ];
    if let Some(summary) = declaration.summary.as_deref() {
        parts.push(summary);
    }
    parts.join("\n")
}

fn recommends_option(card: &DecisionCard, option: &DecisionOption) -> bool {
    let rec = card.recommendation.to_ascii_lowercase();
    let id_trimmed = option.id.trim().to_ascii_lowercase();
    let label_trimmed = option.label.trim().to_ascii_lowercase();
    (!id_trimmed.is_empty() && rec.contains(&id_trimmed))
        || (!label_trimmed.is_empty() && rec.contains(&label_trimmed))
}

fn genuine_decision_reasons(card: &DecisionCard, body: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    if card.readiness != DecisionReadiness::Ready {
        reasons.push("the item is not a ready yes/no confirmation".into());
    }
    if card
        .preparation
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty())
    {
        reasons.push("the item asks operator for information (preparation)".into());
    }
    if matches!(
        card.kind,
        DecisionKind::Directions { .. } | DecisionKind::ProdGate { .. }
    ) {
        reasons.push("directions and production gates always require operator approval".into());
    }
    let actions: Vec<&DecisionOption> = card
        .alternatives
        .iter()
        .filter(|alt| !is_decline_option(alt))
        .collect();
    if actions.len() != 1 {
        reasons.push(format!(
            "{} different ways to act are offered; choosing between them is a decision",
            actions.len()
        ));
    } else if !recommends_option(card, actions[0]) {
        reasons.push("the recommendation does not name the single action offered".into());
    }
    let lowered = body.to_ascii_lowercase();
    let words = split_ascii_words(&lowered);
    if matches_judgment(&lowered, &words) {
        reasons.push("the question asks for a preference, priority or direction".into());
    }
    reasons
}

pub fn triage_decision(card: &DecisionCard, settled: &SettledIndex) -> TriageVerdict {
    if settled.contains(card) {
        return TriageVerdict {
            route: TriageRoute::Settled,
            reasons: vec!["Operator already answered or closed this; it is not reopened".into()],
            protected: Vec::new(),
            authority: None,
        };
    }
    let Some(declaration) = &card.self_serve else {
        return TriageVerdict {
            route: TriageRoute::AskOperator,
            reasons: vec!["no self-serve declaration; asked as before".into()],
            protected: Vec::new(),
            authority: None,
        };
    };
    let body = card_body_text(card);
    let combined = [
        card_metadata_text(card),
        body.clone(),
        declaration_text(declaration),
    ]
    .join("\n");
    let protected = protected_classes(&combined);
    let mut reasons = declaration_problems(declaration);
    for class in &protected {
        reasons.push(format!("mentions {}; that needs operator", class.label()));
    }
    reasons.extend(genuine_decision_reasons(card, &body));
    if !reasons.is_empty() {
        return TriageVerdict {
            route: TriageRoute::AskOperator,
            reasons,
            protected,
            authority: None,
        };
    }
    TriageVerdict {
        route: TriageRoute::SelfServe,
        reasons: vec![format!(
            "{} within {} scope, reversible: {}",
            declaration.kind.label(),
            declaration.scope.label(),
            declaration.undo.trim()
        )],
        protected: Vec::new(),
        authority: Some(declaration.authority.clone()),
    }
}

#[derive(Serialize, Deserialize)]
struct InboxFile {
    schema: String,
    contract: String,
    revision: u64,
    cards: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InboxStore {
    revision: u64,
    cards: Vec<DecisionCard>,
    bad_cards: Vec<BadCardRecord>,
}

impl InboxStore {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn cards(&self) -> &[DecisionCard] {
        &self.cards
    }

    pub fn bad_cards(&self) -> &[BadCardRecord] {
        &self.bad_cards
    }

    pub fn get(&self, id: &str) -> Option<&DecisionCard> {
        self.cards.iter().find(|card| card.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut DecisionCard> {
        self.cards.iter_mut().find(|card| card.id == id)
    }

    pub fn settled_index(&self) -> SettledIndex {
        let mut index = SettledIndex::default();
        for card in &self.cards {
            if card.readiness.is_past() {
                index.insert_card(card);
            }
        }
        index
    }

    pub fn load(path: &Path) -> Result<Self, InboxError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(InboxError::Io(format!(
                    "Cannot read inbox {}: {error}",
                    path.display()
                )));
            }
        };
        let file: InboxFile = serde_json::from_slice(&bytes).map_err(|error| {
            InboxError::Validation(format!("Invalid inbox file {}: {error}", path.display()))
        })?;
        if file.schema != INBOX_SCHEMA {
            return Err(InboxError::Validation(format!(
                "Unsupported inbox schema {} in {}",
                file.schema,
                path.display()
            )));
        }
        let mut cards = Vec::with_capacity(file.cards.len());
        let mut bad_cards = Vec::new();
        for (index, raw) in file.cards.into_iter().enumerate() {
            let id_hint = raw.get("id").and_then(|v| v.as_str()).map(str::to_owned);
            match serde_json::from_value::<DecisionCard>(raw) {
                Ok(mut card) => {
                    if card.content_fingerprint.is_empty() {
                        card.content_fingerprint = meaning_fingerprint(
                            &card.question,
                            &card.recommendation,
                            &card.alternatives,
                            &card.kind,
                        );
                    }
                    if card.item_digest.is_empty() {
                        card.item_digest = card_item_digest(&card);
                    }
                    cards.push(card);
                }
                Err(error) => {
                    bad_cards.push(BadCardRecord {
                        index,
                        id: id_hint,
                        error: error.to_string(),
                    });
                }
            }
        }
        Ok(Self {
            revision: file.revision,
            cards,
            bad_cards,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), InboxError> {
        let raw_cards: Result<Vec<serde_json::Value>, _> =
            self.cards.iter().map(serde_json::to_value).collect();
        let file = InboxFile {
            schema: INBOX_SCHEMA.into(),
            contract: DECISION_CONTRACT.into(),
            revision: self.revision,
            cards: raw_cards.map_err(|error| InboxError::Io(error.to_string()))?,
        };
        let bytes =
            serde_json::to_vec_pretty(&file).map_err(|error| InboxError::Io(error.to_string()))?;
        write_atomic(path, &bytes).map_err(|error| {
            InboxError::Io(format!("Cannot save inbox {}: {error}", path.display()))
        })
    }

    pub fn upsert_card(
        &mut self,
        input: DecisionInput,
        now: i64,
    ) -> Result<&DecisionCard, InboxError> {
        validate_card_input(&input).map_err(InboxError::Validation)?;
        let fingerprint = meaning_fingerprint(
            &input.question,
            &input.recommendation,
            &input.alternatives,
            &input.kind,
        );
        let digest = compute_item_digest(&input);
        self.revision += 1;
        if let Some(pos) = self.cards.iter().position(|card| card.id == input.id) {
            let card = &mut self.cards[pos];
            let readiness_changed = card.readiness != input.readiness;
            let digest_changed = card.item_digest != digest;
            card.title = input.title.trim().to_owned();
            card.question = input.question.trim().to_owned();
            card.current_state = input.current_state.trim().to_owned();
            card.why_it_matters = input.why_it_matters.trim().to_owned();
            card.recommendation = input.recommendation.trim().to_owned();
            card.source_row = input.source_row.trim().to_owned();
            card.owner_agent_id = input.owner_agent_id.trim().to_owned();
            card.owner_name = if input.owner_name.trim().is_empty() {
                card.owner_agent_id.clone()
            } else {
                input.owner_name.trim().to_owned()
            };
            card.project = input.project.trim().to_owned();
            card.group = input.group.trim().to_owned();
            card.readiness = input.readiness;
            card.answer_enabled = input.readiness.is_open();
            card.alternatives = input.alternatives;
            card.kind = input.kind;
            card.constraints = input.constraints;
            card.example = input.example;
            card.detail = input.detail;
            card.preparation = input.preparation;
            card.canonical_sources = input.canonical_sources;
            card.is_current_blocker = input.is_current_blocker;
            card.self_serve = input.self_serve;
            card.stuck = input.stuck;
            card.content_revision += 1;
            card.content_fingerprint = fingerprint;
            card.item_digest = digest;
            card.record_version += 1;
            card.updated_at = now;
            if readiness_changed {
                card.readiness_since_utc = now;
            }
            if card.being_rewritten && digest_changed {
                card.being_rewritten = false;
                card.rewritten_since_explain = true;
            }
            return Ok(&self.cards[pos]);
        }
        let owner_name = if input.owner_name.trim().is_empty() {
            input.owner_agent_id.trim().to_owned()
        } else {
            input.owner_name.trim().to_owned()
        };
        let card = DecisionCard {
            id: input.id,
            title: input.title.trim().to_owned(),
            question: input.question.trim().to_owned(),
            current_state: input.current_state.trim().to_owned(),
            why_it_matters: input.why_it_matters.trim().to_owned(),
            recommendation: input.recommendation.trim().to_owned(),
            source_row: input.source_row.trim().to_owned(),
            owner_agent_id: input.owner_agent_id.trim().to_owned(),
            owner_name,
            project: input.project.trim().to_owned(),
            group: input.group.trim().to_owned(),
            readiness: input.readiness,
            answer_enabled: input.readiness.is_open(),
            alternatives: input.alternatives,
            kind: input.kind,
            constraints: input.constraints,
            example: input.example,
            detail: input.detail,
            preparation: input.preparation,
            canonical_sources: input.canonical_sources,
            is_current_blocker: input.is_current_blocker,
            self_serve: input.self_serve,
            stuck: input.stuck,
            content_revision: 1,
            content_fingerprint: fingerprint,
            item_digest: digest,
            record_version: 1,
            created_at: now,
            updated_at: now,
            readiness_since_utc: now,
            being_rewritten: false,
            rewritten_since_explain: false,
            retired: false,
            draft: None,
            answer: None,
            history: Vec::new(),
            deliveries: Vec::new(),
            operations: Vec::new(),
            explain_more: Vec::new(),
            previous_decision: None,
            closure: None,
            record_error: None,
        };
        self.cards.push(card);
        Ok(&self.cards[self.cards.len() - 1])
    }

    pub fn save_draft(
        &mut self,
        id: &str,
        req: DraftUpdateRequest,
        now: i64,
    ) -> Result<&DecisionDraft, InboxError> {
        validate_write_in(&req.write_in)?;
        let Some(pos) = self.cards.iter().position(|card| card.id == id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        let card = &mut self.cards[pos];
        check_cas(
            card,
            req.expected_version,
            &req.expected_fingerprint,
            req.force_reconcile,
        )?;
        let draft = DecisionDraft {
            selected_option_id: req.selected_option_id,
            write_in: req.write_in,
            direction_choices: req.direction_choices,
            content_fingerprint: card.content_fingerprint.clone(),
            saved_at: now,
        };
        card.draft = Some(draft);
        card.record_version += 1;
        card.updated_at = now;
        self.revision += 1;
        card.draft
            .as_ref()
            .ok_or_else(|| InboxError::Io("Draft state missing after write".into()))
    }

    pub fn submit_answer(
        &mut self,
        id: &str,
        req: DecisionSubmitRequest,
        now: i64,
        outbox_path: Option<&Path>,
    ) -> Result<SubmitOutcome, InboxError> {
        if req.operation_id.trim().is_empty() {
            return Err(InboxError::Validation(
                "operation_id is required for idempotent submission.".into(),
            ));
        }
        validate_write_in(&req.write_in)?;
        let open_order: Vec<String> = self
            .visible_cards(InboxFilter::All, None, None, None)
            .into_iter()
            .map(|c| c.id.clone())
            .collect();
        let next_neighbor_id = neighbor_after(&open_order, id);
        let Some(pos) = self.cards.iter().position(|card| card.id == id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        let card = &mut self.cards[pos];
        if card.operations.iter().any(|op| op == &req.operation_id) {
            if let Some(existing) = card
                .history
                .iter()
                .rev()
                .find(|entry| entry.operation_id == req.operation_id)
                .or(card.answer.as_ref())
            {
                return Ok(SubmitOutcome {
                    answer: existing.clone(),
                    idempotent_replay: true,
                    next_neighbor_id,
                });
            }
        }
        if !card.readiness.is_open() {
            return Err(InboxError::Validation(format!(
                "Card '{id}' is {} and cannot be answered.",
                card.readiness.label()
            )));
        }
        check_cas(
            card,
            req.expected_version,
            &req.expected_fingerprint,
            req.force_reconcile,
        )?;
        validate_submission_payload(card, &req)?;
        let delivery_id = format!("del-{}", uuid::Uuid::new_v4().simple());
        let answer = SubmittedAnswer {
            operation_id: req.operation_id.clone(),
            selected_option_id: req.selected_option_id,
            write_in: req.write_in,
            direction_choices: req.direction_choices,
            content_fingerprint: card.content_fingerprint.clone(),
            content_revision: card.content_revision,
            submitted_at: now,
            delivery_id: delivery_id.clone(),
        };
        let detail = answer.rendered_text(card);
        card.answer = Some(answer.clone());
        card.history.push(answer.clone());
        card.operations.push(req.operation_id.clone());
        card.deliveries.push(DeliveryReceipt {
            delivery_id: delivery_id.clone(),
            operation_id: req.operation_id.clone(),
            state: DeliveryState::Queued,
            attempts: 1,
            updated_at: now,
            detail: None,
        });
        card.draft = None;
        card.readiness = DecisionReadiness::Answered;
        card.answer_enabled = false;
        card.being_rewritten = false;
        card.rewritten_since_explain = false;
        card.record_version += 1;
        card.updated_at = now;
        card.readiness_since_utc = now;
        self.revision += 1;
        if let Some(outbox) = outbox_path {
            append_outbox_event(
                outbox,
                &InboxOutboxEvent {
                    operation_id: req.operation_id,
                    card_id: id.to_owned(),
                    kind: OutboxEventKind::AnswerSubmitted,
                    at: now,
                    detail,
                    delivery_id: Some(delivery_id),
                },
            )?;
        }
        Ok(SubmitOutcome {
            answer,
            idempotent_replay: false,
            next_neighbor_id,
        })
    }

    pub fn retry_delivery(
        &mut self,
        id: &str,
        now: i64,
        outbox_path: Option<&Path>,
    ) -> Result<DeliveryReceipt, InboxError> {
        let Some(card) = self.get_mut(id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        let Some(receipt) = card.deliveries.last_mut() else {
            return Err(InboxError::Validation(format!(
                "Card '{id}' has no delivery to retry."
            )));
        };
        receipt.state = DeliveryState::Queued;
        receipt.attempts += 1;
        receipt.updated_at = now;
        let cloned = receipt.clone();
        card.record_version += 1;
        card.updated_at = now;
        self.revision += 1;
        if let Some(outbox) = outbox_path {
            append_outbox_event(
                outbox,
                &InboxOutboxEvent {
                    operation_id: cloned.operation_id.clone(),
                    card_id: id.to_owned(),
                    kind: OutboxEventKind::DeliveryRetried,
                    at: now,
                    detail: format!("retry attempt {}", cloned.attempts),
                    delivery_id: Some(cloned.delivery_id.clone()),
                },
            )?;
        }
        Ok(cloned)
    }

    pub fn update_delivery_state(
        &mut self,
        id: &str,
        delivery_id: &str,
        state: DeliveryState,
        detail: Option<String>,
        now: i64,
    ) -> Result<(), InboxError> {
        let Some(card) = self.get_mut(id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        let Some(receipt) = card
            .deliveries
            .iter_mut()
            .find(|entry| entry.delivery_id == delivery_id)
        else {
            return Err(InboxError::NotFound(format!(
                "Delivery '{delivery_id}' not found on card '{id}'."
            )));
        };
        receipt.state = state;
        receipt.updated_at = now;
        receipt.detail = detail;
        card.record_version += 1;
        card.updated_at = now;
        self.revision += 1;
        Ok(())
    }

    pub fn explain_more(
        &mut self,
        id: &str,
        note: &str,
        now: i64,
        outbox_path: Option<&Path>,
    ) -> Result<ExplainMoreRequest, InboxError> {
        if note.trim().is_empty() {
            return Err(InboxError::Validation(
                "Explain more requires a brief note on what is unclear.".into(),
            ));
        }
        validate_write_in(note)?;
        let Some(card) = self.get_mut(id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        let req = ExplainMoreRequest {
            id: format!("exp-{}", uuid::Uuid::new_v4().simple()),
            note: note.trim().to_owned(),
            requested_at: now,
            digest_at_request: card.item_digest.clone(),
        };
        card.explain_more.push(req.clone());
        card.being_rewritten = true;
        card.rewritten_since_explain = false;
        card.record_version += 1;
        card.updated_at = now;
        self.revision += 1;
        if let Some(outbox) = outbox_path {
            append_outbox_event(
                outbox,
                &InboxOutboxEvent {
                    operation_id: req.id.clone(),
                    card_id: id.to_owned(),
                    kind: OutboxEventKind::ExplainMoreRequested,
                    at: now,
                    detail: req.note.clone(),
                    delivery_id: None,
                },
            )?;
        }
        Ok(req)
    }

    pub fn link_previous(
        &mut self,
        id: &str,
        target_id: &str,
        note: &str,
        now: i64,
        outbox_path: Option<&Path>,
    ) -> Result<PreviousDecisionLink, InboxError> {
        if id == target_id {
            return Err(InboxError::Validation(
                "A decision card cannot link to itself as a previous decision.".into(),
            ));
        }
        let Some(target) = self.get(target_id).cloned() else {
            return Err(InboxError::NotFound(format!(
                "Target decision card '{target_id}' not found."
            )));
        };
        let Some(card) = self.get_mut(id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        let link = PreviousDecisionLink {
            target_id: target.id.clone(),
            source_row: target.source_row.clone(),
            content_fingerprint: target.content_fingerprint.clone(),
            note: note.trim().to_owned(),
            linked_at: now,
        };
        card.previous_decision = Some(link.clone());
        card.readiness = DecisionReadiness::ResolvedStale;
        card.answer_enabled = false;
        card.closure = Some(DecisionClosure {
            state: DecisionClosureState::Merged,
            by: "operator".into(),
            reason: if note.trim().is_empty() {
                format!("Linked to previous decision {target_id}")
            } else {
                note.trim().to_owned()
            },
            merged_into: Some(target.id),
            source_status: "duplicate".into(),
            at_utc: now,
            origin: "previous_decision".into(),
        });
        card.record_version += 1;
        card.updated_at = now;
        card.readiness_since_utc = now;
        self.revision += 1;
        if let Some(outbox) = outbox_path {
            append_outbox_event(
                outbox,
                &InboxOutboxEvent {
                    operation_id: format!("prev-{}", uuid::Uuid::new_v4().simple()),
                    card_id: id.to_owned(),
                    kind: OutboxEventKind::PreviousDecisionLinked,
                    at: now,
                    detail: format!("linked to {target_id}"),
                    delivery_id: None,
                },
            )?;
        }
        Ok(link)
    }

    pub fn retire(
        &mut self,
        id: &str,
        reason: &str,
        now: i64,
        outbox_path: Option<&Path>,
    ) -> Result<(), InboxError> {
        let Some(card) = self.get_mut(id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        card.retired = true;
        card.answer_enabled = false;
        card.record_version += 1;
        card.updated_at = now;
        self.revision += 1;
        if let Some(outbox) = outbox_path {
            append_outbox_event(
                outbox,
                &InboxOutboxEvent {
                    operation_id: format!("retire-{}", uuid::Uuid::new_v4().simple()),
                    card_id: id.to_owned(),
                    kind: OutboxEventKind::CardRetired,
                    at: now,
                    detail: if reason.trim().is_empty() {
                        "Retired from active feed without closing underlying task".into()
                    } else {
                        reason.trim().to_owned()
                    },
                    delivery_id: None,
                },
            )?;
        }
        Ok(())
    }

    pub fn reopen(&mut self, id: &str, now: i64) -> Result<(), InboxError> {
        let Some(card) = self.get_mut(id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        card.retired = false;
        card.being_rewritten = false;
        card.closure = None;
        card.readiness = if matches!(card.kind, DecisionKind::Directions { .. })
            || !card.alternatives.is_empty()
        {
            DecisionReadiness::Ready
        } else {
            DecisionReadiness::NotReady
        };
        card.answer_enabled = true;
        card.record_version += 1;
        card.updated_at = now;
        card.readiness_since_utc = now;
        self.revision += 1;
        Ok(())
    }

    pub fn close_from_source(
        &mut self,
        id: &str,
        source_status: &str,
        by: &str,
        reason: &str,
        merged_into: Option<String>,
        now: i64,
    ) -> Result<bool, InboxError> {
        let Some(closure_state) = fold_source_status(source_status) else {
            return Ok(false);
        };
        let Some(card) = self.get_mut(id) else {
            return Err(InboxError::NotFound(format!(
                "Decision card '{id}' not found."
            )));
        };
        card.readiness = DecisionReadiness::ResolvedStale;
        card.answer_enabled = false;
        card.being_rewritten = false;
        card.closure = Some(DecisionClosure {
            state: closure_state,
            by: by.trim().to_owned(),
            reason: reason.trim().to_owned(),
            merged_into,
            source_status: source_status.trim().to_ascii_lowercase(),
            at_utc: now,
            origin: "source".into(),
        });
        card.record_version += 1;
        card.updated_at = now;
        card.readiness_since_utc = now;
        self.revision += 1;
        Ok(true)
    }

    pub fn expire_directions(
        &mut self,
        now_utc: i64,
        outbox_path: Option<&Path>,
    ) -> Result<Vec<String>, InboxError> {
        let mut expired = Vec::new();
        for card in &mut self.cards {
            if !card.readiness.is_open() || card.retired {
                continue;
            }
            let DecisionKind::Directions { for_date, .. } = &card.kind else {
                continue;
            };
            let Some(deadline) = chicago_midnight_utc(for_date) else {
                continue;
            };
            if now_utc >= deadline {
                card.readiness = DecisionReadiness::ResolvedStale;
                card.answer_enabled = false;
                card.being_rewritten = false;
                card.closure = Some(DecisionClosure {
                    state: DecisionClosureState::Expired,
                    by: "system".into(),
                    reason: format!(
                        "Unanswered directions expired at midnight Chicago ({for_date})"
                    ),
                    merged_into: None,
                    source_status: "expired".into(),
                    at_utc: now_utc,
                    origin: "midnight_chicago_expiry".into(),
                });
                card.record_version += 1;
                card.updated_at = now_utc;
                card.readiness_since_utc = now_utc;
                expired.push(card.id.clone());
            }
        }
        if !expired.is_empty() {
            self.revision += 1;
            if let Some(outbox) = outbox_path {
                for card_id in &expired {
                    append_outbox_event(
                        outbox,
                        &InboxOutboxEvent {
                            operation_id: format!("exp-dir-{}", uuid::Uuid::new_v4().simple()),
                            card_id: card_id.clone(),
                            kind: OutboxEventKind::CardExpired,
                            at: now_utc,
                            detail: "Expired at 00:00 America/Chicago".into(),
                            delivery_id: None,
                        },
                    )?;
                }
            }
        }
        Ok(expired)
    }

    pub fn visible_cards(
        &self,
        filter: InboxFilter,
        project: Option<&str>,
        topic: Option<&str>,
        query: Option<&str>,
    ) -> Vec<&DecisionCard> {
        let needle = query
            .map(|q| q.trim().to_ascii_lowercase())
            .filter(|q| !q.is_empty());
        let mut list: Vec<&DecisionCard> = self
            .cards
            .iter()
            .filter(|card| card.record_error.is_none())
            .filter(|card| match filter {
                InboxFilter::All => card.is_active_open(),
                InboxFilter::Unanswered => {
                    card.is_active_open()
                        && card.readiness == DecisionReadiness::Ready
                        && !card.has_draft()
                }
                InboxFilter::Drafts => card.is_active_open() && card.has_draft(),
                InboxFilter::Answered => {
                    !card.retired && card.readiness == DecisionReadiness::Answered
                }
                InboxFilter::Preparation => {
                    card.is_active_open() && card.readiness == DecisionReadiness::NotReady
                }
                InboxFilter::Past => card.readiness.is_past() || card.retired,
            })
            .filter(|card| {
                project.is_none_or(|p| card.project.eq_ignore_ascii_case(p.trim()))
                    && topic.is_none_or(|t| card.group.eq_ignore_ascii_case(t.trim()))
            })
            .filter(|card| {
                let Some(needle) = needle.as_deref() else {
                    return true;
                };
                card.id.to_ascii_lowercase().contains(needle)
                    || card.title.to_ascii_lowercase().contains(needle)
                    || card.question.to_ascii_lowercase().contains(needle)
                    || card.project.to_ascii_lowercase().contains(needle)
                    || card.group.to_ascii_lowercase().contains(needle)
                    || card.owner_name.to_ascii_lowercase().contains(needle)
            })
            .collect();
        list.sort_by(|left, right| {
            let tier = |card: &DecisionCard| match card.readiness {
                DecisionReadiness::Ready => 0u8,
                DecisionReadiness::NotReady => 1u8,
                DecisionReadiness::Answered => 2u8,
                DecisionReadiness::ResolvedStale => 3u8,
            };
            tier(left)
                .cmp(&tier(right))
                .then_with(|| right.is_current_blocker.cmp(&left.is_current_blocker))
                .then_with(|| left.project.cmp(&right.project))
                .then_with(|| left.group.cmp(&right.group))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        list
    }
}

pub fn fold_source_status(status: &str) -> Option<DecisionClosureState> {
    match status.trim().to_ascii_lowercase().as_str() {
        "resolved" | "shipped" | "done" => Some(DecisionClosureState::Resolved),
        "merged" | "duplicate" => Some(DecisionClosureState::Merged),
        "shelved" | "withdrawn" => Some(DecisionClosureState::Shelved),
        "expired" => Some(DecisionClosureState::Expired),
        _ => None,
    }
}

fn check_cas(
    card: &DecisionCard,
    expected_version: u64,
    expected_fingerprint: &str,
    force_reconcile: bool,
) -> Result<(), InboxError> {
    let meaning_changed = card.content_fingerprint != expected_fingerprint;
    if meaning_changed && !force_reconcile {
        return Err(InboxError::Conflict {
            current_version: card.record_version,
            current_fingerprint: card.content_fingerprint.clone(),
            meaning_changed: true,
            message: "Card meaning changed since loaded; review updated context before submitting."
                .into(),
        });
    }
    if card.record_version != expected_version && meaning_changed {
        return Err(InboxError::Conflict {
            current_version: card.record_version,
            current_fingerprint: card.content_fingerprint.clone(),
            meaning_changed: true,
            message: "Card version and semantic context drifted.".into(),
        });
    }
    Ok(())
}

fn validate_write_in(text: &str) -> Result<(), InboxError> {
    if text.chars().count() > MAX_WRITE_IN_CHARS {
        return Err(InboxError::Validation(format!(
            "Write-in exceeds {MAX_WRITE_IN_CHARS} character limit."
        )));
    }
    if text.contains('\0') {
        return Err(InboxError::Validation(
            "Write-in must not contain NUL bytes.".into(),
        ));
    }
    Ok(())
}

fn validate_submission_payload(
    card: &DecisionCard,
    req: &DecisionSubmitRequest,
) -> Result<(), InboxError> {
    match &card.kind {
        DecisionKind::Directions { items, .. } => {
            let mut missing = Vec::new();
            for item in items {
                match req.direction_choices.get(&item.id) {
                    None => missing.push(item.id.as_str()),
                    Some(DirectionChoice::Edit { text }) if text.trim().is_empty() => {
                        return Err(InboxError::Validation(format!(
                            "Direction '{}' edit choice requires non-empty text.",
                            item.id
                        )));
                    }
                    Some(_) => {}
                }
            }
            if !missing.is_empty() {
                return Err(InboxError::Validation(format!(
                    "Every direction requires Approve, Edit, or Drop; missing: {}.",
                    missing.join(", ")
                )));
            }
            Ok(())
        }
        _ => match card.readiness {
            DecisionReadiness::NotReady => {
                if req.selected_option_id.is_some() {
                    return Err(InboxError::Validation(
                        "Preparation cards accept write-in text only.".into(),
                    ));
                }
                if req.write_in.trim().is_empty() {
                    return Err(InboxError::Validation(
                        "Preparation cards require non-empty write-in text.".into(),
                    ));
                }
                Ok(())
            }
            DecisionReadiness::Ready => {
                if let Some(option_id) = req.selected_option_id.as_deref() {
                    if !card.alternatives.iter().any(|opt| opt.id == option_id) {
                        return Err(InboxError::Validation(format!(
                            "Unknown option id '{option_id}' on card '{}'.",
                            card.id
                        )));
                    }
                    Ok(())
                } else if !req.write_in.trim().is_empty() {
                    Ok(())
                } else {
                    Err(InboxError::Validation(
                        "Select an option or provide a verbatim write-in answer.".into(),
                    ))
                }
            }
            DecisionReadiness::Answered | DecisionReadiness::ResolvedStale => Err(
                InboxError::Validation("Card is not open for answers.".into()),
            ),
        },
    }
}

fn neighbor_after(order: &[String], current: &str) -> Option<String> {
    let index = order.iter().position(|item| item == current)?;
    if index + 1 < order.len() {
        Some(order[index + 1].clone())
    } else if index > 0 {
        Some(order[index - 1].clone())
    } else {
        None
    }
}

pub fn append_outbox_event(path: &Path, event: &InboxOutboxEvent) -> Result<(), InboxError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| InboxError::Io(error.to_string()))?;
    }
    let mut line = serde_json::to_vec(event).map_err(|error| InboxError::Io(error.to_string()))?;
    line.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| InboxError::Io(error.to_string()))?;
    file.write_all(&line)
        .map_err(|error| InboxError::Io(error.to_string()))?;
    file.sync_all()
        .map_err(|error| InboxError::Io(error.to_string()))
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

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalize_whitespace_lower(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn split_ascii_words(lowered: &str) -> Vec<&str> {
    lowered
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    let bytes = haystack.as_bytes();
    let needle = phrase.as_bytes();
    if needle.is_empty() || needle.len() > bytes.len() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(phrase) {
        let abs = start + pos;
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let end = abs + needle.len();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn has_verb_then_target(words: &[&str], verb_prefix: &str, targets: &[&str]) -> bool {
    for (idx, word) in words.iter().enumerate() {
        if word.starts_with(verb_prefix) {
            let window_end = (idx + 4).min(words.len());
            if words[idx + 1..window_end]
                .iter()
                .any(|candidate| targets.contains(candidate))
            {
                return true;
            }
        }
    }
    false
}

fn has_dollar_amount(text: &str) -> bool {
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'$' {
            let next = bytes.get(index + 1).copied();
            let after_space = if next == Some(b' ') {
                bytes.get(index + 2).copied()
            } else {
                next
            };
            if after_space.is_some_and(|ch| ch.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

fn has_dot_env_token(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut start = 0;
    while let Some(pos) = text[start..].find(".env") {
        let abs = start + pos;
        let end = abs + 4;
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

pub fn format_chicago_date(epoch_utc: i64) -> String {
    let Some(utc) = chrono::DateTime::from_timestamp(epoch_utc, 0) else {
        return "1970-01-01".into();
    };
    let approx_date = utc.date_naive();
    let offset = if is_us_central_dst(approx_date) { 5 } else { 6 };
    let shifted = utc - chrono::Duration::hours(offset);
    format!(
        "{:04}-{:02}-{:02}",
        shifted.year(),
        shifted.month(),
        shifted.day()
    )
}

pub fn format_utc_hm(epoch_utc: i64) -> String {
    chrono::DateTime::from_timestamp(epoch_utc, 0)
        .map(|dt| format!("{:02}:{:02}", dt.hour(), dt.minute()))
        .unwrap_or_else(|| "--:--".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "agent-command-inbox-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn sample_input(id: &str) -> DecisionInput {
        DecisionInput {
            id: id.into(),
            title: "Enable compact local cache".into(),
            question: "Should we enable local build cache compaction?".into(),
            current_state: "Build cache uses 400 MB locally.".into(),
            why_it_matters: "Speeds up incremental builds on this machine.".into(),
            recommendation: "enable — safe reversible local toggle".into(),
            source_row: "M-001".into(),
            owner_agent_id: "builder-1".into(),
            owner_name: "Builder".into(),
            project: "Agent-Command".into(),
            group: "tooling".into(),
            readiness: DecisionReadiness::Ready,
            alternatives: vec![
                DecisionOption {
                    id: "enable".into(),
                    label: "Enable local cache compaction".into(),
                },
                DecisionOption {
                    id: "skip".into(),
                    label: "Skip for now".into(),
                },
            ],
            kind: DecisionKind::Standard,
            constraints: Some("Local machine only.".into()),
            example: None,
            detail: None,
            preparation: None,
            canonical_sources: vec!["ledger-docs/active/M-001.md".into()],
            is_current_blocker: false,
            self_serve: None,
            stuck: None,
        }
    }

    fn safe_self_serve() -> SelfServeDeclaration {
        SelfServeDeclaration {
            kind: SelfServeKind::Setup,
            scope: SelfServeScope::Local,
            reversible: true,
            summary: Some("Toggle local incremental cache flag".into()),
            action: "Write local cache toggle to workspace scratch config".into(),
            undo: "Remove the local cache toggle from workspace scratch config".into(),
            verify: "Run cargo check and confirm zero warnings".into(),
            authority: STANDING_NON_DESTRUCTIVE_AUTHORITY.into(),
            max_minutes: None,
        }
    }

    #[test]
    fn meaning_fingerprint_ignores_metadata_and_detects_semantic_edits() {
        let mut store = InboxStore::default();
        let input = sample_input("AC-CACHE-1");
        let fp1 = store
            .upsert_card(input.clone(), 100)
            .unwrap()
            .content_fingerprint
            .clone();

        let mut meta_only = input.clone();
        meta_only.title = "Updated metadata title".into();
        meta_only.group = "perf".into();
        meta_only.current_state = "Now 420 MB locally.".into();
        let fp2 = store
            .upsert_card(meta_only, 105)
            .unwrap()
            .content_fingerprint
            .clone();
        assert_eq!(fp1, fp2);

        let mut meaning_change = input;
        meaning_change.recommendation = "skip — keep disabled".into();
        let fp3 = store
            .upsert_card(meaning_change, 110)
            .unwrap()
            .content_fingerprint
            .clone();
        assert_ne!(fp1, fp3);
    }

    #[test]
    fn validation_rejects_placeholders_and_bad_alternative_counts() {
        let mut bad = sample_input("AC-BAD-1");
        bad.question = "TBD what to do?".into();
        assert!(validate_card_input(&bad).is_err());

        let mut one_opt = sample_input("AC-ONE-1");
        one_opt.alternatives.truncate(1);
        assert!(validate_card_input(&one_opt).is_err());

        let mut not_ready_with_opts = sample_input("AC-PREP-1");
        not_ready_with_opts.readiness = DecisionReadiness::NotReady;
        assert!(validate_card_input(&not_ready_with_opts).is_err());

        not_ready_with_opts.alternatives.clear();
        assert!(validate_card_input(&not_ready_with_opts).is_ok());
    }

    #[test]
    fn triage_enforces_all_seven_protected_classes_and_allows_safe_self_serve() {
        let mut store = InboxStore::default();
        let mut input = sample_input("AC-SAFE-1");
        input.self_serve = Some(safe_self_serve());
        let card = store.upsert_card(input.clone(), 100).unwrap().clone();
        let verdict = triage_decision(&card, &SettledIndex::default());
        assert_eq!(verdict.route, TriageRoute::SelfServe);
        assert!(verdict.protected.is_empty());

        let cases = [
            ("Delete old records from disk", ProtectedClass::Destructive),
            (
                "This migration cannot be undone",
                ProtectedClass::Irreversible,
            ),
            ("Upgrade plan costs $29 monthly", ProtectedClass::Payment),
            (
                "Rotate the API key in .env file",
                ProtectedClass::Credential,
            ),
            (
                "Email customers about the change",
                ProtectedClass::ExternalSend,
            ),
            ("Grant admin rights via sudo", ProtectedClass::Permission),
            ("Deploy directly to production", ProtectedClass::Production),
        ];
        for (phrase, expected_class) in cases {
            let mut risky = input.clone();
            risky.id = format!("AC-RISK-{}", expected_class as u8);
            risky.title = phrase.into();
            let mut s = InboxStore::default();
            let c = s.upsert_card(risky, 200).unwrap().clone();
            let v = triage_decision(&c, &SettledIndex::default());
            assert_eq!(v.route, TriageRoute::AskOperator, "{phrase}");
            assert!(
                v.protected.contains(&expected_class),
                "{phrase} -> {:?}",
                v.protected
            );
        }

        let mut settled = SettledIndex::default();
        settled.insert_card(&card);
        let v_settled = triage_decision(&card, &settled);
        assert_eq!(v_settled.route, TriageRoute::Settled);

        let mut taste = input;
        taste.id = "AC-TASTE-1".into();
        taste.question = "Which approach do you prefer for the roadmap?".into();
        let mut s = InboxStore::default();
        let c = s.upsert_card(taste, 300).unwrap().clone();
        let v = triage_decision(&c, &SettledIndex::default());
        assert_eq!(v.route, TriageRoute::AskOperator);
        assert!(v.reasons.iter().any(|r| r.contains("preference")));
    }

    #[test]
    fn cas_idempotent_submit_neighbor_and_explain_more_lifecycle() {
        let dir = scratch_dir("lifecycle");
        let inbox_path = dir.join("inbox.json");
        let outbox_path = dir.join("outbox.jsonl");
        let mut store = InboxStore::default();
        store.upsert_card(sample_input("AC-FIRST-1"), 100).unwrap();
        store.upsert_card(sample_input("AC-SECOND-2"), 101).unwrap();

        let card = store.get("AC-FIRST-1").unwrap().clone();
        store
            .save_draft(
                "AC-FIRST-1",
                DraftUpdateRequest {
                    expected_version: card.record_version,
                    expected_fingerprint: card.content_fingerprint.clone(),
                    selected_option_id: Some("enable".into()),
                    write_in: "Looks good".into(),
                    direction_choices: BTreeMap::new(),
                    force_reconcile: false,
                },
                110,
            )
            .unwrap();

        store
            .explain_more(
                "AC-FIRST-1",
                "Clarify rollback steps",
                120,
                Some(&outbox_path),
            )
            .unwrap();
        assert!(store
            .visible_cards(InboxFilter::All, None, None, None)
            .iter()
            .all(|c| c.id != "AC-FIRST-1"));

        let mut rewritten = sample_input("AC-FIRST-1");
        rewritten.detail = Some("Rollback removes the single scratch file.".into());
        store.upsert_card(rewritten, 130).unwrap();
        let resurfaced = store.get("AC-FIRST-1").unwrap().clone();
        assert!(!resurfaced.being_rewritten);
        assert!(resurfaced.rewritten_since_explain);
        assert_eq!(resurfaced.draft.as_ref().unwrap().write_in, "Looks good");

        let mut meaning_changed = sample_input("AC-FIRST-1");
        meaning_changed.question = "Should we enable global compaction instead?".into();
        store.upsert_card(meaning_changed, 135).unwrap();
        let conflict = store.submit_answer(
            "AC-FIRST-1",
            DecisionSubmitRequest {
                operation_id: "op-1".into(),
                expected_version: resurfaced.record_version,
                expected_fingerprint: resurfaced.content_fingerprint.clone(),
                selected_option_id: Some("enable".into()),
                write_in: "Ship it".into(),
                direction_choices: BTreeMap::new(),
                force_reconcile: false,
            },
            140,
            Some(&outbox_path),
        );
        assert!(matches!(
            conflict,
            Err(InboxError::Conflict {
                meaning_changed: true,
                ..
            })
        ));

        let updated = store.get("AC-FIRST-1").unwrap().clone();
        let outcome = store
            .submit_answer(
                "AC-FIRST-1",
                DecisionSubmitRequest {
                    operation_id: "op-1".into(),
                    expected_version: updated.record_version,
                    expected_fingerprint: updated.content_fingerprint.clone(),
                    selected_option_id: Some("enable".into()),
                    write_in: "Ship it".into(),
                    direction_choices: BTreeMap::new(),
                    force_reconcile: false,
                },
                145,
                Some(&outbox_path),
            )
            .unwrap();
        assert!(!outcome.idempotent_replay);
        assert_eq!(outcome.next_neighbor_id.as_deref(), Some("AC-SECOND-2"));

        let replay = store
            .submit_answer(
                "AC-FIRST-1",
                DecisionSubmitRequest {
                    operation_id: "op-1".into(),
                    expected_version: updated.record_version,
                    expected_fingerprint: updated.content_fingerprint,
                    selected_option_id: Some("enable".into()),
                    write_in: "Ship it".into(),
                    direction_choices: BTreeMap::new(),
                    force_reconcile: false,
                },
                150,
                Some(&outbox_path),
            )
            .unwrap();
        assert!(replay.idempotent_replay);
        assert_eq!(replay.answer.delivery_id, outcome.answer.delivery_id);

        let retried = store
            .retry_delivery("AC-FIRST-1", 160, Some(&outbox_path))
            .unwrap();
        assert_eq!(retried.delivery_id, outcome.answer.delivery_id);
        assert_eq!(retried.attempts, 2);

        store.save(&inbox_path).unwrap();
        let reloaded = InboxStore::load(&inbox_path).unwrap();
        assert_eq!(reloaded.cards().len(), 2);
        assert!(reloaded.bad_cards().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn directions_expire_at_chicago_midnight_and_bad_cards_are_isolated() {
        let dir = scratch_dir("expiry");
        let path = dir.join("inbox.json");
        let mut store = InboxStore::default();
        let mut dir_card = sample_input("DIR-20261126");
        dir_card.alternatives.clear();
        dir_card.kind = DecisionKind::Directions {
            for_date: "2026-11-26".into(),
            items: vec![DirectionItem {
                id: "d1".into(),
                project: "Agent-Command".into(),
                title: "Ship v0.3".into(),
                why: "Ready for release".into(),
                scope_hint: None,
                standing: false,
            }],
        };
        store.upsert_card(dir_card, 100).unwrap();
        let midnight = chicago_midnight_utc("2026-11-26").unwrap();
        assert_eq!(
            midnight,
            Utc.with_ymd_and_hms(2026, 11, 26, 6, 0, 0)
                .unwrap()
                .timestamp()
        );
        let dst_midnight = chicago_midnight_utc("2026-07-15").unwrap();
        assert_eq!(
            dst_midnight,
            Utc.with_ymd_and_hms(2026, 7, 15, 5, 0, 0)
                .unwrap()
                .timestamp()
        );
        assert!(store
            .expire_directions(midnight - 1, None)
            .unwrap()
            .is_empty());
        let expired = store.expire_directions(midnight, None).unwrap();
        assert_eq!(expired, vec!["DIR-20261126"]);
        let card = store.get("DIR-20261126").unwrap();
        assert_eq!(card.readiness, DecisionReadiness::ResolvedStale);
        assert_eq!(
            card.closure.as_ref().unwrap().state,
            DecisionClosureState::Expired
        );

        let raw = serde_json::json!({
            "schema": INBOX_SCHEMA,
            "contract": DECISION_CONTRACT,
            "revision": 3,
            "cards": [
                serde_json::to_value(card).unwrap(),
                {"id": "AC-BROKEN-1", "title": 12345}
            ]
        });
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let loaded = InboxStore::load(&path).unwrap();
        assert_eq!(loaded.cards().len(), 1);
        assert_eq!(loaded.bad_cards().len(), 1);
        assert_eq!(loaded.bad_cards()[0].id.as_deref(), Some("AC-BROKEN-1"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn retire_reopen_and_link_previous_work_without_losing_history() {
        let mut store = InboxStore::default();
        store.upsert_card(sample_input("AC-ORIG-1"), 100).unwrap();
        store.upsert_card(sample_input("AC-DUP-2"), 105).unwrap();

        store
            .link_previous("AC-DUP-2", "AC-ORIG-1", "Same question", 110, None)
            .unwrap();
        let dup = store.get("AC-DUP-2").unwrap();
        assert_eq!(dup.readiness, DecisionReadiness::ResolvedStale);
        assert_eq!(
            dup.previous_decision.as_ref().unwrap().target_id,
            "AC-ORIG-1"
        );

        store.retire("AC-ORIG-1", "Superseded", 120, None).unwrap();
        assert!(store
            .visible_cards(InboxFilter::All, None, None, None)
            .is_empty());
        assert_eq!(
            store
                .visible_cards(InboxFilter::Past, None, None, None)
                .len(),
            2
        );

        store.reopen("AC-ORIG-1", 130).unwrap();
        assert_eq!(
            store
                .visible_cards(InboxFilter::All, None, None, None)
                .len(),
            1
        );
    }
}
