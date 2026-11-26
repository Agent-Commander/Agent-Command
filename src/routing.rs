use crate::{
    auth::{self, Profile, Provider},
    catalog::{
        BenchmarkComponent, Catalog, ComponentCatalog, Metric, Metrics, SpeedUnit, KEY_VARIABLE,
    },
    native::{self, Benchmark, Pin, Pins, DEFAULT_SETTING},
    usage::{Tick, Usage},
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub(crate) const STATE_DIRECTORY: &str = "agent-command-routing-v1";
const OVERRIDE_FILE: &str = "override.json";
const SPLIT_DELIMITER: &str = ";;";
const MAX_SPLIT_UNITS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Model {
    Default,
    Alias(&'static str),
    Pinned(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Configuration {
    pub provider: Provider,
    pub model: Model,
    pub effort: Option<String>,
}

const CLAUDE_DISPATCH_ALIASES: [&str; 2] = ["opus", "sonnet"];

pub(crate) fn configurations(provider: Provider, pin: Option<&Pin>) -> Vec<Configuration> {
    let mut configurations = vec![Configuration::provider_default(provider)];
    if provider == Provider::Claude {
        configurations.extend(CLAUDE_DISPATCH_ALIASES.map(|alias| Configuration {
            provider,
            model: Model::Alias(alias),
            effort: None,
        }));
    }
    configurations.extend(
        pin.filter(|pin| pin.provider() == Some(provider) && pin.validate().is_ok())
            .map(|pin| Configuration {
                provider,
                model: Model::Pinned(pin.model.clone()),
                effort: pin.effort.clone(),
            }),
    );
    configurations
}

impl Configuration {
    pub(crate) fn provider_default(provider: Provider) -> Self {
        Self {
            provider,
            model: Model::Default,
            effort: None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn is_default(&self) -> bool {
        self.model == Model::Default && self.effort.is_none()
    }

    pub(crate) fn native_model(&self) -> Option<&str> {
        match &self.model {
            Model::Default => None,
            Model::Alias(alias) => Some(alias),
            Model::Pinned(model) => Some(model),
        }
    }

    pub(crate) fn benchmark(&self) -> Option<Benchmark> {
        match &self.model {
            Model::Pinned(model) => {
                native::benchmark(self.provider, Some(model), self.effort.as_deref())
            }
            Model::Default | Model::Alias(_) => None,
        }
    }

    pub(crate) fn arguments(&self) -> Vec<String> {
        let toml_value = |value: &str| {
            if value.contains(['\'', '\n', '\r']) {
                format!("{value:?}")
            } else {
                format!("'{value}'")
            }
        };
        let mut arguments = Vec::new();
        match self.provider {
            Provider::Claude | Provider::Gemini => {
                if let Some(model) = self.native_model() {
                    arguments.extend(["--model".to_owned(), model.to_owned()]);
                }
                if let Some(effort) = &self.effort {
                    arguments.extend(["--effort".to_owned(), effort.clone()]);
                }
            }
            Provider::Codex => {
                if let Some(model) = self.native_model() {
                    arguments.extend(["-c".to_owned(), format!("model={}", toml_value(model))]);
                }
                if let Some(effort) = &self.effort {
                    arguments.extend([
                        "-c".to_owned(),
                        format!("model_reasoning_effort={}", toml_value(effort)),
                    ]);
                }
            }
        }
        arguments
    }

    pub(crate) fn label(&self) -> String {
        match (&self.model, &self.effort) {
            (Model::Default, _) => "provider default".into(),
            (Model::Alias(alias), _) => format!("{alias} alias"),
            (Model::Pinned(model), Some(effort)) => format!("{model} {effort}"),
            (Model::Pinned(model), None) => format!("{model} {DEFAULT_SETTING}"),
        }
    }

    fn order(&self) -> (u8, &str, &str) {
        let kind = match self.model {
            Model::Default => 0,
            Model::Alias(_) => 1,
            Model::Pinned(_) => 2,
        };
        (
            kind,
            self.native_model().unwrap_or_default(),
            self.effort.as_deref().unwrap_or_default(),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Policy {
    #[default]
    Default,
    Ordinary,
    Urgent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Difficulty {
    #[default]
    Any,
    Standard,
    Hard,
}

pub(crate) const STANDARD_FLOOR: f64 = 40.0;
pub(crate) const HARD_FLOOR: f64 = 55.0;

impl Difficulty {
    pub(crate) fn floor(self) -> Option<f64> {
        match self {
            Self::Any => None,
            Self::Standard => Some(STANDARD_FLOOR),
            Self::Hard => Some(HARD_FLOOR),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Standard => "standard",
            Self::Hard => "hard",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Request {
    pub(crate) policy: Policy,
    pub(crate) difficulty: Difficulty,
    pub(crate) units: Vec<String>,
    pub(crate) split: bool,
}

pub(crate) fn parse(input: &str) -> Result<Request, String> {
    let mut request = Request::default();
    let mut rest = input.trim();
    while let Some((directive, remainder)) = leading_directive(rest) {
        match directive {
            "/urgent" => request.policy = Policy::Urgent,
            "/ordinary" => request.policy = Policy::Ordinary,
            "/any" => request.difficulty = Difficulty::Any,
            "/standard" => request.difficulty = Difficulty::Standard,
            "/hard" => request.difficulty = Difficulty::Hard,
            "/split" => request.split = true,
            _ => break,
        }
        rest = remainder;
    }
    request.units = if request.split {
        split_units(rest)?
    } else {
        vec![rest.to_owned()]
    };
    if request.units.iter().any(String::is_empty) {
        return Err("Enter a task after the routing directives.".into());
    }
    Ok(request)
}

fn leading_directive(text: &str) -> Option<(&str, &str)> {
    if !text.starts_with('/') {
        return None;
    }
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    Some((word, rest.trim_start()))
}

fn split_units(text: &str) -> Result<Vec<String>, String> {
    let units: Vec<String> = text
        .split(SPLIT_DELIMITER)
        .map(|unit| unit.trim().to_owned())
        .collect();
    if units.len() < 2 || units.len() > MAX_SPLIT_UNITS || units.iter().any(String::is_empty) {
        return Err(format!(
            "/split needs 2–{MAX_SPLIT_UNITS} non-empty units separated by {SPLIT_DELIMITER}"
        ));
    }
    Ok(units)
}

pub(crate) fn state_directory(root: &Path) -> io::Result<PathBuf> {
    auth::private_state_directory(root, STATE_DIRECTORY)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OverrideRecord {
    profile: String,
}

pub(crate) fn load_override(state: &Path) -> io::Result<Option<String>> {
    match fs::read(state.join(OVERRIDE_FILE)) {
        Ok(bytes) => {
            let record: OverrideRecord =
                serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            Ok(Some(record.profile))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn save_override(state: &Path, profile: Option<&str>) -> io::Result<()> {
    let path = state.join(OVERRIDE_FILE);
    let Some(profile) = profile else {
        return match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    };
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let mut file = fs::File::create_new(&temporary)?;
    serde_json::to_writer(
        &mut file,
        &OverrideRecord {
            profile: profile.into(),
        },
    )?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Headroom {
    pub(crate) rate: Option<f64>,
    pub(crate) remaining: Option<f64>,
    pub(crate) low_buffer: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub(crate) profile: usize,
    pub(crate) label: String,
    pub(crate) configuration: Configuration,
    pub(crate) metrics: Metrics,
    pub(crate) metric: Option<Metric>,
    pub(crate) headroom: Headroom,
}

impl Candidate {
    pub(crate) fn score(&self) -> Option<f64> {
        self.metric.and_then(|metric| self.metrics.value(metric))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Exclusion {
    pub(crate) label: String,
    pub(crate) reason: &'static str,
}

pub(crate) fn allowance_refusal(usage: &io::Result<Usage>, now: Tick) -> Option<&'static str> {
    match usage {
        Err(_) => Some("usage state unreadable"),
        Ok(usage) if usage.blocked(now) => Some("latched"),
        Ok(usage) if !usage.eligible_alternate(now) => Some("usage window exhausted"),
        Ok(_) => None,
    }
}

pub(crate) fn candidates(
    profiles: &[Profile],
    catalog: &Catalog,
    pins: &Pins,
    now: Tick,
) -> (Vec<Candidate>, Vec<Exclusion>) {
    candidates_with_components(profiles, catalog, None, pins, now)
}

pub(crate) fn candidates_with_components(
    profiles: &[Profile],
    catalog: &Catalog,
    components: Option<&ComponentCatalog>,
    pins: &Pins,
    now: Tick,
) -> (Vec<Candidate>, Vec<Exclusion>) {
    let mut candidates = Vec::new();
    let mut exclusions = Vec::new();
    for (index, profile) in profiles.iter().enumerate() {
        let label = profile.to_string();
        let usage = Usage::read(profile);
        if let Some(reason) = allowance_refusal(&usage, now) {
            exclusions.push(Exclusion { label, reason });
            continue;
        }
        if profile.authenticated_command().is_err() {
            exclusions.push(Exclusion {
                label,
                reason: "sign-in refused",
            });
            continue;
        }
        let usage = usage.unwrap_or_default();
        let headroom = Headroom {
            rate: usage.headroom_rate(now),
            remaining: usage.remaining(now),
            low_buffer: !usage.within_safety_buffer(now),
        };
        for configuration in configurations(profile.provider, pins.get(&label)) {
            candidates.push(Candidate {
                profile: index,
                label: label.clone(),
                metrics: configuration
                    .benchmark()
                    .map(|benchmark| match components {
                        Some(comp) => catalog.metrics_with_components(&benchmark, Some(comp)),
                        None => catalog.metrics(&benchmark),
                    })
                    .unwrap_or_default(),
                metric: None,
                configuration,
                headroom,
            });
        }
    }
    (candidates, exclusions)
}

pub(crate) fn ranking_metric(candidates: &[Candidate]) -> Option<Metric> {
    Metric::PREFERENCE.into_iter().find(|metric| {
        candidates
            .iter()
            .any(|candidate| candidate.metrics.value(*metric).is_some())
    })
}

fn compare_index(left: &Candidate, right: &Candidate) -> Ordering {
    match (left.score(), right.score()) {
        (Some(left), Some(right)) => right.total_cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_speed(left: &Candidate, right: &Candidate) -> Ordering {
    match (left.metrics.speed, right.metrics.speed) {
        (Some(left), Some(right)) => left.unit.cmp(&right.unit).then_with(|| match left.unit {
            SpeedUnit::SecondsPerTask => left.value.total_cmp(&right.value),
            SpeedUnit::TokensPerSecond => right.value.total_cmp(&left.value),
        }),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn headroom_tier(headroom: Headroom) -> u8 {
    match headroom.rate {
        Some(_) if !headroom.low_buffer => 0,
        None => 1,
        Some(_) => 2,
    }
}

fn compare_headroom(left: &Candidate, right: &Candidate) -> Ordering {
    headroom_tier(left.headroom)
        .cmp(&headroom_tier(right.headroom))
        .then_with(|| match (left.headroom.rate, right.headroom.rate) {
            (Some(left_rate), Some(right_rate)) if left.headroom.low_buffer => right
                .headroom
                .remaining
                .unwrap_or(0.0)
                .total_cmp(&left.headroom.remaining.unwrap_or(0.0))
                .then_with(|| right_rate.total_cmp(&left_rate)),
            (Some(left_rate), Some(right_rate)) => right_rate.total_cmp(&left_rate),
            _ => Ordering::Equal,
        })
}

fn compare_identity(left: &Candidate, right: &Candidate) -> Ordering {
    left.label
        .cmp(&right.label)
        .then_with(|| left.configuration.order().cmp(&right.configuration.order()))
}

pub(crate) fn compare(policy: Policy, left: &Candidate, right: &Candidate) -> Ordering {
    let order = match policy {
        Policy::Default => compare_index(left, right)
            .then_with(|| compare_speed(left, right))
            .then_with(|| compare_headroom(left, right)),
        Policy::Ordinary => compare_headroom(left, right)
            .then_with(|| compare_index(left, right))
            .then_with(|| compare_speed(left, right)),
        Policy::Urgent => compare_speed(left, right)
            .then_with(|| compare_headroom(left, right))
            .then_with(|| compare_index(left, right)),
    };
    order.then_with(|| compare_identity(left, right))
}

pub(crate) fn meets_floor(candidate: &Candidate, difficulty: Difficulty) -> bool {
    difficulty
        .floor()
        .is_none_or(|floor| candidate.score().is_some_and(|score| score >= floor))
}

fn floor_refusal(difficulty: Difficulty, metric: Option<Metric>, best: Option<f64>) -> String {
    let floor = difficulty.floor().unwrap_or_default();
    let name = difficulty.name();
    match (metric, best) {
        (Some(metric), Some(best)) => format!(
            "No eligible model meets the {name} floor {floor:.0} on the {}; best is {best:.1}.",
            metric.name()
        ),
        _ => format!(
            "No eligible model meets the {name} floor {floor:.0}: no published coding or intelligence index is available. Run agent-command catalog refresh with your own {KEY_VARIABLE}."
        ),
    }
}

pub(crate) fn rank(
    mut candidates: Vec<Candidate>,
    policy: Policy,
    difficulty: Difficulty,
) -> Result<Vec<Candidate>, String> {
    let metric = ranking_metric(&candidates);
    for candidate in &mut candidates {
        candidate.metric = metric;
    }
    let best = candidates
        .iter()
        .filter_map(Candidate::score)
        .max_by(f64::total_cmp);
    let total = candidates.len();
    candidates.retain(|candidate| meets_floor(candidate, difficulty));
    if candidates.is_empty() && total > 0 {
        return Err(floor_refusal(difficulty, metric, best));
    }
    candidates.sort_by(|left, right| compare(policy, left, right));
    Ok(candidates)
}

pub(crate) fn score_label(candidate: &Candidate) -> String {
    match (candidate.metric, candidate.score()) {
        (None, _) => "unscored".into(),
        (Some(metric), None) => format!("{} n/a", metric.name()),
        (Some(metric), Some(score)) => format!("{} {score:.1}", metric.name()),
    }
}

pub(crate) fn describe(candidate: &Candidate) -> String {
    let speed = candidate.metrics.speed.map_or_else(
        || "speed n/a".to_owned(),
        |speed| match speed.unit {
            SpeedUnit::SecondsPerTask => format!("{:.0}s/task", speed.value),
            SpeedUnit::TokensPerSecond => format!("{:.0} tok/s", speed.value),
        },
    );
    let headroom = candidate.headroom.remaining.map_or_else(
        || "headroom unknown".to_owned(),
        |remaining| format!("{:.0}% left", remaining.max(0.0).floor()),
    );
    format!(
        "{} {} · {} · {speed} · {headroom}",
        candidate.label,
        candidate.configuration.label(),
        score_label(candidate)
    )
}

fn exclusion_summary(exclusions: &[Exclusion]) -> String {
    exclusions
        .iter()
        .map(|exclusion| format!("{} {}", exclusion.label, exclusion.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub(crate) candidate: Candidate,
    pub(crate) reason: String,
}

pub(crate) fn require_ranked(
    ranked: Result<Vec<Candidate>, String>,
    exclusions: &[Exclusion],
) -> Result<Vec<Candidate>, String> {
    match ranked {
        Ok(ranked) if !ranked.is_empty() => Ok(ranked),
        Ok(_) => Err(with_exclusions(
            "No eligible signed-in subscription profile.".into(),
            exclusions,
        )),
        Err(reason) => Err(with_exclusions(reason, exclusions)),
    }
}

pub(crate) fn choose(
    ranked: Result<Vec<Candidate>, String>,
    exclusions: &[Exclusion],
    preferred: Option<&str>,
    rotation: &[String],
) -> Result<Choice, String> {
    let ranked = require_ranked(ranked, exclusions)?;
    let best = ranked[0].clone();
    let Some(preferred) = preferred else {
        return Ok(Choice {
            reason: format!("Auto → {}", describe(&best)),
            candidate: best,
        });
    };
    let best_for = |label: &str| ranked.iter().find(|candidate| candidate.label == label);
    if let Some(candidate) = best_for(preferred) {
        return Ok(Choice {
            reason: format!("Override → {}", describe(candidate)),
            candidate: candidate.clone(),
        });
    }
    let why = exclusions
        .iter()
        .find(|exclusion| exclusion.label == preferred)
        .map_or("below the floor or not found", |exclusion| exclusion.reason);
    if let Some(candidate) = rotation
        .iter()
        .filter(|label| *label != preferred)
        .find_map(|label| best_for(label))
    {
        return Ok(Choice {
            reason: format!(
                "Override {preferred} skipped: {why} · rotated → {}",
                describe(candidate)
            ),
            candidate: candidate.clone(),
        });
    }
    Ok(Choice {
        reason: format!(
            "Override {preferred} skipped: {why} · Auto → {}",
            describe(&best)
        ),
        candidate: best,
    })
}

fn with_exclusions(reason: String, exclusions: &[Exclusion]) -> String {
    if exclusions.is_empty() {
        reason
    } else {
        format!("{reason} Skipped: {}.", exclusion_summary(exclusions))
    }
}

pub(crate) fn allocate(units: usize, ranked: &[Candidate]) -> Vec<Candidate> {
    let mut per_profile: Vec<&Candidate> = Vec::new();
    for candidate in ranked {
        if per_profile
            .iter()
            .all(|chosen| chosen.profile != candidate.profile)
        {
            per_profile.push(candidate);
        }
    }
    let known = per_profile
        .iter()
        .any(|candidate| candidate.headroom.remaining.is_some_and(|left| left > 0.0));
    let weights: Vec<f64> = per_profile
        .iter()
        .map(|candidate| match candidate.headroom.remaining {
            Some(left) if known => left,
            None if !known => 1.0,
            _ => 0.0,
        })
        .collect();
    let mut assigned = vec![0usize; per_profile.len()];
    (0..units)
        .filter_map(|_| {
            let slot = (0..per_profile.len())
                .filter(|slot| weights[*slot] > 0.0)
                .max_by(|left, right| {
                    let quotient = |slot: usize| weights[slot] / (assigned[slot] + 1) as f64;
                    quotient(*left)
                        .total_cmp(&quotient(*right))
                        .then_with(|| right.cmp(left))
                })?;
            assigned[slot] += 1;
            Some(per_profile[slot].clone())
        })
        .collect()
}

pub const BOUNDED_MECHANICAL_SPEED_DELTA: f64 = 5.0;
const QUOTA_PACE_FILE: &str = "quota-pace.json";
const QUOTA_PACE_SCHEMA: &str = "agent-command-quota-pace-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkClass {
    HardDesign,
    BoundedMechanical,
    Debugging,
    Verification,
    Implementation,
    Pinned,
}

impl WorkClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::HardDesign => "hard-design",
            Self::BoundedMechanical => "bounded-mechanical",
            Self::Debugging => "debugging",
            Self::Verification => "verification",
            Self::Implementation => "implementation",
            Self::Pinned => "pinned",
        }
    }

    pub fn preferred_component(self) -> Option<BenchmarkComponent> {
        match self {
            Self::Implementation => Some(BenchmarkComponent::FrontierCode),
            Self::Debugging => Some(BenchmarkComponent::TerminalBench),
            Self::Verification => Some(BenchmarkComponent::SweAtlasQna),
            Self::HardDesign | Self::BoundedMechanical | Self::Pinned => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    Design,
    Plan,
    Mechanical,
    Implementation,
    Debugging,
    Review,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskClassification {
    pub task_type: TaskType,
    pub complexity: Complexity,
    pub work_class: WorkClass,
}

pub fn classify_task(
    title: &str,
    body: &str,
    stage_is_verification: bool,
    is_retry_or_resume: bool,
    explicit_class: Option<WorkClass>,
) -> TaskClassification {
    let combined = format!("{title}\n{body}").to_ascii_lowercase();
    let words: Vec<&str> = combined
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .collect();
    let has_word = |targets: &[&str]| words.iter().any(|word| targets.contains(word));

    if is_retry_or_resume || has_retry_suffix(title.trim()) {
        return TaskClassification {
            task_type: if stage_is_verification {
                TaskType::Review
            } else {
                TaskType::Implementation
            },
            complexity: Complexity::Medium,
            work_class: WorkClass::Pinned,
        };
    }
    if stage_is_verification {
        return TaskClassification {
            task_type: TaskType::Review,
            complexity: Complexity::Medium,
            work_class: WorkClass::Verification,
        };
    }
    if let Some(work_class) = explicit_class {
        let (task_type, complexity) = match work_class {
            WorkClass::HardDesign => (TaskType::Design, Complexity::High),
            WorkClass::BoundedMechanical => (TaskType::Mechanical, Complexity::Low),
            WorkClass::Debugging => (TaskType::Debugging, Complexity::Medium),
            WorkClass::Verification => (TaskType::Review, Complexity::Medium),
            WorkClass::Implementation => (TaskType::Implementation, Complexity::Medium),
            WorkClass::Pinned => (TaskType::Implementation, Complexity::Medium),
        };
        return TaskClassification {
            task_type,
            complexity,
            work_class,
        };
    }
    if has_word(&["architecture", "design", "plan", "rfc", "blueprint"]) {
        TaskClassification {
            task_type: TaskType::Design,
            complexity: Complexity::High,
            work_class: WorkClass::HardDesign,
        }
    } else if has_word(&["debug", "repair", "crash", "flaky", "regression"]) {
        TaskClassification {
            task_type: TaskType::Debugging,
            complexity: Complexity::Medium,
            work_class: WorkClass::Debugging,
        }
    } else if has_word(&["format", "formatting", "rename", "typo", "lint"]) {
        TaskClassification {
            task_type: TaskType::Mechanical,
            complexity: Complexity::Low,
            work_class: WorkClass::BoundedMechanical,
        }
    } else if has_word(&["fix"]) {
        TaskClassification {
            task_type: TaskType::Debugging,
            complexity: Complexity::Medium,
            work_class: WorkClass::Debugging,
        }
    } else if has_word(&["verify", "verification", "review", "audit"]) {
        TaskClassification {
            task_type: TaskType::Review,
            complexity: Complexity::Medium,
            work_class: WorkClass::Verification,
        }
    } else {
        TaskClassification {
            task_type: TaskType::Implementation,
            complexity: Complexity::Medium,
            work_class: WorkClass::Implementation,
        }
    }
}

fn has_retry_suffix(title: &str) -> bool {
    let Some((_, tail)) = title.rsplit_once('-') else {
        return false;
    };
    let rest = tail
        .strip_prefix("NR")
        .or_else(|| tail.strip_prefix('C'))
        .or_else(|| tail.strip_prefix('R'));
    rest.is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn shared_component_for_work_class(
    candidates: &[Candidate],
    work_class: WorkClass,
) -> Option<BenchmarkComponent> {
    let component = work_class.preferred_component()?;
    let scored: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| {
            candidate.metrics.component(component).is_some()
                || candidate.metrics.coding_agent_index.is_some()
                || candidate.metrics.coding_index.is_some()
                || candidate.metrics.intelligence_index.is_some()
        })
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored
        .iter()
        .all(|candidate| candidate.metrics.component(component).is_some())
        .then_some(component)
}

fn effective_work_score(
    candidate: &Candidate,
    component: Option<BenchmarkComponent>,
) -> Option<f64> {
    if let Some(component) = component {
        return candidate.metrics.component(component);
    }
    candidate.score().or(candidate.metrics.coding_agent_index)
}

pub(crate) fn rank_for_work_class(
    mut candidates: Vec<Candidate>,
    policy: Policy,
    difficulty: Difficulty,
    work_class: WorkClass,
) -> Result<Vec<Candidate>, String> {
    let metric = ranking_metric(&candidates);
    for candidate in &mut candidates {
        candidate.metric = metric;
    }
    let component = shared_component_for_work_class(&candidates, work_class);
    let best = candidates
        .iter()
        .filter_map(|candidate| effective_work_score(candidate, component))
        .max_by(f64::total_cmp);
    let total = candidates.len();
    if let Some(floor) = difficulty.floor() {
        candidates.retain(|candidate| {
            effective_work_score(candidate, component).is_some_and(|score| score >= floor)
        });
    }
    if candidates.is_empty() && total > 0 {
        if let (Some(comp), Some(best_val)) = (component, best) {
            let floor = difficulty.floor().unwrap_or_default();
            return Err(format!(
                "No eligible model meets the {} floor {floor:.0} on {}; best is {best_val:.1}.",
                difficulty.name(),
                comp.name()
            ));
        }
        return Err(floor_refusal(difficulty, metric, best));
    }
    let use_mechanical_speed = if work_class == WorkClass::BoundedMechanical {
        if let Some(best_score) = best {
            let near_best: Vec<&Candidate> = candidates
                .iter()
                .filter(|candidate| {
                    effective_work_score(candidate, component)
                        .is_some_and(|score| score >= best_score - BOUNDED_MECHANICAL_SPEED_DELTA)
                })
                .collect();
            !near_best.is_empty()
                && near_best
                    .iter()
                    .all(|candidate| candidate.metrics.speed.is_some())
        } else {
            false
        }
    } else {
        false
    };
    let score_cmp = |left: &Candidate, right: &Candidate| match (
        effective_work_score(left, component),
        effective_work_score(right, component),
    ) {
        (Some(l), Some(r)) => r.total_cmp(&l),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    candidates.sort_by(|left, right| {
        let primary = if use_mechanical_speed {
            let best_score = best.unwrap_or(0.0);
            let left_near = effective_work_score(left, component)
                .is_some_and(|score| score >= best_score - BOUNDED_MECHANICAL_SPEED_DELTA);
            let right_near = effective_work_score(right, component)
                .is_some_and(|score| score >= best_score - BOUNDED_MECHANICAL_SPEED_DELTA);
            right_near
                .cmp(&left_near)
                .then_with(|| compare_speed(left, right))
                .then_with(|| score_cmp(left, right))
                .then_with(|| compare_headroom(left, right))
        } else {
            match policy {
                Policy::Default => score_cmp(left, right)
                    .then_with(|| compare_speed(left, right))
                    .then_with(|| compare_headroom(left, right)),
                Policy::Ordinary => compare_headroom(left, right)
                    .then_with(|| score_cmp(left, right))
                    .then_with(|| compare_speed(left, right)),
                Policy::Urgent => compare_speed(left, right)
                    .then_with(|| compare_headroom(left, right))
                    .then_with(|| score_cmp(left, right)),
            }
        };
        primary.then_with(|| compare_identity(left, right))
    });
    Ok(candidates)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowAllowance {
    pub window_id: String,
    pub remaining: f64,
    pub reserved: f64,
    pub reset_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveUsageHold {
    pub profile: String,
    pub window_id: String,
    pub reset_at: i64,
    pub created_at: i64,
    pub usage_units: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PacedWindow {
    pub profile: String,
    pub window_id: String,
    pub reset_at: i64,
    pub started_at: i64,
    pub original_remaining: f64,
}

#[derive(Serialize, Deserialize)]
struct QuotaPaceFile {
    schema: String,
    windows: BTreeMap<String, PacedWindow>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotaPacer {
    windows: BTreeMap<String, PacedWindow>,
}

impl QuotaPacer {
    pub fn load(state_dir: &Path) -> io::Result<Self> {
        let path = state_dir.join(QUOTA_PACE_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let file: QuotaPaceFile = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid quota pace state {}: {error}", path.display()),
            )
        })?;
        if file.schema != QUOTA_PACE_SCHEMA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Unsupported quota pace schema {} in {}",
                    file.schema,
                    path.display()
                ),
            ));
        }
        Ok(Self {
            windows: file.windows,
        })
    }

    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        fs::create_dir_all(state_dir)?;
        let path = state_dir.join(QUOTA_PACE_FILE);
        let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
        let payload = QuotaPaceFile {
            schema: QUOTA_PACE_SCHEMA.into(),
            windows: self.windows.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&payload).map_err(io::Error::other)?;
        let mut file = fs::File::create_new(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if fs::rename(&temporary, &path).is_err() {
            fs::write(&path, &bytes)?;
            let _ = fs::remove_file(&temporary);
        }
        Ok(())
    }

    pub fn prune_expired(&mut self, now: i64) {
        self.windows.retain(|_, entry| entry.reset_at > now);
    }

    pub fn tracked_windows(&self) -> usize {
        self.windows.len()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn effective_remaining(
        &mut self,
        profile: &str,
        window: &WindowAllowance,
        pace_burst_percent: Option<f64>,
        active_holds: &[ActiveUsageHold],
        urgent_reserve: f64,
        is_urgent: bool,
        now: i64,
    ) -> Option<f64> {
        if window.reset_at <= now {
            return None;
        }
        if !window.remaining.is_finite()
            || !window.reserved.is_finite()
            || !urgent_reserve.is_finite()
            || window.remaining < 0.0
            || window.reserved < 0.0
            || urgent_reserve < 0.0
        {
            return None;
        }
        let paced_remaining = match pace_burst_percent {
            Some(burst) if burst.is_finite() && burst > 0.0 => {
                let key = format!("{profile}:{}:{}", window.window_id, window.reset_at);
                let baseline = self.windows.entry(key).or_insert_with(|| PacedWindow {
                    profile: profile.to_owned(),
                    window_id: window.window_id.clone(),
                    reset_at: window.reset_at,
                    started_at: now,
                    original_remaining: window.remaining,
                });
                let immediate = baseline.original_remaining.min(burst);
                let span = (window.reset_at - baseline.started_at).max(1) as f64;
                let progress = ((now - baseline.started_at) as f64 / span).clamp(0.0, 1.0);
                let allowed = immediate + (baseline.original_remaining - immediate) * progress;
                let estimated_holds: f64 = active_holds
                    .iter()
                    .filter(|hold| {
                        hold.profile == profile
                            && hold.window_id == window.window_id
                            && hold.reset_at == window.reset_at
                            && hold.created_at >= baseline.started_at
                            && hold.usage_units.is_finite()
                            && hold.usage_units > 0.0
                    })
                    .map(|hold| hold.usage_units)
                    .sum();
                let consumed =
                    (baseline.original_remaining - window.remaining).max(estimated_holds);
                window
                    .remaining
                    .min((allowed - consumed).max(0.0) + window.reserved)
            }
            _ => window.remaining,
        };
        let reserve = if is_urgent { 0.0 } else { urgent_reserve };
        let usable = paced_remaining - window.reserved - reserve;
        Some(usable.max(0.0))
    }
}

pub fn burn_score(usable_remaining: f64, reset_at: i64, now: i64) -> f64 {
    if !usable_remaining.is_finite() || usable_remaining <= 0.0 || reset_at <= now {
        return 0.0;
    }
    usable_remaining / (reset_at - now).max(1) as f64 * 3600.0
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePriority {
    Urgent,
    Next,
    #[default]
    Normal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueuedTask {
    pub id: String,
    pub project: String,
    #[serde(default)]
    pub priority: QueuePriority,
    pub created_at: i64,
    #[serde(default)]
    pub blocked: bool,
}

pub fn pick_next_task<'a>(
    tasks: &'a [QueuedTask],
    served_by_project: &HashMap<String, u64>,
) -> Option<&'a QueuedTask> {
    let ready: Vec<&QueuedTask> = tasks.iter().filter(|task| !task.blocked).collect();
    for priority in [QueuePriority::Urgent, QueuePriority::Next] {
        let mut group: Vec<&QueuedTask> = ready
            .iter()
            .copied()
            .filter(|task| task.priority == priority)
            .collect();
        if !group.is_empty() {
            group.sort_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.id.cmp(&right.id))
            });
            return group.into_iter().next();
        }
    }
    let mut by_project: BTreeMap<&str, Vec<&QueuedTask>> = BTreeMap::new();
    for task in ready {
        by_project
            .entry(task.project.as_str())
            .or_default()
            .push(task);
    }
    let mut winner: Option<(&str, f64, i64)> = None;
    for (project, list) in &by_project {
        let served = served_by_project.get(*project).copied().unwrap_or(0) as f64;
        let weight = (list.len() as f64).sqrt().clamp(1.0, 3.0);
        let ratio = served / weight;
        let earliest = list.iter().map(|t| t.created_at).min().unwrap_or(i64::MAX);
        let better = match winner {
            None => true,
            Some((win_proj, win_ratio, win_earliest)) => {
                ratio
                    .total_cmp(&win_ratio)
                    .then_with(|| earliest.cmp(&win_earliest))
                    .then_with(|| project.cmp(&win_proj))
                    == Ordering::Less
            }
        };
        if better {
            winner = Some((project, ratio, earliest));
        }
    }
    let (winning_project, _, _) = winner?;
    let mut chosen_list = by_project.remove(winning_project)?;
    chosen_list.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    chosen_list.into_iter().next()
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::{
        catalog::Speed,
        usage::{tests::Fixture, tick},
    };
    use serde_json::json;

    const NOW: i64 = 10_000;

    pub(crate) const SIGNED_IN: &str = r#"
if test "$1" = auth; then
    profile=${CLAUDE_CONFIG_DIR##*/}
    case "$profile" in refused*) exit 1 ;; esac
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
    exit
fi
if test "$1" = -c && test "$5" = login; then
    profile=${CODEX_HOME##*/}
    case "$profile" in refused*) exit 1 ;; esac
    printf '%s\n' 'Logged in using ChatGPT' >&2
    exit
fi
exit 3
"#;

    pub(crate) fn pin(profile: &str, model: &str, effort: Option<&str>) -> Pin {
        Pin {
            profile: profile.into(),
            model: model.into(),
            effort: effort.map(str::to_owned),
        }
    }

    fn claude_configuration(slot: usize) -> Configuration {
        let pinned = pin("claude/fixture", "claude-opus-5-5", Some("high"));
        configurations(Provider::Claude, Some(&pinned)).swap_remove(slot)
    }

    fn metrics(index: Option<(Metric, f64)>, speed: Option<(SpeedUnit, f64)>) -> Metrics {
        let value = |metric| {
            index
                .filter(|(chosen, _)| *chosen == metric)
                .map(|(_, value)| value)
        };
        Metrics {
            coding_agent_index: None,
            coding_index: value(Metric::Coding),
            intelligence_index: value(Metric::Intelligence),
            speed: speed.map(|(unit, value)| Speed { unit, value }),
            components: crate::catalog::ComponentScores::default(),
        }
    }

    fn candidate(
        label: &str,
        configuration: usize,
        index: Option<(Metric, f64)>,
        speed: Option<(SpeedUnit, f64)>,
        rate: Option<f64>,
    ) -> Candidate {
        Candidate {
            profile: label.bytes().map(usize::from).sum(),
            label: label.into(),
            configuration: claude_configuration(configuration),
            metrics: metrics(index, speed),
            metric: None,
            headroom: Headroom {
                rate,
                remaining: rate.map(|rate| rate * 100.0),
                low_buffer: false,
            },
        }
    }

    fn observe(profile: &Profile, five: (f64, i64), week: Option<(f64, i64)>) {
        let mut windows = json!({"five_hour": {"utilization": five.0, "resetsAt": NOW + five.1}});
        if let Some((used, reset)) = week {
            windows["seven_day"] = json!({"utilization": used, "resetsAt": NOW + reset});
        }
        Usage::observe_claude(
            profile,
            &json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":windows}}),
            tick(NOW, 0),
        )
        .unwrap();
    }

    #[test]
    fn routing_gate_excludes_latched_exhausted_corrupt_and_refused_profiles() {
        let fixture = Fixture::new("routing-gate", SIGNED_IN);
        let profiles = [
            fixture.profile(Provider::Claude, "latched"),
            fixture.profile(Provider::Claude, "exhausted"),
            fixture.profile(Provider::Claude, "corrupt"),
            fixture.profile(Provider::Claude, "refused"),
            fixture.profile(Provider::Claude, "stale"),
            fixture.profile(Provider::Codex, "unreported"),
            fixture.profile(Provider::Claude, "past-reset"),
        ];
        Usage::observe_claude(
            &profiles[0],
            &json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":NOW + 60}}),
            tick(NOW, 0),
        )
        .unwrap();
        observe(&profiles[1], (0.2, 600), Some((1.0, 6000)));
        fs::write(
            profiles[2].usage_directory().unwrap().join("state.json"),
            b"{",
        )
        .unwrap();
        observe(&profiles[4], (0.9, 90_000), None);
        observe(&profiles[6], (1.0, 1), None);
        let later = tick(NOW + 2, 2_000);
        let (candidates, exclusions) =
            candidates(&profiles, &Catalog::load(None), &Pins::default(), later);
        assert_eq!(
            exclusions,
            [
                ("claude/latched", "latched"),
                ("claude/exhausted", "usage window exhausted"),
                ("claude/corrupt", "usage state unreadable"),
                ("claude/refused", "sign-in refused"),
            ]
            .map(|(label, reason)| Exclusion {
                label: label.into(),
                reason
            })
        );
        let mut labels: Vec<_> = candidates.iter().map(|c| c.label.as_str()).collect();
        labels.dedup();
        assert_eq!(
            labels,
            ["claude/stale", "codex/unreported", "claude/past-reset"]
        );
        assert!(candidates
            .iter()
            .all(|candidate| candidate.metrics == Metrics::default()));
        let unreported = candidates
            .iter()
            .find(|candidate| candidate.label == "codex/unreported")
            .unwrap();
        assert_eq!(unreported.headroom, Headroom::default());
    }

    #[test]
    fn difficulty_floor_never_selects_a_below_floor_or_unscored_model() {
        let ranked = rank(
            vec![
                candidate(
                    "claude/a",
                    3,
                    Some((Metric::Intelligence, 54.9)),
                    None,
                    Some(1.0),
                ),
                candidate("claude/b", 0, None, None, Some(9.0)),
            ],
            Policy::Ordinary,
            Difficulty::Hard,
        );
        let error = choose(
            ranked,
            &[Exclusion {
                label: "claude/c".into(),
                reason: "latched",
            }],
            None,
            &[],
        )
        .unwrap_err();
        assert_eq!(
            error,
            "No eligible model meets the hard floor 55 on the AA intelligence index; best is 54.9. Skipped: claude/c latched."
        );
        let ranked = rank(
            vec![
                candidate(
                    "claude/a",
                    3,
                    Some((Metric::Intelligence, 54.9)),
                    None,
                    Some(9.0),
                ),
                candidate(
                    "claude/b",
                    3,
                    Some((Metric::Intelligence, 55.0)),
                    None,
                    Some(1.0),
                ),
            ],
            Policy::Ordinary,
            Difficulty::Hard,
        )
        .unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].label, "claude/b");
        assert_eq!(
            describe(&ranked[0]),
            "claude/b claude-opus-5-5 high · AA intelligence index 55.0 · speed n/a · 100% left"
        );
        assert_eq!(
            rank(
                vec![candidate("claude/b", 0, None, None, None)],
                Policy::Default,
                Difficulty::Any
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn keyless_standard_and_hard_fail_closed_with_a_visible_reason() {
        let fixture = Fixture::new("keyless-floor", SIGNED_IN);
        let profiles = [fixture.profile(Provider::Claude, "work")];
        let pins =
            Pins::from_pins(vec![pin("claude/work", "claude-opus-5-5", Some("high"))]).unwrap();
        for difficulty in [Difficulty::Standard, Difficulty::Hard] {
            let (candidates, _) = candidates(&profiles, &Catalog::load(None), &pins, tick(NOW, 0));
            assert_eq!(candidates.len(), 4);
            let floor = difficulty.floor().unwrap();
            assert_eq!(
                rank(candidates, Policy::Default, difficulty).unwrap_err(),
                format!(
                    "No eligible model meets the {} floor {floor:.0}: no published coding or intelligence index is available. Run agent-command catalog refresh with your own ARTIFICIAL_ANALYSIS_API_KEY.",
                    difficulty.name()
                )
            );
        }
        let (candidates, _) = candidates(&profiles, &Catalog::load(None), &pins, tick(NOW, 0));
        let ranked = rank(candidates, Policy::Default, Difficulty::Any).unwrap();
        assert!(ranked.iter().all(|candidate| candidate.metric.is_none()));
        assert_eq!(
            describe(&ranked[0]),
            "claude/work provider default · unscored · speed n/a · headroom unknown"
        );
        assert_eq!((STANDARD_FLOOR, HARD_FLOOR), (40.0, 55.0));
    }

    #[test]
    fn cold_start_with_an_effort_suffixed_pin_fails_closed() {
        let fixture = Fixture::new("keyless-variant", SIGNED_IN);
        let profiles = [fixture.profile(Provider::Claude, "work")];
        let state = fixture.root.join("variant-pins");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join(crate::native::PINS_FILE),
            r#"{"schema":"agent-command-native-pins-v1","pins":[{"profile":"claude/work","model":"claude-opus-5-5-high","effort":null}]}"#,
        )
        .unwrap();
        assert!(Pins::load(&state)
            .unwrap_err()
            .to_string()
            .contains("benchmark effort variant"));
        let bypassed = pin("claude/work", "claude-opus-5-5-high", None);
        let (unvalidated, _) = candidates(
            &profiles,
            &Catalog::load(None),
            &Pins::default(),
            tick(NOW, 0),
        );
        assert_eq!(
            configurations(Provider::Claude, Some(&bypassed)).len(),
            unvalidated.len()
        );
        for difficulty in [Difficulty::Standard, Difficulty::Hard] {
            assert!(rank(unvalidated.clone(), Policy::Default, difficulty)
                .unwrap_err()
                .contains("no published coding or intelligence index is available"));
        }
        let any = rank(unvalidated, Policy::Default, Difficulty::Any).unwrap();
        assert!(any.iter().all(|candidate| !candidate
            .configuration
            .arguments()
            .contains(&"claude-opus-5-5-high".to_owned())));
    }

    #[test]
    fn ordinary_policy_ranks_by_soonest_expiring_headroom_with_stable_ties() {
        let fixture = Fixture::new("ordinary-policy", SIGNED_IN);
        let profiles = [
            fixture.profile(Provider::Claude, "weekly-bound"),
            fixture.profile(Provider::Claude, "expiring"),
            fixture.profile(Provider::Claude, "tie-b"),
            fixture.profile(Provider::Claude, "tie-a"),
            fixture.profile(Provider::Claude, "unknown"),
        ];
        observe(&profiles[0], (0.0, 100), Some((0.9, 100_000)));
        observe(&profiles[1], (0.5, 100), Some((0.5, 1_000)));
        observe(&profiles[2], (0.5, 2_000), None);
        observe(&profiles[3], (0.5, 2_000), None);
        let (candidates, _) = candidates(
            &profiles,
            &Catalog::load(None),
            &Pins::default(),
            tick(NOW, 0),
        );
        let ranked = rank(candidates, Policy::Ordinary, Difficulty::Any).unwrap();
        let order: Vec<_> = ranked
            .iter()
            .filter(|candidate| candidate.configuration.is_default())
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert_eq!(
            order,
            [
                "claude/expiring",
                "claude/tie-a",
                "claude/tie-b",
                "claude/weekly-bound",
                "claude/unknown"
            ]
        );
        assert_eq!(ranked[0].headroom.rate, Some(0.05));
        assert!(describe(ranked.last().unwrap()).contains("headroom unknown"));
    }

    #[test]
    fn urgent_policy_puts_fastest_same_unit_first_and_breaks_ties_by_headroom() {
        let candidates = vec![
            candidate(
                "claude/tokens",
                1,
                None,
                Some((SpeedUnit::TokensPerSecond, 500.0)),
                Some(1.0),
            ),
            candidate(
                "claude/slow",
                1,
                None,
                Some((SpeedUnit::SecondsPerTask, 90.0)),
                Some(5.0),
            ),
            candidate(
                "claude/fast-a-low",
                1,
                None,
                Some((SpeedUnit::SecondsPerTask, 30.0)),
                Some(1.0),
            ),
            candidate(
                "claude/fast-b-high",
                1,
                None,
                Some((SpeedUnit::SecondsPerTask, 30.0)),
                Some(2.0),
            ),
            candidate("claude/unmeasured", 1, None, None, Some(9.0)),
        ];
        let ranked = rank(candidates.clone(), Policy::Urgent, Difficulty::Any).unwrap();
        let order: Vec<_> = ranked.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            order,
            [
                "claude/fast-b-high",
                "claude/fast-a-low",
                "claude/slow",
                "claude/tokens",
                "claude/unmeasured"
            ]
        );
        assert!(describe(&ranked[4]).contains("speed n/a"));
        let reversed: Vec<_> = candidates.into_iter().rev().collect();
        let again = rank(reversed, Policy::Urgent, Difficulty::Any).unwrap();
        assert_eq!(
            again.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            order
        );
    }

    #[test]
    fn one_metric_ranks_every_candidate_and_missing_values_stay_null() {
        let intelligence_only = vec![
            candidate(
                "claude/high",
                3,
                Some((Metric::Intelligence, 63.0)),
                None,
                None,
            ),
            candidate(
                "claude/low",
                3,
                Some((Metric::Intelligence, 58.0)),
                None,
                Some(9.0),
            ),
            candidate("claude/alias", 1, None, None, Some(9.0)),
        ];
        assert_eq!(
            ranking_metric(&intelligence_only),
            Some(Metric::Intelligence)
        );
        let ranked = rank(
            intelligence_only.clone(),
            Policy::Default,
            Difficulty::Standard,
        )
        .unwrap();
        assert_eq!(
            ranked.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["claude/high", "claude/low"]
        );
        assert!(describe(&ranked[1]).contains("· AA intelligence index 58.0 ·"));
    }

    fn with_agent_index(mut candidate: Candidate, index: f64) -> Candidate {
        candidate.metrics.coding_agent_index = Some(index);
        candidate
    }

    fn agent_only(label: &str, index: f64) -> Candidate {
        with_agent_index(candidate(label, 3, None, None, Some(9.0)), index)
    }

    fn order(ranked: &[Candidate]) -> Vec<&str> {
        ranked.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn coding_index_ranks_ahead_of_an_independent_coding_agent_index() {
        let candidates = vec![
            with_agent_index(
                candidate("claude/coding", 3, Some((Metric::Coding, 70.0)), None, None),
                41.0,
            ),
            agent_only("claude/agent", 99.0),
            candidate(
                "claude/intelligence",
                3,
                Some((Metric::Intelligence, 90.0)),
                None,
                Some(9.0),
            ),
        ];
        assert_eq!(ranking_metric(&candidates), Some(Metric::Coding));
        let hard = rank(candidates.clone(), Policy::Default, Difficulty::Hard).unwrap();
        assert_eq!(order(&hard), ["claude/coding"]);
        assert!(describe(&hard[0]).contains("· AA coding index 70.0 ·"));
        let any = rank(candidates, Policy::Default, Difficulty::Any).unwrap();
        assert_eq!(
            order(&any),
            ["claude/coding", "claude/agent", "claude/intelligence"]
        );
        assert!(any
            .iter()
            .all(|candidate| candidate.metric == Some(Metric::Coding)));
        assert!(describe(&any[1]).contains("· AA coding index n/a ·"));
        assert!(describe(&any[2]).contains("· AA coding index n/a ·"));
        assert!(any
            .iter()
            .all(|candidate| !describe(candidate).contains("agent index")));
    }

    #[test]
    fn intelligence_index_ranks_ahead_of_an_independent_coding_agent_index() {
        let candidates = vec![
            candidate(
                "claude/intelligence",
                3,
                Some((Metric::Intelligence, 58.0)),
                None,
                None,
            ),
            agent_only("claude/agent", 99.0),
        ];
        assert_eq!(ranking_metric(&candidates), Some(Metric::Intelligence));
        let hard = rank(candidates.clone(), Policy::Default, Difficulty::Hard).unwrap();
        assert_eq!(order(&hard), ["claude/intelligence"]);
        let any = rank(candidates, Policy::Default, Difficulty::Any).unwrap();
        assert_eq!(order(&any), ["claude/intelligence", "claude/agent"]);
        assert!(describe(&any[1]).contains("· AA intelligence index n/a ·"));
        let below = vec![
            candidate(
                "claude/intelligence",
                3,
                Some((Metric::Intelligence, 41.0)),
                None,
                None,
            ),
            agent_only("claude/agent", 99.0),
        ];
        assert_eq!(
            rank(below, Policy::Default, Difficulty::Hard).unwrap_err(),
            "No eligible model meets the hard floor 55 on the AA intelligence index; best is 41.0."
        );
    }

    #[test]
    fn coding_agent_index_alone_never_scores_a_ranking() {
        let candidates = vec![agent_only("claude/agent", 99.0)];
        assert_eq!(ranking_metric(&candidates), None);
        assert!(
            rank(candidates.clone(), Policy::Default, Difficulty::Standard)
                .unwrap_err()
                .contains("no published coding or intelligence index is available")
        );
        let any = rank(candidates, Policy::Default, Difficulty::Any).unwrap();
        assert!(describe(&any[0]).contains("· unscored ·"));
    }

    #[test]
    fn split_units_spread_by_headroom_without_ineligible_or_repeated_profiles() {
        let ranked = vec![
            candidate("claude/big", 1, None, None, Some(0.6)),
            candidate("claude/big", 2, None, None, Some(0.6)),
            candidate("claude/small", 1, None, None, Some(0.3)),
            candidate("claude/unknown", 1, None, None, None),
        ];
        let plan: Vec<_> = allocate(6, &ranked)
            .into_iter()
            .map(|candidate| candidate.label)
            .collect();
        assert_eq!(
            plan,
            [
                "claude/big",
                "claude/big",
                "claude/small",
                "claude/big",
                "claude/big",
                "claude/small"
            ]
        );
        let tie = vec![
            candidate("claude/first", 1, None, None, Some(0.5)),
            candidate("claude/second", 1, None, None, Some(0.5)),
        ];
        let plan: Vec<_> = allocate(3, &tie).into_iter().map(|c| c.label).collect();
        assert_eq!(plan, ["claude/first", "claude/second", "claude/first"]);
        let unknown = vec![
            candidate("claude/x", 1, None, None, None),
            candidate("claude/y", 1, None, None, None),
        ];
        let plan: Vec<_> = allocate(3, &unknown).into_iter().map(|c| c.label).collect();
        assert_eq!(plan, ["claude/x", "claude/y", "claude/x"]);
        assert!(allocate(2, &[]).is_empty());
    }

    #[test]
    fn split_requests_need_explicit_independent_units() {
        let request = parse("/urgent /hard /split write docs ;; add tests ;; fix lint").unwrap();
        assert_eq!(request.policy, Policy::Urgent);
        assert_eq!(request.difficulty, Difficulty::Hard);
        assert_eq!(request.units, ["write docs", "add tests", "fix lint"]);
        let plain = parse("write docs ;; add tests").unwrap();
        assert!(!plain.split);
        assert_eq!(plain.units, ["write docs ;; add tests"]);
        assert!(parse("/split only one unit").is_err());
        assert!(parse("/split a ;; ;; b").is_err());
        assert!(parse("/hard").is_err());
        assert_eq!(parse("/help me").unwrap().units, ["/help me"]);
    }

    #[test]
    fn override_persists_and_falls_back_with_visible_reason() {
        let fixture = Fixture::new("override-state", "");
        let state = state_directory(&fixture.root.join("profiles")).unwrap();
        assert_eq!(load_override(&state).unwrap(), None);
        save_override(&state, Some("claude/b")).unwrap();
        assert_eq!(load_override(&state).unwrap().as_deref(), Some("claude/b"));
        let ranked = vec![
            candidate("claude/a", 0, None, None, Some(2.0)),
            candidate("claude/b", 0, None, None, Some(1.0)),
        ];
        let chosen = choose(Ok(ranked.clone()), &[], Some("claude/b"), &[]).unwrap();
        assert_eq!(chosen.candidate.label, "claude/b");
        assert!(chosen.reason.starts_with("Override → claude/b"));
        let exclusions = [Exclusion {
            label: "claude/c".into(),
            reason: "sign-in refused",
        }];
        let fallback = choose(Ok(ranked.clone()), &exclusions, Some("claude/c"), &[]).unwrap();
        assert_eq!(fallback.candidate.label, "claude/a");
        assert!(fallback
            .reason
            .starts_with("Override claude/c skipped: sign-in refused · Auto → claude/a"));
        let rotated = choose(
            Ok(ranked.clone()),
            &exclusions,
            Some("claude/c"),
            &["claude/c".into(), "claude/b".into()],
        )
        .unwrap();
        assert_eq!(rotated.candidate.label, "claude/b");
        assert!(rotated
            .reason
            .contains("skipped: sign-in refused · rotated → claude/b"));
        assert_eq!(
            choose(Ok(Vec::new()), &exclusions, None, &[]).unwrap_err(),
            "No eligible signed-in subscription profile. Skipped: claude/c sign-in refused."
        );
        assert!(choose(Ok(ranked), &[], None, &[])
            .unwrap()
            .reason
            .starts_with("Auto → claude/a provider default"));
        save_override(&state, None).unwrap();
        assert_eq!(load_override(&state).unwrap(), None);
        fs::write(state.join(OVERRIDE_FILE), b"{\"profile\":1}").unwrap();
        assert!(load_override(&state).is_err());
    }

    #[test]
    fn configurations_produce_native_execution_arguments() {
        let claude = |effort: Option<&str>| {
            configurations(
                Provider::Claude,
                Some(&pin("claude/work", "claude-opus-5-5", effort)),
            )
        };
        let pinned = claude(Some("high"));
        assert_eq!(pinned.len(), 4);
        assert!(pinned[0].arguments().is_empty());
        assert_eq!(pinned[1].arguments(), ["--model", "opus"]);
        assert_eq!(pinned[1].benchmark(), None);
        assert_eq!(
            pinned[3].arguments(),
            ["--model", "claude-opus-5-5", "--effort", "high"]
        );
        assert_eq!(
            pinned[3].benchmark().unwrap().models_free_slug(),
            "claude-opus-5-5-high"
        );
        let max = claude(Some("max")).swap_remove(3);
        assert_eq!(
            max.arguments(),
            ["--model", "claude-opus-5-5", "--effort", "max"]
        );
        assert_eq!(max.benchmark(), None);
        let unset = claude(None).swap_remove(3);
        assert_eq!(unset.arguments(), ["--model", "claude-opus-5-5"]);
        assert_eq!(unset.label(), "claude-opus-5-5 AA default setting");
        for variant in [
            pin("claude/work", "claude-opus-5-5-high", None),
            pin("claude/work", "claude-opus-5-5-high", Some("high")),
            pin("claude/work", "opus", None),
        ] {
            let dispatched = configurations(Provider::Claude, Some(&variant));
            assert_eq!(dispatched.len(), 3, "{variant:?}");
            assert!(dispatched.iter().all(|configuration| !configuration
                .arguments()
                .iter()
                .any(|argument| argument.starts_with("claude-opus-5-5"))));
        }
        let future = configurations(
            Provider::Claude,
            Some(&pin("claude/work", "claude-opus-6-1", Some("xhigh"))),
        )
        .swap_remove(3);
        assert_eq!(
            future.arguments(),
            ["--model", "claude-opus-6-1", "--effort", "xhigh"]
        );
        assert_eq!(
            future.benchmark().unwrap().models_free_slug(),
            "claude-opus-6-1-xhigh"
        );
        let codex = configurations(
            Provider::Codex,
            Some(&pin("codex/work", "gpt-6.2-sol", Some("high"))),
        );
        assert_eq!(codex.len(), 2);
        assert!(codex[0].arguments().is_empty());
        assert_eq!(
            codex[1].arguments(),
            [
                "-c",
                "model='gpt-6.2-sol'",
                "-c",
                "model_reasoning_effort='high'"
            ]
        );
        assert_eq!(codex[1].benchmark().unwrap().slug, "gpt-6-2-sol");
        let gemini = configurations(
            Provider::Gemini,
            Some(&pin("gemini/work", "gemini-3.8-flash", Some("xhigh"))),
        );
        assert_eq!(gemini.len(), 2);
        assert!(gemini[0].arguments().is_empty());
        assert_eq!(
            gemini[1].arguments(),
            ["--model", "gemini-3.8-flash", "--effort", "xhigh"]
        );
        assert_eq!(
            gemini[1].benchmark().unwrap().models_free_slug(),
            "gemini-3-8-flash-xhigh"
        );
        assert_eq!(
            configurations(
                Provider::Codex,
                Some(&pin("claude/work", "claude-opus-5-5", None))
            )
            .len(),
            1
        );
        assert_eq!(configurations(Provider::Claude, None).len(), 3);
    }
}

#[cfg(test)]
mod cross_platform_tests {
    use super::*;

    #[test]
    fn gemini_configurations_emit_model_and_effort_flags() {
        let make_pin = |model: &str, effort: Option<&str>| Pin {
            profile: "gemini/default".into(),
            model: model.into(),
            effort: effort.map(str::to_owned),
        };
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let list = configurations(
                Provider::Gemini,
                Some(&make_pin("gemini-3.8-flash", Some(effort))),
            );
            assert_eq!(list.len(), 2);
            assert!(list[0].arguments().is_empty());
            assert_eq!(
                list[1].arguments(),
                ["--model", "gemini-3.8-flash", "--effort", effort]
            );
        }
        let unset = configurations(Provider::Gemini, Some(&make_pin("gemini-3.8-flash", None)));
        assert_eq!(unset.len(), 2);
        assert_eq!(unset[1].arguments(), ["--model", "gemini-3.8-flash"]);
        let refused = configurations(
            Provider::Gemini,
            Some(&make_pin("gemini-3.8-flash", Some("minimal"))),
        );
        assert_eq!(refused.len(), 1);
    }

    fn test_candidate(
        profile: usize,
        label: &str,
        remaining: Option<f64>,
        rate: Option<f64>,
        low_buffer: bool,
    ) -> Candidate {
        Candidate {
            profile,
            label: label.into(),
            configuration: Configuration::provider_default(Provider::Claude),
            metrics: Metrics::default(),
            metric: None,
            headroom: Headroom {
                rate,
                remaining,
                low_buffer,
            },
        }
    }

    #[test]
    fn describe_floors_remaining_headroom_percentage() {
        let cases = [
            (Some(0.9), "0% left"),
            (Some(1.4), "1% left"),
            (Some(23.9), "23% left"),
            (Some(100.0), "100% left"),
            (None, "headroom unknown"),
        ];
        for (remaining, expected) in cases {
            let candidate = test_candidate(0, "claude/main", remaining, remaining, false);
            assert!(
                describe(&candidate).ends_with(expected),
                "remaining={remaining:?} described as {}",
                describe(&candidate)
            );
        }
    }

    #[test]
    fn rank_deprioritizes_low_buffer_profiles_below_healthy_and_unknown_but_keeps_last_resort() {
        let candidates = vec![
            test_candidate(0, "claude/main-1pct", Some(1.0), Some(0.05), true),
            test_candidate(1, "claude/low-4pct", Some(4.0), Some(0.01), true),
            test_candidate(2, "claude/unknown", None, None, false),
            test_candidate(3, "claude/default-23pct", Some(23.0), Some(0.001), false),
        ];
        let expected = [
            "claude/default-23pct",
            "claude/unknown",
            "claude/low-4pct",
            "claude/main-1pct",
        ];
        for policy in [Policy::Default, Policy::Ordinary] {
            let ranked = rank(candidates.clone(), policy, Difficulty::Any).unwrap();
            let labels: Vec<&str> = ranked.iter().map(|c| c.label.as_str()).collect();
            assert_eq!(labels, expected, "policy={policy:?}");

            let reversed: Vec<Candidate> = candidates.iter().cloned().rev().collect();
            let ranked_rev = rank(reversed, policy, Difficulty::Any).unwrap();
            let labels_rev: Vec<&str> = ranked_rev.iter().map(|c| c.label.as_str()).collect();
            assert_eq!(labels_rev, expected, "reversed policy={policy:?}");
        }

        let only_low = vec![test_candidate(
            0,
            "claude/standby",
            Some(3.0),
            Some(0.001),
            true,
        )];
        let choice = choose(
            rank(only_low, Policy::Default, Difficulty::Any),
            &[],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(choice.candidate.label, "claude/standby");
    }

    #[test]
    fn classify_task_identifies_all_work_classes() {
        assert_eq!(
            classify_task("Architecture plan for v0.3", "", false, false, None).work_class,
            WorkClass::HardDesign
        );
        assert_eq!(
            classify_task("Fix flaky parser crash", "", false, false, None).work_class,
            WorkClass::Debugging
        );
        assert_eq!(
            classify_task("Rename helper and fix typo", "", false, false, None).work_class,
            WorkClass::BoundedMechanical
        );
        assert_eq!(
            classify_task("Add inbox persistence", "", true, false, None).work_class,
            WorkClass::Verification
        );
        assert_eq!(
            classify_task("Add inbox persistence", "", false, false, None).work_class,
            WorkClass::Implementation
        );
        assert_eq!(
            classify_task("AC-TASK-NR1", "", false, false, None).work_class,
            WorkClass::Pinned
        );
        assert_eq!(
            classify_task("Add feature", "", false, true, None).work_class,
            WorkClass::Pinned
        );
    }

    #[test]
    fn rank_for_work_class_uses_aa_components_and_bounded_mechanical_speed_tiebreaker() {
        let make =
            |label: &str, fc: f64, tb: f64, qna: f64, coding: f64, sec_per_task: f64| Candidate {
                profile: label.len(),
                label: label.into(),
                configuration: Configuration::provider_default(Provider::Codex),
                metrics: Metrics {
                    coding_agent_index: Some(coding),
                    coding_index: Some(coding),
                    intelligence_index: Some(coding),
                    speed: Some(crate::catalog::Speed {
                        unit: SpeedUnit::SecondsPerTask,
                        value: sec_per_task,
                    }),
                    components: crate::catalog::ComponentScores {
                        frontiercode: Some(fc),
                        terminal_bench: Some(tb),
                        swe_atlas_qna: Some(qna),
                    },
                },
                metric: None,
                headroom: Headroom {
                    rate: Some(1.0),
                    remaining: Some(80.0),
                    low_buffer: false,
                },
            };
        let pool = vec![
            make("codex/builder", 84.0, 62.0, 70.0, 78.0, 180.0),
            make("claude/debugger", 74.0, 88.0, 72.0, 75.0, 40.0),
            make("gemini/verifier", 70.0, 65.0, 91.0, 74.0, 90.0),
        ];

        let impl_rank = rank_for_work_class(
            pool.clone(),
            Policy::Default,
            Difficulty::Any,
            WorkClass::Implementation,
        )
        .unwrap();
        assert_eq!(impl_rank[0].label, "codex/builder");

        let debug_rank = rank_for_work_class(
            pool.clone(),
            Policy::Default,
            Difficulty::Any,
            WorkClass::Debugging,
        )
        .unwrap();
        assert_eq!(debug_rank[0].label, "claude/debugger");

        let verify_rank = rank_for_work_class(
            pool.clone(),
            Policy::Default,
            Difficulty::Any,
            WorkClass::Verification,
        )
        .unwrap();
        assert_eq!(verify_rank[0].label, "gemini/verifier");

        let mech_rank = rank_for_work_class(
            pool,
            Policy::Default,
            Difficulty::Any,
            WorkClass::BoundedMechanical,
        )
        .unwrap();
        assert_eq!(mech_rank[0].label, "claude/debugger");
    }

    #[test]
    fn quota_pacer_caps_burst_and_respects_urgent_reserve_and_persistence() {
        let dir = std::env::temp_dir().join(format!(
            "agent-command-pacer-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let mut pacer = QuotaPacer::default();
        let window = WindowAllowance {
            window_id: "five_hour".into(),
            remaining: 100.0,
            reserved: 0.0,
            reset_at: 2_000,
        };
        let initial = pacer
            .effective_remaining("codex/main", &window, Some(20.0), &[], 10.0, false, 1_000)
            .unwrap();
        assert!((initial - 10.0).abs() < 1e-6);

        let initial_urgent = pacer
            .effective_remaining("codex/main", &window, Some(20.0), &[], 10.0, true, 1_000)
            .unwrap();
        assert!((initial_urgent - 20.0).abs() < 1e-6);

        let holds = vec![ActiveUsageHold {
            profile: "codex/main".into(),
            window_id: "five_hour".into(),
            reset_at: 2_000,
            created_at: 1_100,
            usage_units: 15.0,
        }];
        let halfway = pacer
            .effective_remaining(
                "codex/main",
                &window,
                Some(20.0),
                &holds,
                10.0,
                false,
                1_500,
            )
            .unwrap();
        assert!((halfway - 35.0).abs() < 1e-6);

        assert_eq!(
            pacer.effective_remaining("codex/main", &window, Some(20.0), &[], 10.0, false, 2_000),
            None
        );
        assert!(burn_score(40.0, 2_000, 1_000) > burn_score(40.0, 5_000, 1_000));

        pacer.save(&dir).unwrap();
        let reloaded = QuotaPacer::load(&dir).unwrap();
        assert_eq!(reloaded, pacer);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pick_next_task_enforces_priority_and_backlog_weighted_fair_queue() {
        let tasks = vec![
            QueuedTask {
                id: "n-a1".into(),
                project: "alpha".into(),
                priority: QueuePriority::Normal,
                created_at: 10,
                blocked: false,
            },
            QueuedTask {
                id: "n-a2".into(),
                project: "alpha".into(),
                priority: QueuePriority::Normal,
                created_at: 11,
                blocked: false,
            },
            QueuedTask {
                id: "n-a3".into(),
                project: "alpha".into(),
                priority: QueuePriority::Normal,
                created_at: 12,
                blocked: false,
            },
            QueuedTask {
                id: "n-a4".into(),
                project: "alpha".into(),
                priority: QueuePriority::Normal,
                created_at: 13,
                blocked: false,
            },
            QueuedTask {
                id: "n-b1".into(),
                project: "beta".into(),
                priority: QueuePriority::Normal,
                created_at: 5,
                blocked: false,
            },
            QueuedTask {
                id: "blocked-u".into(),
                project: "gamma".into(),
                priority: QueuePriority::Urgent,
                created_at: 1,
                blocked: true,
            },
        ];
        let mut served = HashMap::new();
        served.insert("alpha".to_owned(), 2);
        served.insert("beta".to_owned(), 2);
        assert_eq!(pick_next_task(&tasks, &served).unwrap().id, "n-a1");

        let mut with_urgent = tasks.clone();
        with_urgent.push(QueuedTask {
            id: "u-1".into(),
            project: "beta".into(),
            priority: QueuePriority::Urgent,
            created_at: 50,
            blocked: false,
        });
        with_urgent.push(QueuedTask {
            id: "next-1".into(),
            project: "beta".into(),
            priority: QueuePriority::Next,
            created_at: 20,
            blocked: false,
        });
        assert_eq!(pick_next_task(&with_urgent, &served).unwrap().id, "u-1");
        with_urgent.retain(|t| t.id != "u-1");
        assert_eq!(pick_next_task(&with_urgent, &served).unwrap().id, "next-1");
    }
}
