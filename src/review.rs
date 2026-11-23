use crate::{
    auth::{Profile, Provider},
    catalog::Catalog,
    mission::{Event, LimitPolicy, Mission, Route},
    native::Pins,
    permissions::PermissionMode,
    routing::Request,
    specialists::Member,
    targeting::Target,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub(crate) const MAX_REVIEW_ROUNDS: u8 = 3;
pub(crate) const REVIEW_FILE: &str = "REVIEW.md";
pub(crate) const RULES_DIRECTORY: &str = ".review-rules";
pub(crate) const MAX_RULE_FILES: usize = 32;
pub(crate) const MAX_RULE_BYTES: u64 = 64 * 1024;
const RULE_EXTENSION: &str = "md";

pub(crate) const PLAYTESTER_CHECKLIST: &str = "Adversarial Playtester checklist — probe each edge case and report only what you can reproduce:\n\
- empty input and huge input\n\
- concurrent use and interleaved operations\n\
- slow or dropped network\n\
- Unicode: wide, combining, right-to-left and emoji text\n\
- expired or revoked authentication\n\
- missing file or OS permissions\n\
Report every finding as:\n\
FINDING: <title>\n\
REPRO: <exact steps>\n\
EXPECTED: <behaviour>\n\
ACTUAL: <behaviour>\n\
SEVERITY: <1 blocker | 2 major | 3 minor>\n\
Finish with one line: VERDICT: APPROVE or VERDICT: CHANGES REQUESTED.";

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewLedger {
    #[serde(default)]
    pub(crate) author: Option<Provider>,
    #[serde(default)]
    pub(crate) revision: u32,
    #[serde(default)]
    pub(crate) rounds: u8,
}

impl ReviewLedger {
    pub(crate) fn record_author(&mut self, provider: Provider) {
        self.author = Some(provider);
    }

    pub(crate) fn next_round(&self) -> Result<u8, String> {
        if self.rounds >= MAX_REVIEW_ROUNDS {
            return Err(format!(
                "Review cap reached: {MAX_REVIEW_ROUNDS} of {MAX_REVIEW_ROUNDS} rounds used on revision {}. Operator decision required: accept the work, or start a new revision with --next-revision.",
                self.revision
            ));
        }
        Ok(self.rounds + 1)
    }

    pub(crate) fn begin_round(&mut self) -> Result<u8, String> {
        let round = self.next_round()?;
        self.rounds = round;
        Ok(round)
    }

    pub(crate) fn next_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.rounds = 0;
    }
}

pub(crate) fn opposite(provider: Provider) -> Provider {
    match provider {
        Provider::Claude => Provider::Codex,
        Provider::Codex => Provider::Claude,
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ReviewerSelection {
    pub(crate) target: Target,
    pub(crate) cross_family: bool,
    pub(crate) note: String,
}

pub(crate) fn select_reviewer(
    name: &str,
    author: Option<Provider>,
    signed_in: &[Provider],
) -> Result<ReviewerSelection, String> {
    let author = author.ok_or_else(|| {
        format!("{name} has no recorded changes yet; delegate work before /review.")
    })?;
    let reviewer = opposite(author);
    if signed_in.contains(&reviewer) {
        return Ok(ReviewerSelection {
            target: Target::Provider(reviewer),
            cross_family: true,
            note: format!(
                "Cross-family review: {} wrote it, {} reviews it",
                author.binary(),
                reviewer.binary()
            ),
        });
    }
    if signed_in.contains(&author) {
        return Ok(ReviewerSelection {
            target: Target::Provider(author),
            cross_family: false,
            note: format!(
                "Fallback: no {} profile is signed in, so {} reviews its own family's work",
                reviewer.binary(),
                author.binary()
            ),
        });
    }
    Err("No signed-in profile can review. Connect a provider first.".into())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewRule {
    pub(crate) name: String,
    pub(crate) text: String,
}

pub(crate) fn discover_rules(root: &Path) -> io::Result<Vec<ReviewRule>> {
    let mut rules = Vec::new();
    if let Some(rule) = read_rule(&root.join(REVIEW_FILE), REVIEW_FILE.into())? {
        rules.push(rule);
    }
    for path in rule_paths(&root.join(RULES_DIRECTORY))? {
        let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(rule) = read_rule(&path, format!("{RULES_DIRECTORY}/{file}"))? {
            rules.push(rule);
        }
    }
    Ok(rules)
}

fn rule_paths(directory: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() && is_markdown(&entry.path()) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths.truncate(MAX_RULE_FILES);
    Ok(paths)
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(RULE_EXTENSION))
}

fn read_rule(path: &Path, name: String) -> io::Result<Option<ReviewRule>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_RULE_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    Ok((!text.is_empty()).then_some(ReviewRule { name, text }))
}

pub(crate) struct ReviewBrief<'a> {
    pub(crate) specialist: &'a str,
    pub(crate) role: &'a str,
    pub(crate) focus: Option<&'a str>,
    pub(crate) round: u8,
    pub(crate) revision: u32,
    pub(crate) rules: &'a [ReviewRule],
}

pub(crate) fn review_prompt(brief: &ReviewBrief<'_>) -> String {
    let rules = if brief.rules.is_empty() {
        "- none found".to_owned()
    } else {
        brief
            .rules
            .iter()
            .map(|rule| format!("## {}\n{}", rule.name, rule.text))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    format!(
        "You are an adversarial read-only reviewer for Agent Command. Review the changes {} ({}) made in the current directory. You cannot edit files; do not try.\n\
         Review round {} of {MAX_REVIEW_ROUNDS} for revision {}.\n\
         Focus: {}\n\
         Project review rules:\n{rules}\n\
         {PLAYTESTER_CHECKLIST}",
        brief.specialist,
        brief.role,
        brief.round,
        brief.revision,
        brief.focus.unwrap_or("all changes"),
    )
}

pub(crate) fn signed_in_providers(profiles: &[Profile]) -> Vec<Provider> {
    Provider::ALL
        .into_iter()
        .filter(|provider| {
            profiles.iter().any(|profile| {
                profile.provider == *provider && profile.authenticated_command().is_ok()
            })
        })
        .collect()
}

pub(crate) fn first_rules(roots: &[&Path]) -> io::Result<Vec<ReviewRule>> {
    for root in roots {
        let rules = discover_rules(root)?;
        if !rules.is_empty() {
            return Ok(rules);
        }
    }
    Ok(Vec::new())
}

pub(crate) struct ReviewRequest<'a> {
    pub(crate) focus: Option<&'a str>,
    pub(crate) next_revision: bool,
    pub(crate) signed_in: &'a [Provider],
    pub(crate) rule_roots: &'a [&'a Path],
}

#[derive(Debug)]
pub(crate) struct ReviewPlan {
    pub(crate) target: Target,
    pub(crate) note: String,
    pub(crate) round: u8,
    pub(crate) prompt: String,
}

pub(crate) fn plan(member: &mut Member, request: &ReviewRequest<'_>) -> Result<ReviewPlan, String> {
    if member.is_commander() {
        return Err("Commander is not reviewed; /review a specialist.".into());
    }
    if request.next_revision {
        member.review.next_revision();
    }
    let selection = select_reviewer(&member.name, member.review.author, request.signed_in)?;
    member.review.next_round()?;
    let rules = first_rules(request.rule_roots)
        .map_err(|error| format!("Cannot read review rules: {error}"))?;
    let round = member.review.begin_round()?;
    let prompt = review_prompt(&ReviewBrief {
        specialist: &member.name,
        role: &member.role,
        focus: request.focus,
        round,
        revision: member.review.revision,
        rules: &rules,
    });
    Ok(ReviewPlan {
        target: selection.target,
        note: selection.note,
        round,
        prompt,
    })
}

pub(crate) fn review_route(plan: &ReviewPlan, catalog: Catalog, pins: Pins) -> Route {
    Route {
        request: Request {
            units: vec![plan.prompt.clone()],
            ..Request::default()
        },
        preferred: None,
        catalog,
        pins,
        target: Some(plan.target),
        mode: PermissionMode::ReadOnly,
        limits: LimitPolicy::default(),
    }
}

pub(crate) fn run_to_completion(
    profiles: Vec<Profile>,
    route: Route,
    workspace: PathBuf,
    out: &mut impl Write,
) -> io::Result<()> {
    let mission = Mission::launch(profiles, route, workspace);
    for event in mission.events.iter() {
        match event {
            Event::Running {
                reason: Some(reason),
                ..
            } => writeln!(out, "{reason}")?,
            Event::Running { reason: None, .. }
            | Event::Continuing { .. }
            | Event::Snoozed { .. } => {}
            Event::Output(text) => writeln!(out, "{text}")?,
            Event::Finished(result) => return result,
        }
    }
    Err(io::Error::other("Review worker stopped"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("ac-review-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
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
    fn claude_work_is_reviewed_by_codex_and_codex_work_by_claude() {
        let both = [Provider::Claude, Provider::Codex];
        let selection = select_reviewer("nova", Some(Provider::Claude), &both).unwrap();
        assert_eq!(selection.target, Target::Provider(Provider::Codex));
        assert!(selection.cross_family);
        assert_eq!(
            selection.note,
            "Cross-family review: claude wrote it, codex reviews it"
        );
        let selection = select_reviewer("nova", Some(Provider::Codex), &both).unwrap();
        assert_eq!(selection.target, Target::Provider(Provider::Claude));
        assert!(selection.cross_family);
    }

    #[test]
    fn single_family_falls_back_with_a_visible_note_and_unknown_author_fails() {
        let selection =
            select_reviewer("nova", Some(Provider::Claude), &[Provider::Claude]).unwrap();
        assert_eq!(selection.target, Target::Provider(Provider::Claude));
        assert!(!selection.cross_family);
        assert!(selection.note.starts_with("Fallback: no codex profile"));
        assert!(select_reviewer("nova", None, &[Provider::Codex])
            .unwrap_err()
            .contains("no recorded changes"));
        assert!(select_reviewer("nova", Some(Provider::Codex), &[])
            .unwrap_err()
            .contains("No signed-in profile"));
    }

    #[test]
    fn three_rounds_are_allowed_and_the_fourth_needs_the_operator() {
        let mut ledger = ReviewLedger::default();
        assert_eq!(ledger.begin_round(), Ok(1));
        assert_eq!(ledger.begin_round(), Ok(2));
        assert_eq!(ledger.begin_round(), Ok(3));
        let refusal = ledger.begin_round().unwrap_err();
        assert!(refusal.starts_with("Review cap reached: 3 of 3 rounds used on revision 0."));
        assert!(refusal.contains("Operator decision required"));
        assert_eq!(ledger.rounds, MAX_REVIEW_ROUNDS);
        ledger.next_revision();
        assert_eq!((ledger.revision, ledger.rounds), (1, 0));
        assert_eq!(ledger.begin_round(), Ok(1));
    }

    fn request<'a>(signed_in: &'a [Provider], roots: &'a [&'a Path]) -> ReviewRequest<'a> {
        ReviewRequest {
            focus: Some("parser"),
            next_revision: false,
            signed_in,
            rule_roots: roots,
        }
    }

    #[test]
    fn plan_consumes_a_round_only_when_a_reviewer_exists_and_stops_at_the_cap() {
        let scratch = Scratch::new("plan");
        let empty = scratch.0.join("empty");
        fs::create_dir(&empty).unwrap();
        fs::write(scratch.0.join(REVIEW_FILE), "Fallback root rules").unwrap();
        let roots = [empty.as_path(), scratch.0.as_path()];
        let mut nova = Member::new("nova", "builder", None, None);
        assert!(plan(&mut nova, &request(&[Provider::Codex], &roots))
            .unwrap_err()
            .contains("no recorded changes"));
        nova.review.record_author(Provider::Codex);
        assert!(plan(&mut nova, &request(&[], &roots)).is_err());
        assert_eq!(nova.review.rounds, 0);
        let both = [Provider::Claude, Provider::Codex];
        for round in 1..=MAX_REVIEW_ROUNDS {
            let planned = plan(&mut nova, &request(&both, &roots)).unwrap();
            assert_eq!(planned.round, round);
            assert_eq!(planned.target, Target::Provider(Provider::Claude));
            assert!(planned.prompt.contains("## REVIEW.md\nFallback root rules"));
            assert!(planned.prompt.contains("Focus: parser"));
        }
        let refusal = plan(&mut nova, &request(&both, &roots)).unwrap_err();
        assert!(refusal.starts_with("Review cap reached"));
        assert_eq!(nova.review.rounds, MAX_REVIEW_ROUNDS);
        let planned = plan(
            &mut nova,
            &ReviewRequest {
                next_revision: true,
                ..request(&both, &roots)
            },
        )
        .unwrap();
        assert_eq!((planned.round, nova.review.revision), (1, 1));
        assert!(plan(&mut Member::commander(), &request(&both, &roots)).is_err());
    }

    #[test]
    fn review_route_is_read_only_and_targets_the_selected_family() {
        let planned = ReviewPlan {
            target: Target::Provider(Provider::Codex),
            note: String::new(),
            round: 1,
            prompt: "Review it".into(),
        };
        let route = review_route(&planned, Catalog::default(), Pins::default());
        assert_eq!(route.mode, PermissionMode::ReadOnly);
        assert_eq!(route.target, Some(Target::Provider(Provider::Codex)));
        assert_eq!(route.preferred, None);
        assert_eq!(route.request.units, ["Review it"]);
    }

    #[test]
    fn ledger_round_trips_and_old_rosters_default_it() {
        let ledger = ReviewLedger {
            author: Some(Provider::Codex),
            revision: 2,
            rounds: 3,
        };
        let text = serde_json::to_string(&ledger).unwrap();
        assert_eq!(text, r#"{"author":"codex","revision":2,"rounds":3}"#);
        assert_eq!(serde_json::from_str::<ReviewLedger>(&text).unwrap(), ledger);
        assert_eq!(
            serde_json::from_str::<ReviewLedger>("{}").unwrap(),
            ReviewLedger::default()
        );
    }

    #[test]
    fn rules_load_review_md_then_sorted_markdown_rules_only() {
        let scratch = Scratch::new("rules");
        let root = &scratch.0;
        assert!(discover_rules(root).unwrap().is_empty());
        fs::write(root.join(REVIEW_FILE), "Top-level rules\n").unwrap();
        let rules = root.join(RULES_DIRECTORY);
        fs::create_dir(&rules).unwrap();
        fs::write(rules.join("b-security.md"), "No secrets").unwrap();
        fs::write(rules.join("a-style.MD"), "Small functions").unwrap();
        fs::write(rules.join("notes.txt"), "ignored").unwrap();
        fs::write(rules.join("empty.md"), "  \n").unwrap();
        fs::create_dir(rules.join("nested.md")).unwrap();
        let found = discover_rules(root).unwrap();
        let names: Vec<&str> = found.iter().map(|rule| rule.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "REVIEW.md",
                ".review-rules/a-style.MD",
                ".review-rules/b-security.md"
            ]
        );
        assert_eq!(found[0].text, "Top-level rules");
    }

    #[test]
    fn rule_files_are_bounded_in_count_and_size() {
        let scratch = Scratch::new("bounds");
        let rules = scratch.0.join(RULES_DIRECTORY);
        fs::create_dir(&rules).unwrap();
        for index in 0..MAX_RULE_FILES + 5 {
            fs::write(rules.join(format!("{index:03}.md")), "rule").unwrap();
        }
        fs::write(
            rules.join("000.md"),
            "x".repeat(MAX_RULE_BYTES as usize * 2),
        )
        .unwrap();
        let found = discover_rules(&scratch.0).unwrap();
        assert_eq!(found.len(), MAX_RULE_FILES);
        assert_eq!(found[0].text.len(), MAX_RULE_BYTES as usize);
    }

    #[test]
    fn prompt_carries_round_focus_rules_and_the_structured_checklist() {
        let rules = [ReviewRule {
            name: REVIEW_FILE.into(),
            text: "Reject unwrap in production paths".into(),
        }];
        let prompt = review_prompt(&ReviewBrief {
            specialist: "nova",
            role: "builder",
            focus: Some("auth flow"),
            round: 2,
            revision: 1,
            rules: &rules,
        });
        assert!(prompt.contains("changes nova (builder) made"));
        assert!(prompt.contains("Review round 2 of 3 for revision 1."));
        assert!(prompt.contains("Focus: auth flow"));
        assert!(prompt.contains("## REVIEW.md\nReject unwrap in production paths"));
        for needle in [
            "empty input and huge input",
            "concurrent",
            "slow or dropped network",
            "Unicode",
            "expired or revoked authentication",
            "missing file or OS permissions",
            "REPRO:",
            "EXPECTED:",
            "ACTUAL:",
            "SEVERITY: <1 blocker | 2 major | 3 minor>",
        ] {
            assert!(prompt.contains(needle), "{needle}");
        }
        let prompt = review_prompt(&ReviewBrief {
            specialist: "nova",
            role: "builder",
            focus: None,
            round: 1,
            revision: 0,
            rules: &[],
        });
        assert!(prompt.contains("Focus: all changes"));
        assert!(prompt.contains("- none found"));
    }
}
