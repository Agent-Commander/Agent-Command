"""Portable bootstrap: python3 run.py (macOS/Linux), py run.py (Windows)."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import sys
import venv


ROOT = Path(__file__).resolve().parent


def main() -> int:
    parser = argparse.ArgumentParser(description="Start Agent Command with live refresh.")
    parser.add_argument("--check", action="store_true", help="Install dependencies and run headless checks instead of opening the UI.")
    parser.add_argument("--no-reload", action="store_true", help="Run without watching source or styles.")
    args = parser.parse_args()
    if sys.version_info < (3, 10):
        parser.error("Python 3.10 or newer is required.")
    # Separate Windows and POSIX environments, including when sharing this folder with WSL.
    environment = ROOT / (".venv" if os.name == "nt" else ".venv-posix")
    python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    try:
        if not python.exists():
            print("Creating a local Python environment...", flush=True)
            venv.EnvBuilder(with_pip=True).create(environment)
        requirements = ROOT / "requirements.txt"
        stamp = environment / ".requirements-installed"
        signature = requirements.read_bytes()
        if not stamp.exists() or stamp.read_bytes() != signature:
            subprocess.run([str(python), "-m", "pip", "install", "-r", str(requirements)], check=True)
            stamp.write_bytes(signature)
        command = [str(python), "-m", "unittest", "discover", "-s", "tests", "-v"] if args.check else [str(python), str(ROOT / ("app.py" if args.no_reload else "dev.py"))]
        if not args.check and not (sys.stdin.isatty() and sys.stdout.isatty()):
            print("Open a terminal and run this command there, or use --check for headless validation.", file=sys.stderr)
            return 2
        child_env = os.environ.copy()
        child_env["PYTHONUTF8"] = "1"
        return subprocess.call(command, cwd=ROOT, env=child_env)
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"Unable to start Agent Command: {error}", file=sys.stderr)
        print("On Linux, ensure your distribution's Python venv/pip packages are installed.", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
