# session_log: the replacement log core

One session, one `<session-id>.jsonl`, one `<session-id>.lock`, one writer at a
time. No compatibility with the old format: a session that fails to load is
reported, not repaired behind the caller's back.

## Invariants

1. **Ownership is the `flock`.** A writer owns the log while it holds
   `LOCK_EX` on the lock file. There are no epochs, owner tokens, prepared
   writes, or acknowledgements. The lock is taken for one operation and
   released; a killed process drops it in the kernel.
2. **A row is a complete line.** `{"seq":N,"ts":"...","crc":"...","row":{...}}`
   is a row only when it is newline-terminated, its envelope parses, its seq
   strictly increases, and the CRC-32 of the seq's 8 little-endian bytes plus
   the raw `row` bytes (never a re-serialization) matches. A line with no
   terminating newline is a torn tail.
3. **Nothing is a row until fsync returns.** `Committed` means the bytes went
   through `fsync(file)`. `Unknown` means the barrier failed, so the core says
   it does not know instead of guessing.
4. **An interrupted write is provably nothing.** `write_all` reports how much
   it wrote only by failing, so a failed write leaves at most a torn line, and
   a torn line can never parse as a row. That is why a write failure is
   `NotWritten` while only a failed barrier is `Unknown`.
5. **The directory is synced on every open.** The log is created in place and
   appended to, so create is the only entry change and there is no rename. But
   the sync cannot be create-only: a creator that dies between creating the file
   and its own directory sync leaves an entry that every later open sees as
   pre-existing, so nothing would ever make that session's existence durable and
   a power loss could take the whole session while its bytes survived. Opens are
   rare, so every open syncs the directory unconditionally.
6. **Sequence numbers outlive a process.** `next_seq` is cached, but every
   append re-checks the file length under the lock and re-reads any lines other
   writers appended, so two writers never share a seq and no cached counter can
   overwrite a committed row.
7. **Reads take no lock.** A reader sees complete rows plus, at worst, a
   half-written line at the end; the scan stops there. Writes are atomic for the
   reader because the line is only valid once its newline is on disk.
8. **A row the reader would reject is never written.** The 16 MiB line cap is
   enforced on both sides. An oversized row is `NotWritten::TooLong` before any
   byte reaches the file, so `Committed` never names a row that a later scan
   would call damage and `repair` would truncate.
9. **A complete but invalid line is damage, not a tail.** An unterminated line
   is how a concurrent append looks and simply ends a read. A newline-terminated
   line that fails the checksum or the order yields one `Error::Corrupt` after
   the rows that preceded it, and `repair()` is the recovery.

## Why there is no handshake

The failure this core replaces was an ownership protocol: prepared writes,
`canonical receipt has not completed`, ACKs, and a `blocked` flag that never
cleared. Every step of that protocol was a promise about another process's
progress, and each promise could be left unfulfilled by a crash, which turned a
transient ordering window into a terminal refusal.

Nothing here depends on another process's progress:

- A writer that is refused gets `Busy` with the holder's self-recorded pid and
  start time; it retries or reports, and the refusal proves nothing about the
  log's content.
- A writer that cannot prove durability gets `Unknown` and a `Recover` handle
  that carries the seq it intended. `recover()` repairs the torn tail, reads the
  log, and answers `Committed` (after a fresh fsync, so the answer is durable) or
  `NotWritten`. The answer comes from the file, not from a peer.
- A retry of the same seq with the same body returns the original outcome, so a
  caller can retry blindly after a crash. Idempotency comes from the log's own
  contents, not from an ACK.

The caller keeps the policy: retry `Busy`, surface `NotWritten`, call
`recover()` on `Unknown`, call `repair()` when `open` reports damage. The core
never waits, never sleeps, and never declares a session dead.

## Deliberate choices

- **No deadline waiting.** `append` takes the caller's deadline and makes one
  non-blocking lock attempt. Waiting for a lock inside the core is the shape of
  the incident, so `Busy` echoes the deadline and the caller decides.
- **`repair()` is explicit.** `open` reports a torn tail as `Error::Damaged`
  rather than truncating on someone's behalf; recovery is a separate, auditable
  step.
- **A damaged middle row truncates the tail.** Per the spec: the log stops at
  the first bad line and `repair()` reports how many bytes it dropped. Silently
  skipping a corrupt row would hide data loss.
- **Rows are JSON objects.** The core rejects a non-object row at the boundary
  instead of writing a line that its own reader would call bad.
- **Idempotency is the seq plus the body, not an op id.** The spec asked for a
  caller-chosen `op_id` with an in-memory `op_id -> seq` map. That map would be
  rebuilt by scanning at open, would cost one entry per row for the life of the
  session, and would be neither shared between two writers nor durable across a
  crash, so it could not be what makes a retry safe. `append_at(seq, row)` is
  strictly stronger: the log itself holds the fact, the comparison is against
  the bytes that are really there, and `Recover` carries the seq out of an
  `Unknown` so the caller never has to remember it. The caller's `op_id` map, if
  it wants one, is its own index over the seqs this returns.
