"""Restart after Python edits; Textual handles stylesheet refresh in place."""
import os
import importlib
import time
from pathlib import Path

os.chdir(Path(__file__).parent)
os.environ.pop("NO_COLOR", None)
os.environ.setdefault("COLORTERM", "truecolor")
if os.environ.get("TERM", "dumb") == "dumb":
    os.environ["TERM"] = "xterm-256color"
os.environ["AGENT_COMMAND_DEV"] = "1"
while True:
    try:
        import app
        importlib.reload(app)
        result = app.AgentCommand(watch_css=True).run()
        if result != "reload":
            break
    except KeyboardInterrupt:
        break
    except Exception:
        import traceback
        traceback.print_exc()
        print("Fix app.py and save to retry. Ctrl+C to quit.")
        stamp = Path("app.py").stat().st_mtime_ns
        try:
            while Path("app.py").stat().st_mtime_ns == stamp:
                time.sleep(0.5)
        except KeyboardInterrupt:
            break
