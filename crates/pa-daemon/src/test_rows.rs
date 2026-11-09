//! Test-only access to a session log line in either on-disk shape.

use serde_json::Value;

/// The row a log line carries.
///
/// The core writes the row itself; logs written before 2026-10-30 wrap it in an
/// envelope. Fixtures read lines back, so they need the row whichever they wrote.
pub(crate) fn row_of(line: &str) -> Value {
    let parsed: Value = serde_json::from_str(line).expect("session log line is JSON");
    match (parsed.get("seq"), parsed.get("row")) {
        (Some(_), Some(row)) => row.clone(),
        _ => parsed,
    }
}
