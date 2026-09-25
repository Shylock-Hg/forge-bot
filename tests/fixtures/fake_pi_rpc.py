#!/usr/bin/env python3
"""Minimal pi RPC stand-in for the pi_rpc pool tests."""

import json
import os
import sys
import time

delay = float(os.environ.get("FAKE_PI_DELAY", "0"))
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except ValueError:
        continue
    kind = message.get("type")
    request_id = message.get("id")
    if kind == "prompt":
        time.sleep(delay)
        print(json.dumps({"type": "response", "id": request_id, "success": True}), flush=True)
        print(json.dumps({"type": "agent_settled"}), flush=True)
    elif kind == "get_last_assistant_text":
        print(
            json.dumps(
                {
                    "type": "response",
                    "id": request_id,
                    "data": {"text": "fake-result"},
                }
            ),
            flush=True,
        )
