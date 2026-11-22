#!/bin/sh
set -eu

binary=$(realpath "${1:-target/debug/agent-command}")
fixture=$(mktemp -d)
socket="$fixture/tmux"
trap 'tmux -S "$socket" kill-server >/dev/null 2>&1 || true; rm -rf "$fixture"' EXIT
profiles="$fixture/profiles"
long=1234567890abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzab
test "${#long}" = 64
umask 077
mkdir -p "$profiles/claude/0" "$profiles/claude/$long" "$fixture/work"

t() {
    tmux -S "$socket" "$@"
}

await_capture() {
    for _ in $(seq 1 60); do
        t capture-pane -p -t "$1:0.0" > "$fixture/screen"
        if grep -Fq "$2" "$fixture/screen"; then
            return
        fi
        sleep 0.05
    done
    printf 'Missing visible text at %s: %s\n' "$1" "$2" >&2
    cat "$fixture/screen" >&2
    return 1
}

for dimensions in 80x24 120x40; do
    width=${dimensions%x*}
    height=${dimensions#*x}
    t new-session -d -s "$dimensions" -x "$width" -y "$height" -c "$fixture/work" \
        env SUB_AUTH_DIR="$profiles" AGENT_COMMAND_THEME="$fixture/no-theme" "$binary"
    await_capture "$dimensions" 'claude/0'
    if test "$width" = 80; then
        t send-keys -t "$dimensions:0.0" F2
    else
        mouse=$(printf '\033[<0;8;%sM' "$((height - 8))" | od -An -v -tx1)
        t send-keys -H -t "$dimensions:0.0" $mouse
    fi
    await_capture "$dimensions" "claude/$long"
    grep -Fq '5h unavailable' "$fixture/screen"
    grep -Fq 'Week unavailable' "$fixture/screen"
    grep -Fq 'No latch' "$fixture/screen"
    printf '%s full profile and both usage windows visible\n' "$dimensions"
    t kill-session -t "$dimensions"
done
