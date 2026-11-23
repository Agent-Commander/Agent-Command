use crate::{
    auth::{self, Profile, Provider},
    catalog::{Catalog, Metric, Metrics, SpeedUnit, KEY_VARIABLE},
    native::{self, Benchmark, Pin, Pins, DEFAULT_SETTING},
    usage::{Tick, Usage},
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) const STATE_DIRECTORY: &str = "agent-command-routing-v1";
const OVERRIDE_FILE: &str = "override.json";
const SPLIT_DELIMITER: &str = ";;";
const MAX_SPLIT_UNITS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Model {
    Default,
    Alias(&'static str),
    Pinned(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Configuration {
    pub(crate) provider: Provider,
    pub(crate) model: Model,
    pub(crate) effort: Option<String>,
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

    fn native_model(&self) -> Option<&str> {
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
        let mut arguments = Vec::new();
        match self.provider {
            Provider::Claude => {
                if let Some(model) = self.native_model() {
                    arguments.extend(["--model".to_owned(), model.to_owned()]);
                }
                if let Some(effort) = &self.effort {
                    arguments.extend(["--effort".to_owned(), effort.clone()]);
                }
            }
            Provider::Codex => {
                if let Some(model) = self.native_model() {
                    arguments.extend(["-c".to_owned(), format!("model=\"{model}\"")]);
                }
                if let Some(effort) = &self.effort {
                    arguments.extend([
                        "-c".to_owned(),
                        format!("model_reasoning_effort=\"{effort}\""),
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
        };
        for configuration in configurations(profile.provider, pins.get(&label)) {
            candidates.push(Candidate {
                profile: index,
                label: label.clone(),
                metrics: configuration
                    .benchmark()
                    .map(|benchmark| catalog.metrics(&benchmark))
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

fn compare_headroom(left: &Candidate, right: &Candidate) -> Ordering {
    match (left.headroom.rate, right.headroom.rate) {
        (Some(left), Some(right)) => right.total_cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
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

fn score_label(candidate: &Candidate) -> String {
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
        |remaining| format!("{remaining:.0}% left"),
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
                "model=\"gpt-6.2-sol\"",
                "-c",
                "model_reasoning_effort=\"high\""
            ]
        );
        assert_eq!(codex[1].benchmark().unwrap().slug, "gpt-6-2-sol");
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
