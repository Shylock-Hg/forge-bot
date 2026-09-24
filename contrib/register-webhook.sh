#! /usr/bin/env bash
#
# Register a Forgejo webhook that points at forge-bot.
#
# Usage:
#   FORGEJO_TOKEN=... FORGEJO_WEBHOOK_SECRET=... ./contrib/register-webhook.sh
#   SCOPE=user   FORGEJO_TOKEN=... FORGEJO_WEBHOOK_SECRET=... ./contrib/register-webhook.sh
#   SCOPE=org    ORG=my-org          ... ./contrib/register-webhook.sh
#   SCOPE=system ...                 ./contrib/register-webhook.sh
#
# Environment:
#   FORGEJO_URL             Base URL of the Forgejo instance (default
#                           http://127.0.0.1:3000)
#   FORGEJO_TOKEN           Access token with the scope/role for SCOPE (required)
#   FORGEJO_WEBHOOK_SECRET  Shared secret, must match the bot config (required)
#   SCOPE                   repo (default), org, user or system
#   REPO                    owner/repo, when SCOPE=repo
#                           (default: the current git remote's repository)
#   ORG                     organization name, when SCOPE=org
#   BOT_URL                 Hook target (default
#                           http://127.0.0.1:8080/webhooks/forgejo)
#   EVENTS                  JSON event list (default ["issue_comment"])
#
# Scope requirements (scope + role):
#   repo    write:repository + repository owner/admin
#   org     write:organization + organization owner
#   user    write:user (covers every repository owned by the token's user)
#   system  write:admin + instance administrator (covers the whole instance)

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

: "${FORGEJO_TOKEN:?set FORGEJO_TOKEN to a token with the required scope}"
: "${FORGEJO_WEBHOOK_SECRET:?set FORGEJO_WEBHOOK_SECRET to a shared secret}"

SCOPE="${SCOPE:-repo}"
FORGEJO_URL="${FORGEJO_URL:-http://127.0.0.1:3000}"
BOT_URL="${BOT_URL:-http://127.0.0.1:8080/webhooks/forgejo}"
EVENTS="${EVENTS:-[\"issue_comment\"]}"

case "$SCOPE" in
    repo)
        REPO="${REPO:-$("$SCRIPT_DIR/detect-repo.sh")}"
        endpoint="$FORGEJO_URL/api/v1/repos/$REPO/hooks"
        target="repository $REPO"
        requirements="write:repository and repository owner/admin"
        ;;
    org)
        : "${ORG:?set ORG when SCOPE=org}"
        endpoint="$FORGEJO_URL/api/v1/orgs/$ORG/hooks"
        target="organization $ORG"
        requirements="write:organization and organization owner"
        ;;
    user)
        endpoint="$FORGEJO_URL/api/v1/user/hooks"
        target="every repository of the token's user"
        requirements="write:user"
        ;;
    system)
        endpoint="$FORGEJO_URL/api/v1/admin/hooks"
        target="every repository on the instance"
        requirements="write:admin and an instance administrator"
        ;;
    *)
        echo "unknown SCOPE '$SCOPE' (expected repo, org, user or system)" >&2
        exit 2
        ;;
esac

payload=$(
    printf '{"type":"forgejo","active":true,"events":%s,"config":{"url":"%s","content_type":"json","secret":"%s","http_method":"post"}}' \
        "$EVENTS" "$BOT_URL" "$FORGEJO_WEBHOOK_SECRET"
)

echo "Creating $SCOPE webhook for $target -> $BOT_URL"

response=$(
    curl -sS -w '\n%{http_code}' -X POST \
        -H "Authorization: token $FORGEJO_TOKEN" \
        -H "Content-Type: application/json" \
        "$endpoint" \
        -d "$payload"
)
status="${response##*$'\n'}"
body="${response%$'\n'*}"

if [[ "$status" != 2* ]]; then
    echo "failed: HTTP $status $body" >&2
    echo "required: $requirements" >&2
    exit 1
fi

echo "Webhook created ($target)."
if [[ "$SCOPE" == "user" || "$SCOPE" == "system" ]]; then
    echo "This hook covers repositories created later too."
else
    echo "Set '[poller] enabled = false' once a hook covers every repository."
fi
