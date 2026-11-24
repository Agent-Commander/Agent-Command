# AGENTS.md

Agent Command is a Rust terminal UI (Ratatui + Crossterm) that runs Commander and on-demand specialists on subscription profiles.

Read first: [TEAM.md](TEAM.md) (roles, `BRIEF` format, gates, protected paths), [REVIEW.md](REVIEW.md), and [.review-rules/](.review-rules/).

## Build and test

```sh
cargo run --locked                                   # open the TUI
cargo run --locked --bin dev                         # live-reload UI development
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

All three gates must pass before you push. CI runs them on Linux, macOS and Windows; workflow rules live in [.github/CI-STANDARDS.md](.github/CI-STANDARDS.md).

## Layout

| Path | Contents |
| --- | --- |
| `src/main.rs`, `src/lib.rs` | Entry point and app core |
| `src/cli.rs` | Command-line parsing |
| `src/panel.rs`, `src/token_burn.rs` | TUI panels and the Token Burn deck |
| `src/auth.rs`, `src/auth/` | Isolated `sub-auth` profiles, Unix and Windows permissions |
| `src/mission.rs`, `src/native.rs` | Mission runs and native CLI streams |
| `src/usage.rs`, `src/checkpoint.rs` | Usage latches and SHA-256 continuation checkpoints |
| `src/catalog.rs`, `src/routing.rs` | Model catalog and routing |
| `src/specialists.rs` | Specialist roster and worktrees |
| `src/settings.rs`, `src/update.rs`, `src/install.rs` | Settings, update check, CLI installers |
| `src/bin/dev.rs` | Development supervisor |
| `tests/` | Integration and terminal layout tests |
| `docs/` | Routing and terminal check references |

## Coding constraints

- Rust only. No Python, Node or shell runtime required to build, test or run.
- No explanatory source comments. Use clear names, small functions, explicit types and tests instead.
- Profile state stays isolated under `sub-auth` (`~/.local/share/sub-auth` or an absolute `SUB_AUTH_DIR`). Tests use a temporary root, never a real profile.
- No network calls in unit tests. Use the fixtures in `src/catalog/fixtures/` or local fakes.
- Never log, checkpoint or pass on the command line a token, key or credential.
- Paths and permissions must work on Windows, macOS and Linux.
- Do not edit protected paths listed in `TEAM.md` unless your `BRIEF` names them.
