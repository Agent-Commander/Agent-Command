"""Agent Command — interactive, local UI prototype."""
from datetime import datetime
import os
from pathlib import Path

# Keep the preview palette when launched from a monochrome automation shell.
os.environ.pop("NO_COLOR", None)

from textual import on
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.widgets import Button, Footer, Input, Label, ListItem, ListView, RichLog, Static


AGENTS = [
    ("01", "Atlas", "BUILDING", "Implementing the command center", "12m 34s", "#9eb8cc"),
    ("02", "Nova", "REVIEWING", "Reviewing interface components", "04m 18s", "#d6ac6a"),
    ("03", "Echo", "READY", "Standing by for a new mission", "—", "#9eb8cc"),
    ("04", "Orion", "READY", "Standing by for a new mission", "—", "#9eb8cc"),
]

GLYPHS = {
    "A": (" ▄▄ ", "█▄▄█", "█  █"), "G": (" ▄▄▄", "█ ▄▄", "█▄▄█"),
    "E": ("▄▄▄▄", "█▄▄ ", "█▄▄▄"), "N": ("▄  ▄", "██ █", "█ ██"),
    "T": ("▄▄▄▄", " ▐▌ ", " ▐▌ "), "C": (" ▄▄▄", "█   ", "█▄▄▄"),
    "O": (" ▄▄ ", "█  █", "▀▄▄▀"), "M": ("▄   ▄", "█▀▄▀█", "█   █"),
    "D": ("▄▄▄ ", "█  █", "█▄▄▀"), " ": ("  ", "  ", "  "),
}
BACKDROP = "\n".join(" ".join(GLYPHS[c][row] for c in "AGENT COMMAND") for row in range(3))


class AgentCommand(App):
    CSS_PATH = "theme.tcss"
    TITLE = "Agent Command"
    BINDINGS = [Binding("ctrl+n", "new_mission", "New mission"), Binding("ctrl+p", "pause_agent", "Pause / resume", priority=True), Binding("ctrl+l", "clear_log", "Clear feed"), Binding("ctrl+q", "quit", "Quit")]

    def compose(self) -> ComposeResult:
        yield Static(" ▣  AGENT COMMAND     File   Crew   Missions   Radio                         PRIVATEER / 01", id="topbar")
        with Horizontal(id="hero"):
            yield Static(BACKDROP, id="wordmark")
            yield Static("PRIVATEER RADIO  /  LOCAL\n[dim]───┤[/dim] [#d6ac6a]◆[/] [dim]├───[/dim]  SIGNAL CLEAR\n1200 BAUD  ·  UI PREVIEW", id="workspace")
        yield Static("[#89a76f]━━[/][#dbc77b]━━[/][#cc955d]━━[/][#b86d67]━━[/][#9c829f]━━[/][#7d9fb6]━━[/]  INDEPENDENT OPERATORS. UNCHARTED WATERS.", id="tagline")
        with Horizontal(id="metrics"):
            yield Static("[#9eb8cc]04[/]  AGENTS ONLINE", classes="metric")
            yield Static("[#9eb8cc]02[/]  ACTIVE MISSIONS", classes="metric")
            yield Static("[#9eb8cc]00[/]  NEED ATTENTION", classes="metric")
            yield Static("[#d6ac6a]UI PROTOTYPE[/]  /  SAMPLE DATA", classes="metric")
        with Horizontal(id="main"):
            with Vertical(id="roster", classes="panel"):
                yield Static("01 / CREW", classes="panel-title")
                yield ListView(*[ListItem(Label(f"[{color}]●[/] {name}  [dim]/{num}[/dim]\n  {status.lower()}")) for num, name, status, _, _, color in AGENTS], id="agents")
                yield Static("[dim]↑ ↓ select an agent\nEnter to inspect[/dim]", id="roster-hint")
            with Vertical(id="detail", classes="panel"):
                yield Static("02 / CHART ROOM", classes="panel-title")
                yield Static(id="agent-heading")
                yield Static(id="mission")
                yield Static("WORKSPACE\nAgent-Commander UI\n\nCURRENT STEP\nRefining layout and interaction\n\nPROGRESS\n[#9eb8cc]━━━━━━━━━━━━━━━━[/][#273335]━━━━━━━━[/]  68%", id="mission-body")
                with Horizontal(id="actions"):
                    yield Button("+ New mission", id="new", variant="primary")
                    yield Button("Ⅱ Pause", id="pause")
            with Vertical(id="activity", classes="panel"):
                yield Static("03 / SHIP'S LOG", classes="panel-title")
                yield RichLog(id="feed", highlight=False, markup=True, wrap=True)
        with Horizontal(id="command-row"):
            yield Static("❯", id="prompt")
            yield Input(placeholder="Give your selected agent a mission…", id="command")
            yield Button("Send ↗", id="send", variant="primary")
        yield Static("[#9eb8cc]●[/] SIGNAL CLEAR    [dim]Local demo · commands stay in this preview[/dim]", id="status")
        yield Footer()

    def on_mount(self) -> None:
        self.paused: set[int] = set()
        self.query_one("#agents", ListView).index = 0
        self.select_agent(0)
        self.log_event("SYSTEM", "Command center initialized.")
        self.log_event("ATLAS", "Building the operations console.")
        self.log_event("NOVA", "Interface review in progress.")
        self.log_event("SYSTEM", "Ready for your next command.")
        self._source_stamp = Path(__file__).stat().st_mtime_ns
        if os.environ.get("AGENT_COMMAND_DEV") == "1":
            self.set_interval(0.6, self.watch_source)

    def watch_source(self) -> None:
        if Path(__file__).stat().st_mtime_ns != self._source_stamp:
            self.exit("reload")

    @property
    def selected(self) -> int:
        return self.query_one("#agents", ListView).index or 0

    def select_agent(self, index: int) -> None:
        num, name, state, mission, elapsed, color = AGENTS[index]
        state = "PAUSED" if index in self.paused else state
        self.query_one("#agent-heading", Static).update(f"[bold]{name}[/bold]  [{color}]● {state}[/]")
        self.query_one("#mission", Static).update(f"{mission}\n[dim]AGENT {num}  /  ELAPSED {elapsed}[/dim]")
        self.query_one("#pause", Button).label = "▶ Resume" if index in self.paused else "Ⅱ Pause"

    @on(ListView.Highlighted)
    def agent_highlighted(self, event: ListView.Highlighted) -> None:
        if hasattr(self, "paused"):
            self.select_agent(self.selected)

    def log_event(self, source: str, message: str) -> None:
        self.query_one("#feed", RichLog).write(f"[dim]{datetime.now():%H:%M:%S}[/dim]  [#9eb8cc]{source}[/]\n{message}\n")

    def action_new_mission(self) -> None:
        self.query_one("#command", Input).focus()

    def action_pause_agent(self) -> None:
        index = self.selected
        if index in self.paused:
            self.paused.remove(index)
        else:
            self.paused.add(index)
        self.select_agent(index)
        self.log_event(AGENTS[index][1].upper(), "Paused in preview." if index in self.paused else "Resumed in preview.")

    def action_clear_log(self) -> None:
        self.query_one("#feed", RichLog).clear()

    @on(Button.Pressed)
    def button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "new":
            self.action_new_mission()
        elif event.button.id == "pause":
            self.action_pause_agent()
        elif event.button.id == "send":
            self.submit_command()

    @on(Input.Submitted)
    def command_submitted(self) -> None:
        self.submit_command()

    def submit_command(self) -> None:
        field = self.query_one("#command", Input)
        if command := field.value.strip():
            self.log_event("YOU → " + AGENTS[self.selected][1].upper(), command)
            self.query_one("#mission", Static).update(f"{command}\n[dim]PREVIEW MISSION · NOT DISPATCHED[/dim]")
            field.value = ""


if __name__ == "__main__":
    AgentCommand().run()
