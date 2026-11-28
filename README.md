# Agent Command

Agent Command is a terminal workspace for running coding agents through Claude Code, OpenAI Codex, and Antigravity CLI. Give Commander a task in your project, or delegate independent changes to specialists. In a Git repository, each specialist gets its own branch and worktree so you can inspect its changes while other work continues.

## Install

The installers download the latest release, verify its SHA-256 checksum, and install it for your user. Prebuilt binaries are available for x64 and ARM64 on macOS, Linux, and Windows. [Release downloads](https://github.com/Agent-Commander/Agent-Command/releases/latest)

**macOS or Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/install.sh | sh
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/install.ps1 | iex
```

## Run your first task

1. Open a terminal in the project you want to work on and run `agent-command`. Commander uses that directory as its workspace.
2. Use an existing Claude Code (`claude`), Codex (`codex`), or Antigravity (`agy`) login. Agent Command detects signed-in local accounts on startup. One provider is enough. To connect another account, enter `/connect` in Chat or open Settings with F3.
3. In **Chat**, enter a task such as `Find where this project parses command-line arguments and show me the relevant tests.` Press Enter and follow the streamed reply. Commander defaults to `workspace-auto`, which permits edits and shell commands. For an inspection-only session, send `/mode read-only` before the task.
4. Open **Inbox** for agent questions, approvals, and work that needs your attention. Open **Ledger** to see recorded tasks, their status, and their task documents. Alt+1, Alt+2, and Alt+3 switch between Chat, Inbox, and Ledger.

### Work with a specialist

From Commander’s chat in a Git repository, send each command as a separate message. Wait for the specialist to finish before inspecting its diff or requesting review.

```text
/spawn docs writer Add a usage example for the first public command in the project README.
/diff docs
/review docs Check that the example matches the command’s actual flags.
```

The specialist works on an `ac/` branch. `/diff docs` includes uncommitted changes. `/review docs` requests a read-only review and prefers a different provider family when one is eligible. Use `/ask docs TASK` to request a revision. When you are ready to publish, `/pr docs --commit` commits pending changes, pushes the branch, and opens a pull request using the GitHub CLI (`gh`). This requires a configured Git remote and a signed-in `gh` for the pull request.

Outside a Git repository, specialists get private directories. Branch diffs and pull requests require a Git worktree.

### Everyday controls

| Action | Control |
| --- | --- |
| Send a message, or queue it if the thread is busy | Enter |
| Insert a newline | Alt+Enter or Ctrl+J |
| Steer the current turn | Ctrl+Enter |
| Find an action or browse chat commands | Ctrl+P or `/help` |
| Ask a side question while the mission runs | `/btw QUESTION` |
| Keep working toward a completion criterion, up to the configured turn cap | `/goal CRITERIA` |
| Select a Commander profile or restore automatic routing | F2 or `/entry auto` |
| Copy the latest reply | Ctrl+Y |
| Cancel the active turn | Esc twice |
| Exit the terminal UI | Ctrl+Q |

## Accounts, models, and usage

To check which accounts Agent Command can see, run `agent-command onboard --status`. For a second account, choose a profile name and sign in through the provider’s CLI. For example, these commands create a Codex profile called `work` and check its status.

```sh
agent-command auth login codex work
agent-command auth status codex work
```

The CLI provider names are `claude`, `codex`, and `gemini` (`agy`). Additional profiles live under `~/.local/share/sub-auth` by default. Set `SUB_AUTH_DIR` to an absolute path to choose another location. For enterprise sign-in, cloud providers, or GitHub Enterprise, see `agent-command onboard --help`.

Automatic routing selects an eligible signed-in profile. Use `/entry codex/work` to choose the example profile for Commander, or `@codex` in a task to target that provider for one turn. The usage display shows reported allowances and reset times. When a provider reports a limit, Agent Command can continue on an eligible alternate after verifying that the Git workspace still matches its checkpoint. Settings also lets you choose a fallback profile or enable resuming after a known reset.

Run `agent-command model show` to inspect model pins and `agent-command catalog show` to inspect the ranking data. Benchmark-based routing requires a catalog refresh with your own Artificial Analysis key. [Model selection and routing reference](docs/routing.md)

## Memory and credentials

Use `/memory search QUERY` to look up saved project knowledge. Self-Improvement Memory records previous attempts, outcomes, and guidance for retries. Use `/sim search QUERY` to find a past problem or `/sim recent` to browse recent attempts.

The encrypted vault stores named secrets. Run `agent-command vault list` to list names and `agent-command vault check NAME` to check availability. To supply a value through standard input, use `agent-command vault set NAME --stdin`.

## Headless use and MCP

To keep specialist work running under a headless process, start the daemon in your project. From a second terminal on the same machine, check its threads.

```sh
agent-command daemon
```

```sh
agent-command connect http://127.0.0.1:47100 --once
```

The daemon listens on loopback by default and authenticates requests with a pairing token. A local client can read the token from private state. For a remote connection, bind the daemon to a specific private-network address with `--bind` and `--allow-remote`. On the client, use `--token-env VARIABLE` to name the environment variable holding the token.

An MCP client can also launch Agent Command directly over stdio. Replace the example root with your project’s absolute path. This configuration gives the client read-only access to inspect threads and their transcripts.

```json
{
  "mcpServers": {
    "agent-command": {
      "command": "agent-command",
      "args": ["mcp", "--root", "/absolute/path/to/project", "--scope", "read-only"]
    }
  }
}
```

Use `--scope supervised` to let the client create specialist threads and delegate tasks. Run `agent-command mcp --help` for the remote daemon connection syntax.

## Updates, diagnostics, and removal

Release builds check for updates at launch. Press F6 when an update is available, or rerun the installer. For troubleshooting, `agent-command triage` prints a redacted report of the environment, settings, subscription status, and workspace state. Add `--json` for structured output.

To remove the release binary, close Agent Command and run the uninstaller for your system. Saved profiles and settings are kept.

**macOS or Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/uninstall.sh | sh
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/Agent-Commander/Agent-Command/main/install/uninstall.ps1 | iex
```

## Build and test

From a checkout with Rust 1.99 or newer, run `cargo run --locked` to open the app, or `cargo run --locked --bin dev` for live-reload development. Run the same checks used before a push.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

For the source layout and contribution workflow, see the [repository guide](AGENTS.md) and [team charter](TEAM.md). For terminal sizes and interaction checks, see the [terminal test guide](docs/terminal-checks.md).
