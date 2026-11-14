# Agent Command

## What we're working on

A Rust terminal interface for managing agents: a selectable crew roster, mission controls, command input, and an activity feed. The current prototype uses sample agents; commands stay local.

The interface retains its existing layout, block-letter backdrop, muted blue palette, ivory menu bar, and small color accent.

## Starter terminal

The current starter screen with sample agents and local mission controls:

![Agent Command starter terminal with crew, chart room, and ship's log](docs/images/starter-terminal.png)

The header and palette in detail:

![Agent Command header with ivory lettering and muted blue accents](docs/images/starter-terminal-header.png)

## Install and open

With Rust/Cargo 1.99 or newer installed, run this one command on Windows, Linux, or macOS:

```sh
cargo install --locked --git https://github.com/Agent-Commander/Agent-Command --branch main --bin agent-command
```

Then open the terminal UI from any directory:

```sh
agent-command
```

Cargo installs into its `bin` directory (normally `~/.cargo/bin`), which must be on your PATH. The executable is named `agent-command` so it can coexist with tools named `agent`. The installed UI includes its default palette and does not need a source checkout. Optional theme overrides use `AGENT_COMMAND_THEME`, or `agent-command/theme.toml` under `XDG_CONFIG_HOME`, Windows `APPDATA`, or `~/.config`.

From a source checkout, `cargo run --locked` also opens the UI.

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
