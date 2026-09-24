#! /usr/bin/env bash
#
# Install forge-bot as a systemd service. Run as root.
#
# Usage:
#   sudo ./contrib/install.sh
#
# Environment:
#   BIN_SRC       Binary to install        (default ../target/release/forge-bot)
#   CONFIG_SRC    Config to install        (default ../forge-bot.toml)
#   PREFIX        Install prefix           (default /usr/local)
#   SERVICE_USER  Service account          (default agent)
#
# Secrets are taken from the existing EnvironmentFile, or from
# contrib/forge-bot.env.example when no env file is present yet. The installer
# never writes secrets into the config or the unit.

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
readonly BIN_SRC="${BIN_SRC:-$PROJECT_DIR/target/release/forge-bot}"
readonly CONFIG_SRC="${CONFIG_SRC:-$PROJECT_DIR/forge-bot.toml}"
readonly PREFIX="${PREFIX:-/usr/local}"
readonly SERVICE_USER="${SERVICE_USER:-agent}"

readonly CONFIG_DIR=/etc/forge-bot
readonly CONFIG_DST="$CONFIG_DIR/forge-bot.toml"
readonly ENV_DST="$CONFIG_DIR/forge-bot.env"
readonly UNIT_DST="/etc/systemd/system/forge-bot.service"

if (( EUID != 0 )); then
    echo "Run this script as root (for example: sudo $0)." >&2
    exit 1
fi

for file in "$BIN_SRC" "$CONFIG_SRC" "$SCRIPT_DIR/forge-bot.service"; do
    if [[ ! -f "$file" ]]; then
        echo "Missing required file: $file" >&2
        exit 1
    fi
done

echo "Installing binary to $PREFIX/bin/forge-bot..."
install -Dm755 "$BIN_SRC" "$PREFIX/bin/forge-bot"

echo "Installing config to $CONFIG_DST..."
install -Dm640 -o "$SERVICE_USER" -g "$SERVICE_USER" "$CONFIG_SRC" "$CONFIG_DST"

if [[ ! -f "$ENV_DST" ]]; then
    echo "Installing example environment file to $ENV_DST (edit it!)..."
    install -Dm600 -o "$SERVICE_USER" -g "$SERVICE_USER" \
        "$SCRIPT_DIR/forge-bot.env.example" "$ENV_DST"
fi

echo "Installing systemd unit..."
sed -e "s/^User=.*/User=$SERVICE_USER/" \
    -e "s/^Group=.*/Group=$SERVICE_USER/" \
    "$SCRIPT_DIR/forge-bot.service" >"$UNIT_DST"
systemctl daemon-reload

echo "Starting forge-bot..."
systemctl enable --now forge-bot

cat <<EOF

Installed.

Edit secrets:  $ENV_DST
Edit config:    $CONFIG_DST
Logs:           journalctl -u forge-bot -f
Status:         systemctl status forge-bot

Until a webhook is registered, keep \`[poller] enabled = true\` in the config.
EOF
