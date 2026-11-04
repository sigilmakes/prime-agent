#!/usr/bin/env python3
"""A kernel that starts and then never answers: the in-flight cell never
settles, so the manager keeps it active and refuses the next cell."""
import json
import signal
import sys

# force_abort interrupts the child; this fixture must survive that.
signal.signal(signal.SIGINT, signal.SIG_IGN)

print(json.dumps({"event": "ready", "protocol": 3, "python": "3.13.0"}), flush=True)
for _line in sys.stdin:
    pass
