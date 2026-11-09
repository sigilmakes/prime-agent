//! One line on disk: the caller's row bytes, and nothing else.
//!
//! The line *is* the row, so any JSONL reader sees the same `{"type":...}`
//! object the session format has always carried. The sequence number is the
//! line's position, and the row's own `timestamp` is its only stamp.
//!
//! The reader also accepts the envelope the 2026-10-30 core wrote around every
//! row, because the files it wrote are still on disk. Nothing writes one.

use serde::Deserialize;
use serde_json::value::RawValue;

use super::crc::crc32_parts;

/// Longest line the reader accepts; a longer line is a bad line.
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Which shape a line has. The writer has one: [`Format::Bare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// The line is the row.
    Bare,
    /// The 2026-10-30 envelope around the row.
    Envelope,
}

/// One committed row, decoded.
#[derive(Debug, Clone)]
pub struct Row {
    /// The row's line number in the session log, counting from one.
    pub seq: u64,
    /// The row as the caller serialized it.
    pub body: serde_json::Value,
}

impl Row {
    /// Decode one complete log line that follows `after_seq` rows. The caller
    /// removes the terminating newline; an unterminated tail must not be
    /// passed here. This read-only decoder never repairs or locks a log.
    ///
    /// # Errors
    ///
    /// Returns [`super::Error::Corrupt`] for an invalid line, row body, size,
    /// or (for an envelope written before this format) a checksum or sequence
    /// that disagrees with the line's position.
    pub fn decode(line: &[u8], after_seq: u64) -> Result<Self, super::Error> {
        let corrupt = || super::Error::Corrupt { after_seq };
        if line.len() >= MAX_LINE_BYTES {
            return Err(corrupt());
        }
        let (_, body) = parse(line, after_seq).ok_or_else(corrupt)?;
        Ok(Self {
            seq: after_seq + 1,
            body: serde_json::from_str(body).map_err(|_| corrupt())?,
        })
    }
}

/// The members of the 2026-10-30 envelope as the line holds them, plus what a
/// strict reading needs to know: whether the line *names* any of them (so a
/// `null` counts, which an `Option` field cannot show), whether it names
/// anything else, and whether one of them appears twice.
#[derive(Default)]
struct Probe<'a> {
    named: bool,
    other: bool,
    duplicate: bool,
    seq: Option<&'a RawValue>,
    ts: Option<&'a RawValue>,
    crc: Option<&'a RawValue>,
    row: Option<&'a RawValue>,
}

impl<'de> Deserialize<'de> for Probe<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Members;

        impl<'de> serde::de::Visitor<'de> for Members {
            type Value = Probe<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a session log line")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Probe<'de>, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut probe = Probe::default();
                while let Some(key) = map.next_key::<&str>()? {
                    let slot = match key {
                        "seq" => Some(&mut probe.seq),
                        "ts" => Some(&mut probe.ts),
                        "crc" => Some(&mut probe.crc),
                        "row" => Some(&mut probe.row),
                        _ => None,
                    };
                    if let Some(slot) = slot {
                        probe.named = true;
                        probe.duplicate |= slot.is_some();
                        // The value is kept raw: the checksum covers the row's
                        // exact bytes, so it must never be re-serialized.
                        *slot = Some(map.next_value::<&RawValue>()?);
                    } else {
                        probe.other = true;
                        map.next_value::<&RawValue>()?;
                    }
                }
                Ok(probe)
            }
        }

        deserializer.deserialize_map(Members)
    }
}

impl<'a> Probe<'a> {
    fn of(line: &'a [u8]) -> Option<Self> {
        serde_json::from_slice(line).ok()
    }

    /// Whether the line names any part of the envelope. Such a line is judged
    /// as an envelope and nothing else: a half-written envelope must never be
    /// mistaken for a row. A `null` member counts as naming it.
    const fn envelope_shaped(&self) -> bool {
        self.named
    }

    /// The declared seq and the row's exact bytes, when this line is a whole
    /// envelope: exactly the four members, each of the right kind, and a
    /// checksum that covers the seq and the row.
    fn envelope(&self) -> Option<(u64, &'a str)> {
        if self.other || self.duplicate {
            return None;
        }
        let seq: u64 = serde_json::from_str(self.seq?.get()).ok()?;
        // `ts` is accepted and discarded: the row carries its own timestamp.
        let _: String = serde_json::from_str(self.ts?.get()).ok()?;
        let checksum: String = serde_json::from_str(self.crc?.get()).ok()?;
        let checksum = parse_checksum(&checksum)?;
        let body = self.row?.get();
        if !body.starts_with('{') || checksum_of(seq, body) != checksum {
            return None;
        }
        Some((seq, body))
    }
}

/// The envelope's checksum field: exactly eight hexadecimal digits, which is
/// what the 2026-10-30 core wrote. A longer or shorter field is not that field,
/// even when it parses to the same number.
fn parse_checksum(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    if bytes.len() != 8 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

/// Whether a row's top level names an envelope member, which would make the
/// line it is written to unreadable. The writer refuses such a row.
pub(crate) fn reserved_name(body: &[u8]) -> bool {
    Probe::of(body).is_some_and(|probe| probe.envelope_shaped())
}

/// Validate one line and return its shape and row bytes, borrowing the line
/// buffer.
pub(crate) fn parse(line: &[u8], after_seq: u64) -> Option<(Format, &str)> {
    let probe = Probe::of(line)?;
    if !probe.envelope_shaped() {
        return Some((Format::Bare, std::str::from_utf8(line).ok()?));
    }
    // Every part of an envelope must be right, including that it has no
    // members beyond the four: an unknown member means this is not the
    // envelope this reader knows. A bare row cannot name a sequence, so its
    // position is the only seq there is; an envelope that disagrees with its
    // own position is damage.
    let (seq, body) = probe.envelope()?;
    if seq != after_seq.checked_add(1)? {
        return None;
    }
    Some((Format::Envelope, body))
}

/// Which shape this line has, without judging whether it is valid.
#[cfg(test)]
pub(crate) fn shape(line: &[u8]) -> Option<Format> {
    Probe::of(line).map(|probe| {
        if probe.envelope_shaped() {
            Format::Envelope
        } else {
            Format::Bare
        }
    })
}

/// Validate one line whose position in the log is not known, returning the seq
/// an envelope declares when the line is one.
///
/// A reader that walks a log backwards knows a line's byte offset but not how
/// many precede it, so position is proven by chaining: each envelope it meets
/// must declare one less than the one after it. [`BackwardsSequence`] is that
/// chain, and a reader must call it; without it a wrong seq reads fine here.
pub(crate) fn parse_unpositioned(line: &[u8]) -> Option<(Option<u64>, &str)> {
    let probe = Probe::of(line)?;
    if !probe.envelope_shaped() {
        return Some((None, std::str::from_utf8(line).ok()?));
    }
    let (seq, body) = probe.envelope()?;
    if seq == 0 {
        return None;
    }
    Some((Some(seq), body))
}

/// The consistency check a backwards walk can make when it does not know a
/// line's number.
///
/// A tail window knows byte offsets, not how many lines precede them, so it
/// cannot prove an absolute position: a file whose seqs are all shifted by the
/// same amount reads exactly the same. What it can check is that neighbouring
/// envelope lines agree — within one run of them, each declares one less than
/// the line below it. A bare row has no seq, so it ends the run.
#[derive(Debug, Default)]
pub(crate) struct BackwardsSequence {
    expected: Option<u64>,
}

impl BackwardsSequence {
    /// Accept one line's declared seq, or refuse it.
    ///
    /// # Errors
    ///
    /// `Err(())` when an envelope's seq is not one less than the envelope below
    /// it. This is a local consistency check, not proof of position.
    pub(crate) fn accept(&mut self, seq: Option<u64>) -> Result<(), ()> {
        let Some(seq) = seq else {
            // A bare row carries no position to compare, and the rows under it
            // are a different run.
            self.expected = None;
            return Ok(());
        };
        if self.expected.is_some_and(|expected| expected != seq) {
            return Err(());
        }
        self.expected = seq.checked_sub(1);
        Ok(())
    }
}
/// Serialize a caller row; only JSON objects are rows, and JSON escaping keeps
/// every row on one line.
pub(crate) fn body_of<T: serde::Serialize>(row: &T) -> Result<String, String> {
    let body = serde_json::to_string(row).map_err(|error| error.to_string())?;
    if !body.starts_with('{') {
        return Err("a session log row must serialize to a JSON object".to_owned());
    }
    // The reader judges a line that names any envelope member as an envelope,
    // so a row carrying one would be committed and then be unreadable, taking
    // the rest of the log with it. Refused before the disk is touched.
    if reserved_name(body.as_bytes()) {
        return Err(
            "a session log row must not carry seq, ts, crc or row at its top level".to_owned(),
        );
    }
    Ok(body)
}

/// The envelope's checksum: the seq's 8 little-endian bytes plus the row's
/// exact bytes. A bare line has no checksum, so this serves the envelope only.
pub(crate) fn checksum_of(seq: u64, body: &str) -> u32 {
    crc32_parts(&[&seq.to_le_bytes(), body.as_bytes()])
}

/// One bare line, newline included.
pub(crate) fn encode_bare(body: &str) -> Vec<u8> {
    let mut line = Vec::with_capacity(body.len() + 1);
    line.extend_from_slice(body.as_bytes());
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::{encode_bare, parse, shape, Format};

    /// The envelope the 2026-10-30 core wrote, for tests that must read one.
    fn envelope(seq: u64, body: &str) -> String {
        format!(
            r#"{{"seq":{seq},"ts":"2026-10-30T04:00:00.000Z","crc":"{:08x}","row":{body}}}"#,
            super::checksum_of(seq, body)
        )
    }

    #[test]
    fn a_bare_line_is_its_own_row() {
        assert_eq!(
            parse(br#"{"type":"message","id":"a"}"#, 3).unwrap().1,
            r#"{"type":"message","id":"a"}"#
        );
        assert_eq!(shape(br#"{"type":"message"}"#), Some(Format::Bare));
    }

    #[test]
    fn an_envelope_yields_the_row_it_wraps() {
        let body = r#"{"type":"message","id":"b"}"#;
        let line = envelope(2, body);
        assert_eq!(parse(line.as_bytes(), 1).unwrap().1, body);
        assert_eq!(shape(line.as_bytes()), Some(Format::Envelope));
        // The same envelope one line earlier is damage: its seq disagrees.
        assert!(parse(line.as_bytes(), 0).is_none());
        // A flipped byte in the row is caught by the envelope's checksum.
        assert!(parse(line.replace("id", "ie").as_bytes(), 1).is_none());
    }

    #[test]
    fn a_partial_envelope_is_damage_and_never_a_row() {
        let body = r#"{"type":"message","id":"c"}"#;
        let full = envelope(1, body);
        // Every way an envelope can be half-written: a missing member, an
        // extra one, a bad checksum, a seq naming another line.
        for broken in [
            r#"{"seq":1,"row":{"type":"message"}}"#.to_owned(),
            r#"{"seq":1,"ts":"t","row":{"type":"message"}}"#.to_owned(),
            r#"{"seq":1,"ts":"t","crc":"00000000","row":{"type":"message"}}"#.to_owned(),
            format!(r#"{{"seq":1,"ts":"t","crc":"00000000","row":{body},"extra":1}}"#),
            full.replace(r#""seq":1"#, r#""seq":2"#),
            format!(
                r#"{{"seq":1,"ts":"t","crc":"{:08x}","row":{body}}}"#,
                super::checksum_of(1, body) ^ 1
            ),
        ] {
            assert!(parse(broken.as_bytes(), 0).is_none(), "{broken}");
        }
    }

    #[test]
    fn a_non_object_or_a_trailing_byte_is_not_a_row() {
        assert!(parse(br#"{"type":"message"} {"x":1}"#, 0).is_none());
        assert!(parse(br#"["type","message"]"#, 0).is_none());
        assert!(parse(b"", 0).is_none());
    }

    #[test]
    fn both_shapes_round_trip_through_their_encoder() {
        let body = r#"{"type":"message","content":"line\nbreak"}"#;
        let bare = encode_bare(body);
        assert_eq!(bare.last(), Some(&b'\n'));
        assert_eq!(parse(&bare[..bare.len() - 1], 0).unwrap().1, body);

        let wrapped = envelope(7, body);
        assert_eq!(parse(wrapped.as_bytes(), 6).unwrap().1, body);
        assert!(parse(wrapped.as_bytes(), 7).is_none());
    }
    #[test]
    fn a_null_envelope_member_still_marks_the_line_as_an_envelope() {
        // `Option` fields cannot tell an absent member from a null one, so the
        // probe records the name instead. A line that says `"seq":null` is
        // claiming the envelope and must not be read as a row.
        for line in [
            r#"{"seq":null,"type":"message"}"#,
            r#"{"crc":null,"type":"message"}"#,
            r#"{"row":null,"type":"message"}"#,
            r#"{"ts":null,"type":"message"}"#,
        ] {
            assert_eq!(shape(line.as_bytes()), Some(Format::Envelope), "{line}");
            assert!(parse(line.as_bytes(), 0).is_none(), "{line}");
        }
        // And a row that carries one is refused by the writer-facing check.
        assert!(super::reserved_name(br#"{"type":"message","seq":null}"#));
        assert!(!super::reserved_name(br#"{"type":"message","id":"a"}"#));
    }

    #[test]
    fn a_backwards_walk_checks_that_neighbouring_envelopes_agree() {
        use super::BackwardsSequence;

        let mut correct = BackwardsSequence::default();
        for seq in [4, 3, 2, 1] {
            assert!(correct.accept(Some(seq)).is_ok(), "{seq}");
        }
        // Every line under the first must be one less than the one above it.
        let mut skipped = BackwardsSequence::default();
        assert!(skipped.accept(Some(4)).is_ok());
        assert!(skipped.accept(Some(2)).is_err());
        let mut repeated = BackwardsSequence::default();
        assert!(repeated.accept(Some(4)).is_ok());
        assert!(repeated.accept(Some(4)).is_err());
        // A bare row ends the run; the envelopes under it start a new one, which
        // this window cannot relate to the first.
        let mut mixed = BackwardsSequence::default();
        assert!(mixed.accept(Some(4)).is_ok());
        assert!(mixed.accept(None).is_ok());
        assert!(mixed.accept(Some(2)).is_ok());
        assert!(mixed.accept(Some(1)).is_ok());
        // A whole bare file has nothing to check.
        let mut bare = BackwardsSequence::default();
        for _ in 0..3 {
            assert!(bare.accept(None).is_ok());
        }
    }
}
