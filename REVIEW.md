# Review

Plain-English rules every PR is checked against. Reviewers are read-only and cross-family (see `TEAM.md`), with at most 3 rounds per PR.

## Every PR

1. All three gates pass on Linux, macOS and Windows:
   `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --all-targets`.
2. The diff matches its `BRIEF`: nothing outside `GOAL`, nothing under `DO NOT`, no protected path unless named.
3. New behavior has a test. A fixed bug has a test that fails without the fix.
4. No test is skipped, ignored or deleted to get green.
5. No explanatory source comments are added.
6. User-visible behavior changes update `README.md` or `docs/` in the same PR.

## Rule files

| File | Applies when the diff touches |
| --- | --- |
| [.review-rules/rust-architecture.md](.review-rules/rust-architecture.md) | Any Rust code |
| [.review-rules/tui-invariants.md](.review-rules/tui-invariants.md) | Rendering, input, layout, the Token Burn deck |
| [.review-rules/auth-and-checkpoints.md](.review-rules/auth-and-checkpoints.md) | Profiles, usage, routing, checkpoints, durable state |

## Reporting findings

One finding per item: file and line, the rule broken, a concrete failing scenario, and the fix. Mark each as `blocking` or `nit`. Only `blocking` findings hold a merge.

End the review with one line: `REVIEW: APPROVE`, `REVIEW: CHANGES REQUESTED`, or, after round 3, `REVIEW: ESCALATED TO LEAD`.
