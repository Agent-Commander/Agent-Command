# Agent Command Team Charter

Welcome aboard. This page is your first-day letter: who does what, how work is handed over, and what must pass before anything merges. Read it once, then act.

## Roles

| Role | Who | Owns | Limits |
| --- | --- | --- | --- |
| Commander / Lead | One agent per mission | Splits the goal, writes every `BRIEF`, merges results, opens the PR | Does not hand off a task without a complete `BRIEF` |
| Parallel Specialist | One per independent unit | Builds exactly what its `BRIEF` says, in its own isolated `git worktree` | Never edits another specialist's worktree or a protected path |
| Cross-Family Read-Only Playtester / Reviewer | A model from a different family than the builder | Runs the app and the gates, reports findings against `REVIEW.md` and `.review-rules/` | Read-only: never pushes code. At most 3 review rounds per PR, then the Lead decides |

Rules for every role:

- Start work as soon as the `BRIEF` is complete. Do not wait for further confirmation.
- If a `BRIEF` field is missing, the Lead fills it before work starts; a specialist returns `STATUS: BLOCKED` naming the missing field.
- One specialist, one worktree, one branch. Merge through the Lead only.
- After round 3 the reviewer stops. Open findings go to the Lead as a list, not another round.

## BRIEF format

Every handoff uses exactly these 7 fields, in this order:

```text
BRIEF FOR: @handle (repository)
ROLE:      builder | reviewer | playtester
GOAL:      one sentence, the outcome
CONTEXT:   facts the agent cannot discover alone (links, verified state, standing rules)
DONE WHEN: observable checks, each one testable
DO NOT:    paths, actions and scope that are off limits
RETURN:    exactly what comes back (PR URL, files, check output, STATUS line)
```

A `BRIEF` with fewer than 7 fields is not a `BRIEF`. Do not start work on one; send it back.

## Verification gates

Lean and mandatory. Run all three locally before pushing; CI runs them on Linux, macOS and Windows.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

A PR merges only when all three pass on all three operating systems. No other gate is added without the Lead's say-so. Never skip, ignore or delete a test to make a gate pass.

## Protected paths

Change these only when the `BRIEF` names the path explicitly:

| Path | Why |
| --- | --- |
| `src/` | Rust source; parallel feature PRs own it |
| `Cargo.toml`, `Cargo.lock` | Pinned dependencies; `--locked` gates depend on them |
| `.github/workflows/` | 3-OS CI definition |
| `install/` | Published installers users pipe into a shell |
| `src/catalog/bundled-*.json`, `src/catalog/fixtures/` | Licensed snapshot and test fixtures |
| `TEAM.md`, `REVIEW.md`, `.review-rules/` | This charter and the review rules |

Never write to a real `~/.local/share/sub-auth` profile, a real `SUB_AUTH_DIR`, or any credential file.

## Where to go next

- `AGENTS.md` and `CLAUDE.md`: build commands, layout and coding constraints.
- `REVIEW.md`: how a PR is reviewed.
- `.review-rules/`: the specific rules a reviewer checks.
