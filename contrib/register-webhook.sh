#! /usr/bin/env bash
#
# Register the Forgejo webhook for a repository. Requires an owner/admin token:
# a plain collaborator token cannot create hooks.
#
# Usage:
#   FORGEJO_TOKEN=... FORGEJO_WEBHOOK_SECRET=... REPO=owner/repo \
#       ./contrib/register-webhook.sh
#
# Environment:
#   FORGEJO_URL             Base URL of the Forgejo instance (default
#                           http://127.0.0.1:3000)
#   FORGEJO_TOKEN           Owner/admin access token (required)
#   FORGEJO_WEBHOOK_SECRET  Shared secret, must match the bot config (required)
#   REPO                    owner/repo (default: the current git remote's repo)
#   BOT_URL                 Hook target (default
#                           http://127.0.0.1:8080/webhooks/forgejo)
#   EVENTS                  JSON event list (default ["issue_comment"])

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

: "${FORGEJO_TOKEN:?set FORGEJO_TOKEN to an owner/admin token}"
: "${FORGEJO_WEBHOOK_SECRET:?set FORGEJO_WEBHOOK_SECRET to a shared secret}"

# Default to the repository of the current git remote (any repo works).
REPO="${REPO:-$("$SCRIPT_DIR/detect-repo.sh")}"

FORGEJO_URL="${FORGEJO_URL:-http://127.0.0.1:3000}"
BOT_URL="${BOT_URL:-http://127.0.0.1:8080/webhooks/forgejo}"
EVENTS="${EVENTS:-[\"issue_comment\"]}"

payload=$(
    printf '{"type":"forgejo","active":true,"events":%s,"config":{"url":"%s","content_type":"json","secret":"%s","http_method":"post"}}' \
        "$EVENTS" "$BOT_URL" "$FORGEJO_WEBHOOK_SECRET"
)

echo "Creating webhook on $REPO -> $BOT_URL"
curl -fsS -X POST \
    -H "Authorization: token $FORGEJO_TOKEN" \
    -H "Content-Type: application/json" \
    "$FORGEJO_URL/api/v1/repos/$REPO/hooks" \
    -d "$payload" >/dev/null

echo "Webhook created. Set '[poller] enabled = false' in the bot config."
