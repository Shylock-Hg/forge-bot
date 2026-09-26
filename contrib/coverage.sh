#!/usr/bin/env bash
#
# Run test coverage for forge-bot using cargo-llvm-cov.
#
# Usage:
#   ./contrib/coverage.sh [args...]
#
# Examples:
#   ./contrib/coverage.sh
#   ./contrib/coverage.sh --fail-under-lines 95
#   ./contrib/coverage.sh --html --open

set -euo pipefail

export LLVM_COV="${LLVM_COV:-llvm-cov}"
export LLVM_PROFDATA="${LLVM_PROFDATA:-llvm-profdata}"

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    if [[ -x "$HOME/.cargo/bin/cargo-llvm-cov" ]]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    elif [[ -x "$HOME/.local/bin/cargo-llvm-cov" ]]; then
        export PATH="$HOME/.local/bin:$PATH"
    else
        echo "cargo-llvm-cov is required but not found in PATH." >&2
        echo "Install it via: cargo install cargo-llvm-cov --locked" >&2
        echo "Or download prebuilt binary from: https://github.com/taiki-e/cargo-llvm-cov" >&2
        exit 1
    fi
fi

exec cargo llvm-cov "$@"
