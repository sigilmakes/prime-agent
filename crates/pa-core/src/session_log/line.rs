//! One line on disk: a fixed envelope around the caller's raw row bytes.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::value::RawValue;

use super::crc::crc32_parts;

/// Longest line the reader accepts; a longer line is a bad line.
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// One committed row, decoded.
#[derive(Debug, Clone)]
pub struct Row {
    /// Strictly increasing within one session log.
    pub seq: u64,
    /// The writer's stamp, `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
    pub ts: String,
    /// The row as the caller serialized it.
    pub body: serde_json::Value,
}

/// A validated line, borrowing the line buffer.
pub(crate) struct Parsed<'a> {
    pub(crate) seq: u64,
    pub(crate) ts: &'a str,
    pub(crate) body: &'a str,
}

#[derive(serde::Deserialize)]
struct Wire<'a> {
    seq: u64,
    ts: &'a str,
    crc: &'a str,
    #[serde(borrow)]
    row: &'a RawValue,
}

/// Validate the envelope, the checksum, and the row shape of one line.
pub(crate) fn parse(line: &[u8]) -> Option<Parsed<'_>> {
    let wire: Wire<'_> = serde_json::from_slice(line).ok()?;
    let checksum = u32::from_str_radix(wire.crc, 16).ok()?;
    let body = wire.row.get();
    if checksum_of(wire.seq, body) != checksum || !body.starts_with('{') {
        return None;
    }
    Some(Parsed {
        seq: wire.seq,
        ts: wire.ts,
        body,
    })
}

/// Serialize a caller row; only JSON objects are rows.
pub(crate) fn body_of<T: Serialize>(row: &T) -> Result<String, String> {
    let body = serde_json::to_string(row).map_err(|error| error.to_string())?;
    if !body.starts_with('{') {
        return Err("a session log row must serialize to a JSON object".to_owned());
    }
    Ok(body)
}

/// The checksum covers the seq and the row's exact bytes, so a flipped seq is
/// caught as surely as a flipped byte in the row. The seq joins as its 8
/// little-endian bytes: no decimal formatting can change the checksum.
pub(crate) fn checksum_of(seq: u64, body: &str) -> u32 {
    crc32_parts(&[&seq.to_le_bytes(), body.as_bytes()])
}

/// One wire line, newline included. `ts` is the writer's own timestamp and
/// carries no JSON metacharacters.
pub(crate) fn encode(seq: u64, ts: &str, body: &str) -> Vec<u8> {
    debug_assert!(ts
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\'));
    let checksum = checksum_of(seq, body);
    let mut line = String::with_capacity(body.len() + 96);
    let _ = write!(
        line,
        "{{\"seq\":{seq},\"ts\":\"{ts}\",\"crc\":\"{checksum:08x}\",\"row\":"
    );
    line.push_str(body);
    line.push_str("}\n");
    line.into_bytes()
}

/// The writer's stamp for a new row: `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
pub(crate) fn now_iso8601() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let (year, month, day, hour, minute, second) = civil_from_unix((millis / 1000) as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        millis % 1000
    )
}

/// Howard Hinnant's civil-from-days conversion.
fn civil_from_unix(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let hour = (remainder / 3600) as u32;
    let minute = ((remainder % 3600) / 60) as u32;
    let second = (remainder % 60) as u32;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}
