#!/bin/sh
set -eu
base=${AGENT_COMMAND_RELEASE_URL:-https://github.com/Agent-Commander/Agent-Command/releases/latest/download}
case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) echo 'Unsupported OS. On Windows use the PowerShell installer.' >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo 'Supported processors: x86_64 and ARM64.' >&2; exit 1 ;;
esac
asset="agent-command-$arch-$os.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
curl -fsSL "$base/$asset" -o "$tmp/$asset"
curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256"
(cd "$tmp"; if command -v sha256sum >/dev/null 2>&1; then sha256sum -c "$asset.sha256"; else shasum -a 256 -c "$asset.sha256"; fi)
tar -xzf "$tmp/$asset" -C "$tmp"
bin=${AGENT_COMMAND_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$bin"
if [ -e "$bin/agent-command" ] && ! "$bin/agent-command" --version 2>/dev/null | grep -q '^agent-command '; then
  echo "Refusing to replace unrelated file: $bin/agent-command" >&2; exit 1
fi
install -m 755 "$tmp/agent-command" "$bin/agent-command"
# Only append our own bin directory when it is missing. Never replace existing PATH entries.
case ":$PATH:" in *":$bin:"*) ;; *)
  if [ -n "${AGENT_COMMAND_INSTALL_DIR:-}" ]; then
    echo "Add $bin to PATH to run agent-command by name."
  else
    line='export PATH="$HOME/.local/bin:$PATH"'
    for profile in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
      if ! grep -Fq "$line" "$profile" 2>/dev/null; then printf '\n# Agent Command\n%s\n' "$line" >> "$profile"; fi
    done
  fi
;; esac
printf '\nInstalled Agent Command. Open a new terminal and run: agent-command\n'
if [ "${AGENT_COMMAND_NO_LAUNCH:-0}" != 1 ] && [ -r /dev/tty ]; then
  "$bin/agent-command" </dev/tty >/dev/tty
fi
