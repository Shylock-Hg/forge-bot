#! /usr/bin/env bash
#
# Print the `owner/repo` for a git remote (default `origin`).
#
# Handles https://, ssh:// and scp-like (`git@host:owner/repo`) URLs, including
# GitLab subgroups.

set -euo pipefail

readonly REMOTE="${1:-origin}"

url="$(git remote get-url "$REMOTE" 2>/dev/null || true)"
if [[ -z "$url" ]]; then
    echo "could not determine repository: no git remote '$REMOTE'" >&2
    exit 1
fi

# Normalize scp-like syntax to a URL: git@host:owner/repo -> ssh://git@host/owner/repo
if [[ "$url" =~ ^[^/]+@[^/:]+: ]]; then
    url="ssh://${url/:/\/}"
fi

# Strip scheme, user, host[:port], trailing .git and slash.
printf '%s\n' "$url" | sed -E \
    -e 's#^[a-zA-Z][a-zA-Z0-9+.-]*://##' \
    -e 's#^[^/@]+@##' \
    -e 's#^[^/]+/##' \
    -e 's#\.git$##' \
    -e 's#/$##'
