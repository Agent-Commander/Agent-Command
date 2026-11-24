# CLAUDE.md

Agent Command: a Rust terminal UI for Commander and on-demand specialists on subscription profiles.

Read first: [TEAM.md](TEAM.md) for roles, the 7-field `BRIEF`, gates and protected paths; [REVIEW.md](REVIEW.md) and [.review-rules/](.review-rules/) for review rules. [AGENTS.md](AGENTS.md) has the full module layout.

## Commands

```sh
cargo run --locked
cargo run --locked --bin dev
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Run all three gates before every push. CI repeats them on Linux, macOS and Windows; see [.github/CI-STANDARDS.md](.github/CI-STANDARDS.md) before editing workflows.

## Layout

- `src/`: all Rust source. `lib.rs` is the app core; one module per concern (`auth`, `mission`, `usage`, `checkpoint`, `catalog`, `routing`, `specialists`, `panel`, `token_burn`).
- `tests/`: integration and terminal layout tests.
- `docs/`: routing and terminal check references.

## Constraints

- Rust only; no Python runtime.
- No explanatory source comments in code you write or edit.
- Keep `sub-auth` profile state isolated; tests use a temporary `SUB_AUTH_DIR`.
- No network calls in unit tests.
- No secrets in logs, checkpoints or process arguments.
- No synthetic Claude prompts to probe usage.
- Do not touch protected paths from `TEAM.md` unless the `BRIEF` names them.
