---
kind: fix
area: daemon
---

A held session lease now re-verifies the on-disk owner token before every
appended mutation and refuses revoked or replaced leases with a typed
revocation error, so a stale holder can no longer double-write the session
behind a successor. Lease release reports its failures (busy guard, unreadable
or unremovable record) instead of swallowing them, and owner records whose
liveness cannot be verified — an unreadable record or a live pid with no
recorded start identity — are reclaimable past a bounded 60-second age window
by the dead-owner sweep and by acquisition, which self-heals over the freed
path. The lease file format is unchanged.
