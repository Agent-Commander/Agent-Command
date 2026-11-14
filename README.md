# Agent Command

A native terminal UI prototype with an Agent Command backdrop, selectable agent roster, mission details, activity feed, and command input. All agents and actions are local sample data; no agent services are connected.

## Run with live refresh

Requires Python 3.10+ and a Unicode terminal, at least 120 columns by 40 rows for the complete layout. A modern terminal with true color gives the intended palette. The same Python/Textual UI runs on Windows, Linux, and macOS (Intel or Apple Silicon with a compatible Python installation).

macOS and Linux:

```sh
sh start.sh
# or, from any directory:
python3 /path/to/Agent-Command/run.py
```

Windows PowerShell:

```powershell
.\start.ps1
# or:
python run.py
```

The launcher creates an isolated environment and installs pinned dependencies on first run. Windows uses `.venv`; Linux/macOS use `.venv-posix`, so a shared checkout also works with WSL. Internet access is needed for the first installation. On Debian/Ubuntu, install `python3-venv` if Python reports that ensurepip is missing. If using uv, `uv venv --seed .venv-posix` is another option. No system Python packages are modified.

Run without reload using `python3 run.py --no-reload`. Run the headless checks using `python3 run.py --check` (use `python` on Windows). CI runs these checks on Ubuntu, macOS, and Windows with Python 3.10 and 3.13. CI verifies the app and controls; it does not guarantee identical font rendering in every terminal emulator.

Save `theme.tcss` to refresh styles in place. Save `app.py` to automatically reload the interface (demo state resets). Ctrl+Q exits. Ctrl+N focuses the command bar; Ctrl+P pauses or resumes the selected demo agent.

The Privateer theme uses muted slate backgrounds with a slight blue tint, an ivory menu bar and primary buttons, square terminal borders, a small six-color stripe, and privateer radio details. The development launcher enables true color for this process, overriding inherited `NO_COLOR` without changing system settings.

Ctrl+P has priority over list navigation so pause/resume works while the crew list has focus. The menu-bar labels are decorative in this prototype; use the buttons or footer shortcuts.
