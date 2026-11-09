# session_log: the replacement log core

One session, one `<session-id>.jsonl`, one `<session-id>.lock`, one writer at a
time. A session file is plain JSONL: every line is one row object, so `jq`,
`agentsview`, and any other reader see the format the session file always had.

## Read-only consumers

`Row::decode` validates one newline-stripped line. Catalog readers keep their own
byte cursor and read resumable derived metadata. They ignore incomplete trailing
lines and never create, lock, fsync, or repair the source log.

## The line is the row

The 2026-10-30 core wrapped every row in
`{"seq":N,"ts":"...","crc":"...","row":{...}}`. That envelope broke every
external reader: a tool that reads session files stops at the first enveloped
file, or prints the envelope instead of the row. It also carried nothing the
product used. `seq` was the line number, `ts` was a second copy of a timestamp
every row already has, and the CRC only chose between `Corrupt` and a torn tail.

So the line is the row again, and these facts follow:

- **A seq is a position, not a field.** The seq of a row is its line number. A
  reader that counts lines and the core that writes them cannot disagree, so
  there is nothing to store and nothing to repair when they would have.
- **A pin is the next line.** `append_at(seq, row)` appends only when the log
  holds exactly `seq - 1` lines. A pin past the end would leave a line whose
  neighbours cannot be numbered, so it is `SeqConflict`, not a gap.
- **Every append writes the row itself.** There is one writer shape. An appended
  line is the row, whether the file already holds envelopes or rows.
- **The envelope still reads.** A line that parses as the old envelope is
  accepted when its checksum validates and its `seq` equals the line's position
  (`line.rs::parse`). The enveloped session files already on disk — 39 in
  `~/.prime/agent/sessions`, of which 35 are closed and 4 are in use — therefore
  load, and a row appended to one of them is read by this core as well. A file
  that holds both shapes reads line by line: each line's position is its own.
- **A half-written envelope is damage, never a row.** A line that names any
  member of the envelope is judged as an envelope: it must have exactly those
  four members, its `seq` must equal its position, and its checksum must be
  exactly eight hexadecimal digits that match. A `null` member names it too. There
  is no fallback to "read it as a row", because a truncated envelope would then
  become a row nobody wrote.
- **A row may not name an envelope member.** The same rule read from the writer's
  side: `body_of` refuses a row whose top level carries `seq`, `ts`, `crc` or
  `row`, so a committed row is always a readable line. The four names are
  reserved at a row's top level.
- **A reader that walks backwards cannot prove position.** It knows byte offsets,
  not line numbers, because a window does not say how many lines precede it. It
  checks only that neighbouring enveloped lines agree with each other
  (`line::BackwardsSequence`); a file whose seqs are all shifted by the same
  amount reads the same. That is why no stored seq is written any more: the
  value is only decidable by the writer that counts its own lines.

What the envelope cost is the checksum, and that is a real loss: a byte flip
inside a JSON string now reads as that different string, where the CRC named it
damage. A flip that breaks the syntax is still damage.

The loss is not that damage goes unhandled. The daemon's repaired-open path,
`open_repaired_dir`, still repairs an envelope-prefixed log whenever `open` calls
it damaged: it truncates at the last line before the damage, reopens, and reports
the byte count, so a checksum mismatch there is still caught and still costs the
tail. That repair is what invariant 10 keeps off a bare file, where the same
operation would delete rows this binary did not write. One thing is therefore
lost with the checksum and is not replaced: a byte flip inside a bare row reads as
the different, valid row it now spells. The case is recorded in
`a_byte_flip_inside_a_string_is_the_damage_a_bare_line_cannot_show`.

## Rolling back

Deleting the stored seq is a one-way change inside this line format, and a
rollback pays for it. This is the cost, stated here rather than discovered:

- **A session created after this change cannot be read by the 2026-10-30 binary.**
  Its first line is a bare row, which that reader does not accept as a row at all;
  the daemon's repaired-open path refuses the session as `LegacyUnsupported` before
  it reads a second line. The bytes are intact and this core reads them.
- **One bare append to an envelope file leaves a file with both shapes.** This core
  reads it, because a row's position is its line number and each line carries its
  own. The 2026-10-30 binary checks only the first line, which is still an
  envelope, then opens the log, calls the first bare line damage, and repairs by
  truncating there — at the last envelope line — reporting the dropped byte count
  to stderr and nowhere else. Every row after that point is gone.
- **Four live sessions are envelope files on this desk's disk today.** A rollback
  that lets the old binary open them loses whatever was appended after the first
  bare row. They are the reason the conversion is a separate, explicit step rather
  than something this core does on open.

The trade is deliberate. A seq and a checksum are the only evidence a line was
intact, and a bare row has neither; the format that carries them cannot be written
by a core that does not know a line's absolute position, and the previous revision
of this branch shipped a reader that claimed to know it. Reading both shapes is
what makes the transition possible without a flag day; the asymmetry in what the
two binaries do with a mixed file is the price.

## Invariants

1. **Ownership is the `flock`.** A writer owns the log while it holds
   `LOCK_EX` on the lock file. There are no epochs, owner tokens, prepared
   writes, or acknowledgements. The lock is taken for one operation and
   released; a killed process drops it in the kernel.
2. **A row is a complete line.** The line's bytes are the row: newline-
   terminated, a JSON object, and — for a line written by the envelope this
   replaced — a valid checksum whose `seq` is the line's position. A line with
   no terminating newline is a torn tail.
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
6. **Sequence numbers outlive a process.** A seq is the line count, cached as
   `next_seq` but never trusted: every append re-checks the file length under
   the lock and re-reads any lines other writers appended, so two writers never
   share a line and no cached count can overwrite a committed row.
7. **Reads take no lock.** A reader sees complete rows plus, at worst, a
   half-written line at the end; the scan stops there. Writes are atomic for the
   reader because the line is only valid once its newline is on disk.
8. **A row the reader would reject is never written.** The 16 MiB line cap is
   enforced on both sides. An oversized row is `NotWritten::TooLong` before any
   byte reaches the file, so `Committed` never names a row that a later scan
   would call damage and `repair` would truncate.
9. **A complete but invalid line is damage, not a tail.** An unterminated line
   is how a concurrent append looks and simply ends a read. A newline-terminated
   line that is not a row yields one `Error::Corrupt` after the rows that
   preceded it.
10. **Repair drops a torn tail; it does not delete history.** A torn tail is a
    write that never became a row, so `repair()` always truncates it, in either
    shape. A complete line that is not a row is truncated only when every line
    *before the damage* was the 2026-10-30 envelope, where the checksum proves the
    line is damage. That condition is about the prefix, not the whole file: a
    malformed line that follows an envelope prefix is truncated even if valid rows
    follow it, because the prefix is evidence and the damaged line is where the
    evidence stops. The reverse case is the one that must not truncate: on a file
    whose prefix is bare rows, the bad line may be history this binary did not
    write — 19 session logs on this desk's disk carry one, with up to 18 MB behind
    it — so `repair()` returns `Error::Corrupt` and changes nothing, and
    `Recover::recover` applies the same rule to this process's undecided append.
    A file whose first complete line is already bad is refused too: with nothing
    before the damage there is no anchor, and a repair would empty it. Cutting
    such a file is an operator's decision, taken with a backup, not an `open`'s.
11. **Nothing rewrites a log.** The core creates, appends, reads and truncates a
    torn tail; it never rewrites a line and never changes a file's shape. Reading
    a log is always possible without writing to it.

## Why there is no handshake

The failure this core replaces was an ownership protocol: prepared writes,
`canonical receipt has not completed`, ACKs, and a `blocked` flag that never
cleared. Every step of that protocol was a promise about another process's
progress, and each promise could be left unfulfilled by a crash, which turned a
transient ordering window into a terminal refusal.

Nothing here depends on another process's progress:

- A writer that is refused gets `Busy` with the holder's self-recorded pid and
  process start time (`Holder::since_ms`); it retries or reports, and the
  refusal proves nothing about the log's content. The note is diagnostics only:
  a holder killed before it wrote one leaves the field empty, and ownership is
  still the `flock`.
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
  step, and on a bare file it refuses to drop anything but a torn tail.
- **A bad line ends the readable prefix, and only an envelope prefix may be
  cut.** The log stops at the first bad line; `repair()` reports how many bytes
  it dropped. Skipping the line and reading on would hide data loss. Cutting the
  tail is a different act, and one this binary does only where a checksum says the
  line was damage: on a file of rows it refuses instead (invariant 10), because
  there the rows behind the bad line are not recoverable from the file.
- **Rows are JSON objects.** The core rejects a non-object row at the boundary
  instead of writing a line that its own reader would call bad.
- **A follower has a cursor, not a rescan.** `refresh` adopts the bytes appended
  since the handle last looked: the cursor starts at the end of the prefix
  `open` scanned, moves past the rows a call returns and no further, and an idle
  call is a length check with no scan. A torn tail leaves the cursor on the last
  complete line (nothing is skipped); a complete bad line stops the cursor
  before it. If a repair truncates below the cursor, the cursor resets to 0 and
  the surviving rows are returned again.
- **Idempotency is the seq plus the body, not an op id.** The spec asked for a
  caller-chosen `op_id` with an in-memory `op_id -> seq` map. That map would be
  rebuilt by scanning at open, would cost one entry per row for the life of the
  session, and would be neither shared between two writers nor durable across a
  crash, so it could not be what makes a retry safe. `append_at(seq, row)` is
  strictly stronger: the log itself holds the fact, the comparison is against
  the bytes that are really there, and `Recover` carries the seq out of an
  `Unknown` so the caller never has to remember it. The caller's `op_id` map, if
  it wants one, is its own index over the seqs this returns.

## Live fold readers

`ReadCursor` is a cloneable, independent byte position for each serving fold.
`read_tail` validates the previous row and returns only newly appended rows under
the operation flock. `append_at_checked` validates the same boundary and appends
under one flock, so repair cannot remove the fold's parent between validation and
mutation. `ReadError::Replaced` requires reopening the fold; it does not set a
blocked state, and a fresh cursor can immediately replay the repaired log.

The cursor checks one prior row plus the new tail: the same body at the same byte
offset. This detects suffix truncation and replacement even if another writer
regrows the file beyond its previous size. It is not a full-history integrity
audit: arbitrary earlier-prefix rewrites that leave the boundary byte-identical
require a full replay to detect. `repair` only truncates a suffix; it never
rewrites that prefix.

A pinned retry that finds its row takes a fresh fsync before reporting Committed.
The surviving bytes alone do not prove the failed original barrier completed.
