//! Test-only access to a session log line in either on-disk shape.

use serde_json::Value;

/// The row a log line carries.
///
/// The core writes the row itself; logs written before 2026-10-30 wrap it in an
/// envelope. A line is envelope-shaped exactly when it names an envelope
/// member — the writer refuses rows that do — so this mirrors the reader.
pub(crate) fn row_of(line: &str) -> Value {
    let parsed: Value = serde_json::from_str(line).expect("session log line is JSON");
    match parsed.as_object() {
        Some(map)
            if ["seq", "ts", "crc", "row"]
                .iter()
                .any(|key| map.contains_key(*key)) =>
        {
            map.get("row")
                .cloned()
                .expect("an envelope carries its row")
        }
        _ => parsed,
    }
}
