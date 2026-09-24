#! /usr/bin/env bash
#
# Rootless launcher for forge-bot, for hosts where systemd is unavailable.
# Detaches with setsid + nohup so the bot survives the login shell.
#
# Usage:
#   FORGEJO_TOKEN=... ./contrib/run.sh [config-path]
#
# Stop with:  kill "$(cat ~/.local/state/forge-bot/forge-bot.pid)"

set -euo pipefail

readonly CONFIG="${1:-$HOME/.config/forge-bot/forge-bot.toml}"
readonly BIN="${FORGE_BOT_BIN:-$HOME/.local/bin/forge-bot}"
readonly STATE_DIR="$HOME/.local/state/forge-bot"
readonly LOG="$STATE_DIR/forge-bot.log"
readonly PID_FILE="$STATE_DIR/forge-bot.pid"

if [[ ! -x "$BIN" ]]; then
    echo "forge-bot binary not found at $BIN (set FORGE_BOT_BIN)" >&2
    exit 1
fi
if [[ ! -f "$CONFIG" ]]; then
    echo "config not found at $CONFIG" >&2
    exit 1
fi
if [[ -z "${FORGEJO_TOKEN:-}" ]]; then
    echo "warning: FORGEJO_TOKEN is not set; the bot may not be able to read or reply" >&2
fi

mkdir -p "$STATE_DIR"
if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "forge-bot already running (pid $(cat "$PID_FILE"))" >&2
    exit 1
fi

setsid nohup "$BIN" --config "$CONFIG" serve >"$LOG" 2>&1 </dev/null &
echo $! >"$PID_FILE"
sleep 2

if kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "forge-bot started (pid $(cat "$PID_FILE")), log: $LOG"
else
    echo "forge-bot failed to start; see $LOG" >&2
    exit 1
fi
