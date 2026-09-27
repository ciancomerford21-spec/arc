#!/usr/bin/env python3
"""Pretty-print arcd event lines from stdin (used by voice-live.sh).

Kept as a file rather than inline `python -c` so shell quoting can't break it,
and free of f-string tricks so it runs on any Python 3.8+.
"""

import json
import sys

for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("kind") != "event":
        continue
    e = m.get("event")
    if e == "state":
        print("  [%s]" % m.get("state"))
    elif e == "heard":
        print("heard:  %r" % m.get("text"))
    elif e == "reply":
        print("reply:  %s  (%s)" % (m.get("text"), m.get("route")))
    elif e == "tool_finished":
        r = m.get("record", {})
        print("tool:   %s -> %s" % (r.get("tool"), r.get("outcome")))
    elif e == "error":
        print("ERROR %s: %s" % (m.get("component"), m.get("message")))
    sys.stdout.flush()
