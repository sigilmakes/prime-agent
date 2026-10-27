---
kind: fix
area: daemon
---

A wire `kill` of a session whose worker was mid-adoption now re-asks the worker
instead of failing with `AdoptionConflict`: the pending caller's ACK lands within
milliseconds, and a hard stop must not be refused by that handshake. A refusal
that outlives the two-second window still stands — an owed accounting close
stays refused.
