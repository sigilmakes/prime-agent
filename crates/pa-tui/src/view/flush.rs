//! The streaming main-screen flush (TS `exitFullscreen`'s inline
//! repaint) and authoritative restored-transcript replay. Both render the inline frame section
//! by section and append rows to native scrollback in bounded writes.

use super::AgentView;
use crate::Line;

impl AgentView {
    /// Stream the changed rows of the inline layout to `out` as the main-screen flush: the one
    /// shared output path that writes into the user's native scrollback, so its byte stream is
    /// parity-frozen. `self.flushed_frame` is the diff base.
    ///
    /// - rows extending the flushed frame append into scrollback;
    /// - a change above the flushed tail repaints the last screenful
    ///   (scrollback is immutable);
    /// - an identical frame writes nothing.
    ///
    /// # Errors
    ///
    /// Propagates the write error when `out` rejects a chunk: the rows already written have
    /// scrolled, so the flush is not retried.
    pub fn stream_flush_to(
        &mut self,
        out: &mut dyn std::io::Write,
        width: usize,
        screen_height: usize,
    ) -> std::io::Result<()> {
        self.stream_inline_to(out, width, screen_height, FlushMode::Changed)
    }

    /// Append the complete restored inline transcript to native scrollback, regardless of the
    /// previous flush. Only authoritative restores request this; ordinary draws and resizes do not.
    /// The cursor moves below the existing primary-screen contents without erasing them. A
    /// successful replay becomes the next ordinary flush's diff base, preventing a duplicate exit.
    ///
    /// # Errors
    ///
    /// Propagates writer errors. Output may already have scrolled; callers must not retry a failed
    /// replay automatically. The complete diff base is retained even after a partial write error,
    /// so the ordinary exit flush does not silently retry it.
    pub fn stream_replay_to(
        &mut self,
        out: &mut dyn std::io::Write,
        width: usize,
        screen_height: usize,
    ) -> std::io::Result<()> {
        write!(out, "\x1b[{};1H\r\n", screen_height.max(1))?;
        self.stream_inline_to(out, width, screen_height, FlushMode::Replay)
    }

    fn stream_inline_to(
        &mut self,
        out: &mut dyn std::io::Write,
        width: usize,
        screen_height: usize,
        mode: FlushMode,
    ) -> std::io::Result<()> {
        // `layout_pass` is a measurement pass with one mode-flag side effect: it
        // sets `sparse_enabled = false` so the measurement's cache invalidation
        // is dense. A measurement on a live fullscreen view must not keep that
        // false: the flag's contract couples it to a consumed window (`sparse_window`
        // is `None` whenever the flag rests false), and leaving the live window
        // in place wedges the next compose's tail re-anchor behind the
        // `sparse_enabled` gate - every later streaming frame renders the
        // stale pre-edit anchor until a full view rebuild (the Ctrl+G
        // external-editor freeze, 2026-10-28). The in-tree measurement
        // brackets (view/lazy.rs) save and restore around the pass; the
        // flush streamer runs on the live view for the suspend handoff and
        // the replay restore, so it brackets too.
        let sparse_was = self.sparse_enabled;
        let layout = self.layout_pass(width);
        self.sparse_enabled = sparse_was;
        let previous = std::mem::take(&mut self.flushed_frame);
        let mut sink = FlushSink {
            flushed: match mode {
                FlushMode::Changed => previous,
                FlushMode::Replay => Vec::new(),
            },
            texts: Vec::new(),
            ring: std::collections::VecDeque::new(),
            chunk: String::new(),
            screen_height,
            appending: false,
            repaint: false,
            write_error: None,
        };
        sink.feed(out, &layout.splash);
        let mut preceded_by_tool_activity = false;
        for (index, entry) in self.chat.iter().enumerate() {
            let rows =
                self.render_entry(index, entry, width, index == 0, preceded_by_tool_activity);
            sink.feed(out, &rows);
            preceded_by_tool_activity = Self::is_compact_neighbor(entry);
        }
        sink.feed(out, &layout.tail);
        let dock = self.render_dock(width);
        sink.feed(out, &dock);
        let result = sink.finish(out);
        self.flushed_frame = std::mem::take(&mut sink.texts);
        result
    }
}

/// Whether the caller is flushing changes or replaying an authoritative restored transcript.
#[derive(Clone, Copy)]
enum FlushMode {
    Changed,
    Replay,
}

/// The encoded flush rows leave the process in slices of at most this many bytes: each PTY
/// write stays one syscall, and a completed chunk write is the exit guard's progress proof.
const CHUNK_BYTES: usize = 32 * 1024;

/// The streaming main-screen flush state: feeds rows section by section, routes them between
/// the append stream and the repaint ring, writes in bounded chunks.
struct FlushSink {
    /// The last flush's row texts — the diff base.
    flushed: Vec<String>,
    /// The new frame's row texts (the NEXT flush's diff base).
    texts: Vec<String>,
    /// The most recent `screen_height` rows, for the repaint write.
    ring: std::collections::VecDeque<crate::Line>,
    /// The encoded append rows not yet handed to `out`.
    chunk: String,
    screen_height: usize,
    /// Set once a row extends the flushed frame: every later row appends.
    appending: bool,
    /// Set when a row inside the flushed frame changed: every row lands
    /// in the repaint ring instead.
    repaint: bool,
    /// The first failed write. Later rows still establish the diff base, but never retry output.
    write_error: Option<std::io::Error>,
}

impl FlushSink {
    /// Feed one section of the inline frame.
    fn feed(&mut self, out: &mut dyn std::io::Write, rows: &[crate::Line]) {
        for row in rows {
            let index = self.texts.len();
            let text = row_text_of(row);
            if self.appending {
                crate::interactive::write_flush_rows(&mut self.chunk, std::slice::from_ref(row));
                self.texts.push(text);
                if self.chunk.len() >= CHUNK_BYTES {
                    self.drain_chunk(out);
                }
            } else if self.repaint || index >= self.flushed.len() {
                // A changed row turns the write into a repaint; a row
                // past the flushed frame turns it into an append.
                if self.repaint {
                    self.ring_push(row);
                } else {
                    self.appending = true;
                    crate::interactive::write_flush_rows(
                        &mut self.chunk,
                        std::slice::from_ref(row),
                    );
                }
                self.texts.push(text);
            } else {
                if self.flushed[index].as_str() != text.as_str() {
                    self.repaint = true;
                }
                self.ring_push(row);
                self.texts.push(text);
            }
        }
    }

    /// Submit pending bytes in bounded writes, stopping permanently on the first error.
    fn drain_chunk(&mut self, out: &mut dyn std::io::Write) {
        if self.write_error.is_none() {
            for chunk in self.chunk.as_bytes().chunks(CHUNK_BYTES) {
                if let Err(error) = out.write_all(chunk) {
                    self.write_error = Some(error);
                    break;
                }
                crate::exit_guard::note_exit_progress();
            }
        }
        self.chunk.clear();
    }

    /// Keep the repaint ring at one screenful.
    fn ring_push(&mut self, row: &crate::Line) {
        self.ring.push_back(row.clone());
        while self.ring.len() > self.screen_height {
            self.ring.pop_front();
        }
    }

    /// Write what the decided mode owes: the append tail, the repaint
    /// erase plus the ring, or nothing for an identical frame.
    fn finish(&mut self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        if self.appending {
            if !self.chunk.is_empty() {
                self.drain_chunk(out);
            }
        } else if self.repaint || self.texts.len() < self.flushed.len() {
            // A frame that shrank never rewinds into a rewrite of
            // scrollback: the changed region repaints the visible window.
            self.chunk.push_str("\x1b[2J\x1b[H");
            let ring: Vec<crate::Line> = std::mem::take(&mut self.ring).into_iter().collect();
            crate::interactive::write_flush_rows(&mut self.chunk, &ring);
            self.drain_chunk(out);
        }
        match self.write_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Concatenated span contents of a row (includes zero-width OSC zone
/// markers, which must persist into scrollback).
fn row_text_of(line: &Line) -> String {
    line.iter().map(|span| span.content.as_str()).collect()
}

/// Split a string at a char boundary.
pub(super) fn split_at_chars(text: &str, at: usize) -> (&str, &str) {
    let mut end = text.len();
    let mut count = 0;
    for (index, _) in text.char_indices() {
        if count == at {
            end = index;
            break;
        }
        count += 1;
    }
    if count < at {
        return (text, "");
    }
    (&text[..end], &text[end..])
}
