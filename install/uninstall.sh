#!/bin/sh
set -eu
marker='# Agent Command'
path_line='export PATH="$HOME/.local/bin:$PATH"'
bin=${AGENT_COMMAND_INSTALL_DIR:-"$HOME/.local/bin"}
exe="$bin/agent-command"
auth_dir=${SUB_AUTH_DIR:-"$HOME/.local/share/sub-auth"}
theme=${AGENT_COMMAND_THEME:-"${XDG_CONFIG_HOME:-$HOME/.config}/agent-command/theme.toml"}

remove_binary() {
  if [ ! -e "$exe" ]; then
    echo "No agent-command found in $bin"
    return 0
  fi
  if ! "$exe" --version 2>/dev/null | grep -q '^agent-command '; then
    echo "Refusing to remove unrelated file: $exe" >&2
    exit 1
  fi
  if ! rm -f "$exe" || [ -e "$exe" ]; then
    echo "Could not remove $exe. Close Agent Command, check permissions, and run the uninstaller again." >&2
    exit 1
  fi
  echo "Removed $exe"
}

remove_profile_lines() {
  profile=$1
  [ -f "$profile" ] && grep -Fxq "$marker" "$profile" || return 0
  cleaned=$(mktemp)
  awk -v marker="$marker" -v path_line="$path_line" '
    held { held = 0; if ($0 == path_line) next; print marker }
    $0 == marker { held = 1; next }
    { print }
    END { if (held) print marker }
  ' "$profile" > "$cleaned"
  if ! cmp -s "$cleaned" "$profile"; then
    cat "$cleaned" > "$profile"
    echo "Removed Agent Command PATH lines from $profile"
  fi
  rm -f "$cleaned"
}

remove_binary
rmdir "$bin" 2>/dev/null && echo "Removed empty $bin" || true
if [ -z "${AGENT_COMMAND_INSTALL_DIR:-}" ]; then
  for profile in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
    remove_profile_lines "$profile"
  done
fi
echo "Kept sign-ins and app data: $auth_dir"
echo "Kept theme settings: $theme"
echo 'To sign out, run agent-command auth logout PROVIDER PROFILE first; reinstall to do it now.'
if command -v cargo >/dev/null 2>&1 && cargo install --list 2>/dev/null | grep -q '^agent-command '; then
  echo 'Cargo also installed agent-command. Next step: cargo uninstall agent-command'
fi
printf '\nUninstalled Agent Command.\n'
