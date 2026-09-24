#! /usr/bin/env bash
#
# Send a signed test `issue_comment` webhook to a running forge-bot.
#
# Usage:
#   FORGEJO_WEBHOOK_SECRET=... REPO=owner/repo ./contrib/test-webhook.sh
#
# Environment:
#   FORGEJO_WEBHOOK_SECRET  Shared secret configured on the bot (required)
#   FORGEJO_URL             Forgejo base URL (default http://127.0.0.1:3000)
#   BOT_URL                 Bot webhook endpoint (default
#                           http://127.0.0.1:8080/webhooks/forgejo)
#   REPO                    owner/repo (default: the current git remote's repo)
#   AUTHOR                  Comment author, must satisfy the bot policy
#                           (default shylock)
#   MESSAGE                 Comment body (default "@shylock-bot reply with pong")

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

: "${FORGEJO_WEBHOOK_SECRET:?set FORGEJO_WEBHOOK_SECRET}"

FORGEJO_URL="${FORGEJO_URL:-http://127.0.0.1:3000}"
BOT_URL="${BOT_URL:-http://127.0.0.1:8080/webhooks/forgejo}"
REPO="${REPO:-$("$SCRIPT_DIR/detect-repo.sh")}"
AUTHOR="${AUTHOR:-shylock}"
MESSAGE="${MESSAGE:-@shylock-bot reply with pong}"

readonly COMMENT_ID="$(( $(date +%s) % 100000 ))"
readonly ISSUE_URL="${FORGEJO_URL%/}/$REPO/issues/1"

body=$(
    printf '{"action":"created","issue":{"number":1,"title":"webhook test","html_url":"%s"},"comment":{"id":%s,"body":"%s","html_url":"%s#issuecomment-%s","user":{"login":"%s"}},"repository":{"full_name":"%s"},"sender":{"login":"%s"}}' \
        "$ISSUE_URL" "$COMMENT_ID" "$MESSAGE" "$ISSUE_URL" "$COMMENT_ID" "$AUTHOR" "$REPO" "$AUTHOR"
)

signature=$(
    printf '%s' "$body" | openssl dgst -sha256 -hmac "$FORGEJO_WEBHOOK_SECRET" | awk '{print $2}'
)

status=$(
    curl -s -o /tmp/forge-bot-webhook-response.$$ -w '%{http_code}' \
        -X POST "$BOT_URL" \
        -H 'Content-Type: application/json' \
        -H 'X-Forgejo-Event: issue_comment' \
        -H "X-Forgejo-Signature: $signature" \
        -d "$body"
)
response="$(cat /tmp/forge-bot-webhook-response.$$)"
rm -f /tmp/forge-bot-webhook-response.$$

echo "HTTP $status $response"
[[ "$status" == "202" ]]
