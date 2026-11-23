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
usage="$profiles/agent-command-usage-v1/claude/$long"
mkdir -p "$profiles/claude/0" "$profiles/claude/$long" "$usage" "$fixture/work"
now=$(date +%s)
printf '{"week":{"used":97,"reset":%s,"observed":%s,"stamp":null},"unavailable":false}\n' \
    "$((now + 604800))" "$((now - 901))" > "$usage/state.json"

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
    if test "$width" = 80; then
        await_capture "$dimensions" 'claude/0'
        await_capture "$dimensions" '· Auto'
        cycle='Override saved: claude/0|Override saved: claude/1234567890abc'
    else
        await_capture "$dimensions" "claude/$long"
        grep -Fq '· Override' "$fixture/screen"
        cycle='Auto routing|Override saved: claude/0|Override saved: claude/1234567890abc'
    fi
    old_ifs=$IFS
    IFS='|'
    for expected in $cycle; do
        t send-keys -t "$dimensions:0.0" F2
        await_capture "$dimensions" "$expected"
    done
    IFS=$old_ifs
    await_capture "$dimensions" "claude/$long"
    grep -Fq '5h unavailable' "$fixture/screen"
    grep -Fq '· Override' "$fixture/screen"
    printf '%s keyboard override cycle visible\n' "$dimensions"
    mouse=$(printf '\033[<0;8;%sM' "$((height - 8))" | od -An -v -tx1)
    for expected in 'Auto routing' 'Override saved: claude/0' 'Override saved: claude/1234567890abc'; do
        t send-keys -H -t "$dimensions:0.0" $mouse
        await_capture "$dimensions" "$expected"
    done
    printf '%s mouse override cycle visible\n' "$dimensions"
    await_capture "$dimensions" "stale age unknown"
    grep -Eq "Week 97% used / 3% left.*stale" "$fixture/screen"
    grep -Fq 'No latch' "$fixture/screen"
    grep -Fq "\"profile\":\"claude/$long\"" "$profiles/agent-command-routing-v1/override.json"
    printf '%s full profile, override, unavailable five-hour window and stale weekly window visible\n' "$dimensions"
    t kill-session -t "$dimensions"
done
