# Agent Command

## What we're working on

A Rust terminal interface for managing agents: a selectable crew roster, mission controls, command input, and an activity feed. The current prototype uses sample agents; commands stay local.

The interface retains its existing layout, block-letter backdrop, muted blue palette, ivory menu bar, and small color accent.

## Starter terminal

The current starter screen with sample agents and local mission controls:

![Agent Command starter terminal with crew, chart room, and ship's log](docs/images/starter-terminal.png)

The header and palette in detail:

![Agent Command header with ivory lettering and muted blue accents](docs/images/starter-terminal-header.png)

## Run

Install a current stable Rust toolchain (Rust 1.99 or newer). On Windows, Linux, and macOS:

```sh
cargo run --locked
```

Use a Unicode terminal at least 120 columns by 40 rows for the complete layout. Smaller terminals have a compact title.

## UI development

```sh
cargo run --locked --bin dev
```

Save `theme.toml` to refresh colors in place. Invalid theme edits keep the last working palette. Save Rust source to rebuild and restart automatically; demo state resets after a rebuild. The development supervisor and application are both Rust.

Controls: arrows select an agent, Tab changes focus, Ctrl+N focuses command input, Enter sends a local demo command, Ctrl+P pauses/resumes, Ctrl+L clears the feed, and Ctrl+Q exits. The roster and action buttons also accept mouse clicks. Menu-bar labels remain decorative.

## Validation

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

CI checks Windows, Linux, and macOS. No Python runtime is required.
