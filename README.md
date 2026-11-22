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

No Rust, Cargo, Python, or administrator access needed. Paste the command for your system; it downloads the prebuilt Rust app, verifies its checksum, installs it for your user, and opens the terminal UI.

**macOS / Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/install.sh | sh
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/install.ps1 | iex
```

After installation, open a new terminal and run:

```sh
agent-command
```

The command is `agent-command`, so tools already named `agent` are unaffected. Installers support x64 and ARM64 Windows, Intel/Apple Silicon macOS, and x64/ARM64 Linux. Linux packages use musl to avoid a distribution-specific glibc requirement. A Unicode terminal is required.

The installers download the latest published release. Re-running the installer updates the app. Close an existing instance before updating on Windows.

Optional theme overrides use `AGENT_COMMAND_THEME`, or `agent-command/theme.toml` under `XDG_CONFIG_HOME`, Windows `APPDATA`, or `~/.config`.

### Build from source

With Rust/Cargo 1.99 or newer:

```sh
cargo install --locked --git https://github.com/Agent-Commander/Agent-Command --branch main --bin agent-command
```

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

## Subscription profiles

Install Claude Code and/or Codex CLI and put the provider executable on PATH.
Keep the provider's regular login for its standalone CLI. Agent Command uses
separate subscription profiles.

```sh
agent-command auth login claude work
agent-command auth login codex work
agent-command auth list
agent-command auth status codex work
agent-command auth run claude work -- --help
agent-command auth logout claude work
```

Profile directories use the existing sub-auth layout under
`~/.local/share/sub-auth` or an absolute `SUB_AUTH_DIR`. On Windows, use
an NTFS directory so Agent Command can enforce user-only access. The
provider handles browser sign-in and manages its own session in that profile.
Logout keeps the named profile directory and its settings.

Open `agent-command` from the workspace where the mission should run. Press
F2 or click the profile row to choose the provider and profile. Enter a
mission and press Enter or click Send. The status and assistant output appear
in the mission panel and ship's log. One mission runs at a time. Reopen the
TUI after adding a profile in another terminal.

### Subscription usage in the terminal

Use F2 or click the profile row to inspect each profile. The usage rows show
used and remaining percentages, time until reset, and observation age. Readings
become stale after 15 monotonic minutes, at the window reset, after an app
restart, or when a refreshed reading is unavailable. A wall-clock rollback
cannot make a future-dated observation fresh.

Agent Command reads Codex usage through its local app server. Readbacks for a
profile start at least five monotonic minutes apart, even after a failure.
A reopened app waits a full floor on discovering a previous probe. Probes
are skipped during a known active latch. Claude readings come from the native CLI's
structured mission stream when it reports a window. An unreported window is
unavailable. Agent Command sends no synthetic Claude prompts to collect usage.

A reported subscription limit latches the profile until the provider's reset
time. When no reliable reset is available, the profile stays latched and shows
“reset unknown.” A reported past reset requires a new reading. Usage metadata lives in the private `agent-command-usage-v1`
directory under the isolated profile root.

For new missions, a latched preferred profile can be replaced by the first
signed-in same-provider profile by name with fresh headroom in both windows.
If the provider interrupts a mission with a usage limit, Agent Command creates
a checkpoint for the root of a Git workspace. It checks the workspace again
before launching one continuation on an eligible alternate. The terminal shows
the source and destination profiles together with the reason. It stops if there
is no eligible account, the workspace changed, or a continuation also hits a limit.

A checkpoint stores a mission UUID, interrupted profile, progress counters,
and a SHA-256 digest covering HEAD, index, and tracked and non-ignored working
files. The continuation prompt adds a manifest of changed tracked paths,
untracked paths, and separate SHA-256 hashes of the staged blob and working copy when present. The app does not store
the task, transcript, file contents, or credentials in checkpoint state.
The alternate receives instructions to inspect retained work and skip
completed actions. The app cannot control the provider's decisions or verify
effects outside the workspace. A nested Git repository, submodule, symlink, or unsupported or unreadable
worktree entry stops continuation and identifies the path. This includes
special files Git leaves out of its file listing. Non-Git workspaces also
prevent continuation. Keep isolated profile state outside the worktree.

If the app closes with a pending claim, reopen the same workspace. Re-enter the
original task and select Send to resume on the claimed alternate, or use
F4 or click Abandon. The app checks the retained workspace and the alternate's
known limit windows before a user-driven resume. It does not automatically
launch a continuation on startup.

The private `agent-command-checkpoints-v1` directory tracks claimed, launched,
and abandoned continuations. A launched or abandoned claim cannot be resumed.
The launch is fenced durably immediately before dispatch. If the app exits
at that boundary, it rejects a repeat rather than risk starting two processes.
Pending claims from the previous Stage 2 revision can be abandoned. Their old digest cannot authorize a launch under the current snapshot format.

Reverting the Stage 2 commits restores Stage 1 behavior. Stage 1 ignores the
Stage 2 state directories.
