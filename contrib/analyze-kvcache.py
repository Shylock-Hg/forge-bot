#!/usr/bin/env python3
"""Measure the prompt/KV cache hit rate of the agents forge-bot invokes.

Both backends record token accounting next to their transcripts:

* Codex writes ``rollout-*.jsonl`` files under ``~/.codex/sessions``. Each
  ``token_usage_record`` entry carries ``usage.input_tokens`` (the whole prompt)
  and ``usage.cached_input_tokens`` (the part the provider served from cache).
* Pi writes ``*.jsonl`` session files under ``~/.pi/agent/sessions``. Each
  assistant message carries a ``usage`` block where ``input`` is the cache-miss
  part and ``cacheRead`` is the cache-hit part of the prompt.

Only sessions whose working directory lives under the forge-bot workspace root
are considered, so unrelated interactive use of Codex/Pi is ignored.

Run it with no arguments::

    python3 contrib/analyze-kvcache.py

The output is plain text and ends with a Markdown summary suitable for pasting
into an issue.
"""

from __future__ import annotations

import glob
import json
import os
import sys
from collections import defaultdict

HOME = os.path.expanduser("~")
WORKSPACE_ROOT = os.environ.get(
    "FORGE_BOT_WORKSPACE_ROOT",
    os.path.join(HOME, ".local/state/forge-bot/workspaces"),
)
CODEX_GLOB = os.path.join(HOME, ".codex/sessions/**/*.jsonl")
PI_GLOB = os.path.join(HOME, ".pi/agent/sessions/*/*.jsonl")


def pct(part: float, whole: float) -> float:
    return 100.0 * part / whole if whole else 0.0


def _load_jsonl(path: str):
    with open(path, errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except ValueError:
                continue


# --------------------------------------------------------------------------
# Codex
# --------------------------------------------------------------------------
def collect_codex():
    """Return (threads, runs).

    ``threads`` maps a codex thread id to its ordered runs; ``runs`` is a flat
    list of ``(phase, hit_rate)`` samples, where phase is ``cold`` (first run of
    a thread), ``resume`` (a later run in an existing thread) or ``later`` (a
    non-first call inside one run).
    """
    runs_by_thread = defaultdict(list)
    for path in glob.glob(CODEX_GLOB, recursive=True):
        meta = None
        calls = []
        for obj in _load_jsonl(path):
            if obj.get("type") == "session_meta":
                meta = obj.get("payload") or {}
            elif obj.get("type") == "token_usage_record":
                calls.append((obj.get("payload") or {}).get("usage") or {})
        if not meta or not calls:
            continue
        cwd = meta.get("cwd") or ""
        if not cwd.startswith(WORKSPACE_ROOT):
            continue
        thread = meta.get("session_id") or meta.get("id")
        runs_by_thread[thread].append(
            {
                "timestamp": meta.get("timestamp"),
                "cwd": cwd,
                "path": path,
                "calls": calls,
            }
        )
    for thread in runs_by_thread:
        runs_by_thread[thread].sort(key=lambda run: run["timestamp"] or "")

    phases = []
    for thread, runs in runs_by_thread.items():
        for index, run in enumerate(runs):
            first = run["calls"][0]
            phase = "cold" if index == 0 else "resume"
            phases.append(
                (
                    phase,
                    pct(first.get("cached_input_tokens", 0), first.get("input_tokens", 0)),
                )
            )
            for call in run["calls"][1:]:
                phases.append(
                    (
                        "later",
                        pct(
                            call.get("cached_input_tokens", 0),
                            call.get("input_tokens", 0),
                        ),
                    )
                )
    return runs_by_thread, phases


# --------------------------------------------------------------------------
# Pi
# --------------------------------------------------------------------------
def collect_pi():
    sessions = []
    for path in glob.glob(PI_GLOB):
        cwd = None
        started = None
        calls = []
        for obj in _load_jsonl(path):
            if obj.get("type") == "session":
                cwd = obj.get("cwd")
                started = obj.get("timestamp")
            elif obj.get("type") == "message":
                message = obj.get("message") or {}
                if message.get("role") == "assistant" and message.get("usage"):
                    calls.append(message["usage"])
        if not cwd or not cwd.startswith(WORKSPACE_ROOT):
            continue
        sessions.append(
            {
                "path": path,
                "cwd": cwd,
                "started": started,
                "calls": calls,
            }
        )
    sessions.sort(key=lambda session: session["started"] or "")
    return sessions


def main() -> int:
    missing = not os.path.isdir(WORKSPACE_ROOT)
    if missing:
        print(f"workspace root not found: {WORKSPACE_ROOT}", file=sys.stderr)
        return 1

    print("=" * 78)
    print(f"Codex sessions under {WORKSPACE_ROOT}")
    print("=" * 78)
    codex_threads, codex_phases = collect_codex()
    total_input = total_cached = total_calls = 0
    for thread, runs in sorted(
        codex_threads.items(), key=lambda item: item[1][0]["timestamp"] or ""
    ):
        run_input = sum(
            call.get("input_tokens", 0) for run in runs for call in run["calls"]
        )
        run_cached = sum(
            call.get("cached_input_tokens", 0) for run in runs for call in run["calls"]
        )
        calls = sum(len(run["calls"]) for run in runs)
        total_input += run_input
        total_cached += run_cached
        total_calls += calls
        repo = os.path.basename(runs[0]["cwd"])
        print(
            f"\nthread {thread} [{repo}] runs={len(runs)} calls={calls}\n"
            f"  input={run_input} cached={run_cached} hit={pct(run_cached, run_input):.1f}%"
        )
        for index, run in enumerate(runs):
            first = run["calls"][0]
            first_hit = pct(
                first.get("cached_input_tokens", 0), first.get("input_tokens", 0)
            )
            r_input = sum(call.get("input_tokens", 0) for call in run["calls"])
            r_cached = sum(call.get("cached_input_tokens", 0) for call in run["calls"])
            tag = "cold " if index == 0 else "resume"
            print(
                f"    run {index} {tag} {run['timestamp']} "
                f"first_call={first_hit:.0f}% run_hit={pct(r_cached, r_input):.1f}%"
            )

    print("\n" + "=" * 78)
    print(f"Pi sessions under {WORKSPACE_ROOT}")
    print("=" * 78)
    pi_input = pi_cached = pi_turns = 0
    pi_sessions = collect_pi()
    for session in pi_sessions:
        miss = sum(call.get("input", 0) for call in session["calls"])
        hit = sum(call.get("cacheRead", 0) for call in session["calls"])
        output = sum(call.get("output", 0) for call in session["calls"])
        pi_input += miss
        pi_cached += hit
        pi_turns += len(session["calls"])
        rates = [
            pct(call.get("cacheRead", 0), call.get("cacheRead", 0) + call.get("input", 0))
            for call in session["calls"]
        ]
        print(
            f"\n{session['path']}\n"
            f"  cwd={session['cwd']}\n"
            f"  turns={len(session['calls'])} miss={miss} hit={hit} output={output}\n"
            f"  session hit={pct(hit, miss + hit):.1f}% "
            f"first_turn={rates[0] if rates else 0:.0f}% "
            f"avg_turn={sum(rates) / len(rates) if rates else 0:.1f}%"
        )

    print("\n" + "=" * 78)
    print("Summary")
    print("=" * 78)
    if total_calls:
        print(
            f"Codex: {len(codex_threads)} threads, {total_calls} API calls, "
            f"hit {pct(total_cached, total_input):.1f}% "
            f"({total_cached}/{total_input} prompt tokens)"
        )
    for phase in ("cold", "resume", "later"):
        samples = [rate for name, rate in codex_phases if name == phase]
        if samples:
            label = {
                "cold": "first call, brand-new thread",
                "resume": "first call, resumed run (new comment)",
                "later": "subsequent call in the same run",
            }[phase]
            print(f"  {label:<42} avg={sum(samples) / len(samples):5.1f}% n={len(samples)}")
    if pi_turns:
        print(
            f"Pi:    {len(pi_sessions)} sessions, {pi_turns} turns, "
            f"hit {pct(pi_cached, pi_input + pi_cached):.1f}% "
            f"({pi_cached}/{pi_input + pi_cached} prompt tokens)"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
