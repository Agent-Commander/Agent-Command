use std::fs;
use std::path::Path;

const REVIEW_RULE_FILES: [&str; 3] = [
    ".review-rules/rust-architecture.md",
    ".review-rules/tui-invariants.md",
    ".review-rules/auth-and-checkpoints.md",
];

const BRIEF_FIELDS: [&str; 7] = [
    "BRIEF FOR:",
    "ROLE:",
    "GOAL:",
    "CONTEXT:",
    "DONE WHEN:",
    "DO NOT:",
    "RETURN:",
];

const GATES: [&str; 3] = [
    "cargo fmt --check",
    "cargo clippy --locked --all-targets -- -D warnings",
    "cargo test --locked --all-targets",
];

fn read_repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{relative}: {error}"))
}

fn assert_contains_all(relative: &str, needles: &[&str]) {
    let text = read_repo_file(relative);
    for needle in needles {
        assert!(text.contains(needle), "{relative} is missing {needle:?}");
    }
}

#[test]
fn team_charter_defines_the_seven_brief_fields_in_order() {
    let team = read_repo_file("TEAM.md");
    let positions: Vec<usize> = BRIEF_FIELDS
        .iter()
        .map(|field| {
            team.find(field)
                .unwrap_or_else(|| panic!("TEAM.md is missing {field:?}"))
        })
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn team_charter_names_roles_gates_and_protected_paths() {
    assert_contains_all(
        "TEAM.md",
        &[
            "Commander / Lead",
            "Parallel Specialist",
            "Cross-Family Read-Only Playtester / Reviewer",
            "3 review rounds",
            "Protected paths",
        ],
    );
    assert_contains_all("TEAM.md", &GATES);
}

#[test]
fn agent_guides_point_to_the_charter_and_review_rules() {
    for guide in ["AGENTS.md", "CLAUDE.md"] {
        assert_contains_all(guide, &["TEAM.md", "REVIEW.md", ".review-rules/", "src/"]);
        assert_contains_all(guide, &GATES);
    }
}

#[test]
fn review_guide_links_every_rule_file_that_exists() {
    for rule_file in REVIEW_RULE_FILES {
        assert_contains_all("REVIEW.md", &[rule_file]);
        assert!(!read_repo_file(rule_file).trim().is_empty());
    }
}

#[test]
fn tui_invariants_show_bad_and_good_examples() {
    assert_contains_all(
        ".review-rules/tui-invariants.md",
        &["80x24", "120x40", "BAD", "GOOD", "muted"],
    );
}
