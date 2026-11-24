# CI standards

CI must stay fast and cheap as Agent Command grows. Every workflow change follows these rules.

## Gates that never change

- `cargo fmt --check` runs once, on Linux. Formatting does not depend on the OS.
- `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked --all-targets` run on Linux, macOS and Windows for every code PR and every push to `main`.
- `--locked` stays on every Cargo command.

## Triggers

- `push` runs only on `main`. Feature branches get their checks from `pull_request`, so a branch push never runs the matrix twice.
- Docs-only changes (`**.md`, `docs/**`) skip Rust checks. `README.md` is the exception: a test reads it with `include_str!`. Any new `include_str!` or `include_bytes!` of a doc must be added back to the `paths` filter.
- `main` has no required checks today. If branch protection adds them, keep a fast docs-only pass so docs PRs never wait on a check that never starts.
- `workflow_dispatch` stays available for manual reruns.

## Cost

- Every workflow has a `concurrency` group. PR runs use `cancel-in-progress`; `main` and tag runs never cancel.
- Every Rust job uses `Swatinem/rust-cache@v2` with a key per OS or target. Only `main` saves the test cache, so PRs restore it without writing new entries.
- No duplicate compiles. Smoke tests reuse the binary the job already built; release-mode builds happen only in `release.yml`.
- macOS minutes cost about 10x Linux and Windows about 2x. Put OS-independent work on `ubuntu-latest`.
- Slow end-to-end or platform proofs (installers, release packaging) run on tags or on PRs that touch their paths, not on every push.

## Scaling triggers

- When the repository becomes a Cargo workspace, select tests by affected crate instead of testing everything.
- When `cargo test` passes about 3 minutes on one OS, measure per-test times and pack shards by measured duration, not by file count.
- Gate the release `publish` job on `!cancelled()` and a successful build.

## Audits

Run the `ci-speedup` skill from `starslingdev/skills` against the workflows at least once per release cycle, and after any change that adds a job, an OS or a target. Record findings in the PR that applies them.
