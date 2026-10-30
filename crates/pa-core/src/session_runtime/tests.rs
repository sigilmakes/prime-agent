//! The behavior the runtime owes: replay, branches, repair, recovery, order.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{Error, Outcome, Pending, ProviderMessage, SessionRuntime};
use crate::session_log::SessionLog;

const ID: &str = "01a18b20-00e5-719c-897c-3e15e8da74a0";

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

fn message(id: &str, parent: Option<&str>, role: &str, content: &Value) -> Value {
    json!({
        "type": "message",
        "id": id,
        "parentId": parent,
        "timestamp": "2026-10-30T05:00:00.000Z",
        "message": { "role": role, "content": content },
    })
}

fn said(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    message(id, parent, role, &json!(text))
}

fn answered(id: &str, parent: &str, text: &str) -> Value {
    message(
        id,
        Some(parent),
        "assistant",
        &json!([{ "type": "text", "text": text }]),
    )
}

fn opened(dir: &Path) -> SessionRuntime {
    SessionRuntime::open(dir, ID, deadline())
        .expect("open session")
        .runtime
}

fn committed(outcome: Outcome) -> u64 {
    match outcome {
        Outcome::Committed { seq, .. } => seq,
        other => panic!("expected a commit, got {other:?}"),
    }
}

fn texts(runtime: &SessionRuntime) -> Vec<String> {
    runtime
        .projection()
        .into_iter()
        .map(|message| message.content)
        .collect()
}

fn message_of(role: &str, content: &str) -> ProviderMessage {
    ProviderMessage {
        role: role.to_owned(),
        content: content.to_owned(),
    }
}

#[test]
fn an_empty_session_opens_empty() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = opened(temp.path());
    assert_eq!(
        (runtime.rows().len(), runtime.last_seq(), runtime.leaf()),
        (0, 0, None)
    );
    assert_eq!(runtime.projection(), []);

    assert_eq!(
        committed(
            runtime
                .append(&said("root", None, "user", "hello"), deadline())
                .unwrap()
        ),
        1
    );
    assert_eq!(runtime.leaf(), Some("root"));
}

#[test]
fn replay_indexes_ten_thousand_rows() {
    let temp = tempfile::tempdir().unwrap();
    let mut parent = None;
    {
        let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
        for n in 0..10_000u64 {
            let id = format!("row-{n}");
            let role = if n % 2 == 0 { "user" } else { "assistant" };
            let body = said(&id, parent.as_deref(), role, &format!("turn {n}"));
            assert_eq!(committed(log.append(&body, deadline())), n + 1);
            parent = Some(id);
        }
    }

    let open = SessionRuntime::open(temp.path(), ID, deadline()).unwrap();
    assert!(open.repair.is_none());
    let runtime = open.runtime;
    assert_eq!(runtime.rows().len(), 10_000);
    assert_eq!(runtime.last_seq(), 10_000);
    assert_eq!(runtime.leaf(), Some("row-9999"));
    assert_eq!(runtime.branch().len(), 10_000);
    assert_eq!(runtime.projection().len(), 10_000);
    let last = &runtime.rows()[9_999];
    assert_eq!(last.id, "row-9999");
    assert_eq!(last.timestamp.as_deref(), Some("2026-10-30T05:00:00.000Z"));
    assert!(last.ts.ends_with('Z'), "the writer stamp is indexed");
}

#[test]
fn a_branch_switch_reprojects_the_selected_leaf() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = opened(temp.path());
    runtime
        .append(&said("root", None, "user", "hello"), deadline())
        .unwrap();
    runtime
        .append(&answered("first", "root", "first answer"), deadline())
        .unwrap();
    runtime
        .append(&answered("second", "root", "second answer"), deadline())
        .unwrap();

    assert_eq!(
        runtime.leaf(),
        Some("first"),
        "a row on another branch does not move the selection"
    );
    assert_eq!(texts(&runtime), ["hello", "first answer"]);
    assert_eq!(runtime.leaves(), ["first", "second"]);

    assert_eq!(committed(runtime.select("second", deadline()).unwrap()), 4);
    assert_eq!(runtime.leaf(), Some("second"));
    assert_eq!(texts(&runtime), ["hello", "second answer"]);

    runtime
        .append(&answered("follow-up", "second", "more"), deadline())
        .unwrap();
    assert_eq!(
        runtime.leaf(),
        Some("follow-up"),
        "a row on the selected branch moves the tip"
    );
    assert_eq!(texts(&runtime), ["hello", "second answer", "more"]);
    assert!(matches!(
        runtime.select("root", deadline()),
        Err(Error::NotALeaf { .. })
    ));
    assert!(matches!(
        runtime.select("missing", deadline()),
        Err(Error::NoSuchRow { .. })
    ));

    drop(runtime);
    let reopened = opened(temp.path());
    assert_eq!(
        reopened.leaf(),
        Some("follow-up"),
        "the selection is a row, so it survives a reopen"
    );
    assert_eq!(texts(&reopened), ["hello", "second answer", "more"]);
}

#[test]
fn a_torn_tail_is_repaired_on_open_and_reported() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(format!("{ID}.jsonl"));
    let good;
    {
        let mut runtime = opened(temp.path());
        runtime
            .append(&said("root", None, "user", "hello"), deadline())
            .unwrap();
        runtime
            .append(&answered("answer", "root", "hi"), deadline())
            .unwrap();
        good = std::fs::metadata(&path).unwrap().len();
    }

    // What a writer killed mid-line leaves behind: no terminating newline.
    {
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(
            br#"{"seq":3,"ts":"2026-10-30T05:00:00.000Z","crc":"00000000","row":{"type":"message","#,
        )
        .unwrap();
        file.sync_all().unwrap();
    }
    let torn = std::fs::metadata(&path).unwrap().len() - good;

    let open = SessionRuntime::open(temp.path(), ID, deadline()).unwrap();
    let repair = open.repair.expect("a torn tail is reported, not kept");
    assert_eq!((repair.last_good_seq, repair.truncated_bytes), (2, torn));

    let mut runtime = open.runtime;
    assert_eq!(runtime.rows().len(), 2);
    assert_eq!(texts(&runtime), ["hello", "hi"]);
    assert_eq!(
        committed(
            runtime
                .append(&answered("next", "answer", "again"), deadline())
                .unwrap()
        ),
        3
    );
}

#[test]
fn an_undecided_append_is_reconciled_from_the_log_once() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = opened(temp.path());
    runtime
        .append(&said("root", None, "user", "hello"), deadline())
        .unwrap();

    // The undecided write reached the file, but the runtime never saw it commit.
    {
        let mut other = SessionLog::open(temp.path(), ID, deadline()).unwrap();
        assert_eq!(
            committed(other.append(&answered("answer", "root", "hi"), deadline())),
            2
        );
    }
    assert_eq!(
        runtime.rows().len(),
        1,
        "an undecided append applies nothing on its own"
    );

    let verdict = |seq| Outcome::Committed {
        seq,
        ts: String::new(),
    };
    assert_eq!(
        committed(runtime.reconcile(verdict(2), deadline()).unwrap()),
        2
    );
    assert_eq!(runtime.rows().len(), 2);
    assert_eq!(runtime.leaf(), Some("answer"));
    assert_eq!(texts(&runtime), ["hello", "hi"]);

    runtime.reconcile(verdict(2), deadline()).unwrap();
    assert_eq!(
        runtime.rows().len(),
        2,
        "the same verdict adds no second row"
    );
    assert!(matches!(
        runtime.reconcile(verdict(3), deadline()),
        Err(Error::MissingRow { seq: 3 })
    ));
}

#[test]
fn a_retried_row_keeps_one_row_and_one_seq() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = opened(temp.path());
    let row = said("root", None, "user", "hello");

    assert_eq!(committed(runtime.append(&row, deadline()).unwrap()), 1);
    // A retry of the row resolves to the seq the session holds.
    assert_eq!(committed(runtime.append(&row, deadline()).unwrap()), 1);
    // A pinned retry is the core's own idempotent path.
    assert_eq!(
        committed(runtime.append_at(1, &row, deadline()).unwrap()),
        1
    );
    assert_eq!(runtime.rows().len(), 1);
    assert_eq!(runtime.read_since(0).unwrap().len(), 1);

    // The same id for a different row is a conflict, not a second row.
    assert!(matches!(
        runtime.append(&said("root", None, "user", "other"), deadline()),
        Err(Error::DuplicateId { .. })
    ));
    // A pinned seq that holds another row is refused; nothing is indexed.
    assert!(matches!(
        runtime
            .append_at(1, &said("other", None, "user", "x"), deadline())
            .unwrap(),
        Outcome::NotWritten { .. }
    ));
    assert_eq!(runtime.rows().len(), 1);
}

#[test]
fn the_projection_follows_the_branch_in_row_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = opened(temp.path());
    runtime
        .append(&said("root", None, "user", "one"), deadline())
        .unwrap();
    runtime
        .append(&answered("two", "root", "two"), deadline())
        .unwrap();
    // Not a message: indexed on the branch, absent from the projection.
    let label = json!({
        "type": "label",
        "id": "label",
        "parentId": "two",
        "timestamp": "2026-10-30T05:00:00.000Z",
        "label": "note",
    });
    runtime.append(&label, deadline()).unwrap();
    let parts = json!([
        { "type": "text", "text": "three" },
        { "type": "image", "data": "ignored" },
    ]);
    runtime
        .append(&message("three", Some("label"), "user", &parts), deadline())
        .unwrap();

    assert_eq!(
        runtime.projection(),
        vec![
            message_of("user", "one"),
            message_of("assistant", "two"),
            message_of("user", "three"),
        ]
    );
}

#[test]
fn two_runtimes_see_each_others_rows() {
    let temp = tempfile::tempdir().unwrap();
    let mut a = opened(temp.path());
    let mut b = opened(temp.path());

    a.append(&said("one", None, "user", "from a"), deadline())
        .unwrap();
    b.append(&answered("two", "one", "from b"), deadline())
        .unwrap();
    assert_eq!(
        b.rows().len(),
        2,
        "b indexed a's row before writing its own"
    );
    assert_eq!(texts(&b), ["from a", "from b"]);

    a.append(&answered("three", "two", "more"), deadline())
        .unwrap();
    assert_eq!(a.rows().len(), 3, "a indexed the row b committed");
    assert_eq!(texts(&a), ["from a", "from b", "more"]);
}

#[test]
fn a_lost_recover_retries_to_one_row_and_the_session_opens() {
    let row = answered("answer", "root", "hi");
    // The write reached the file, or it did not: both are decided from the log,
    // with the pending seq the runtime kept when recover() could not run.
    for landed in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let mut runtime = opened(temp.path());
        runtime
            .append(&said("root", None, "user", "hello"), deadline())
            .unwrap();
        if landed {
            let mut other = SessionLog::open(temp.path(), ID, deadline()).unwrap();
            assert_eq!(committed(other.append_at(2, &row, deadline())), 2);
        }
        runtime.pending = Some(Pending {
            seq: 2,
            id: "answer".to_owned(),
        });
        assert_eq!(committed(runtime.append(&row, deadline()).unwrap()), 2);
        assert_eq!(runtime.pending_seq(), None);
        assert_eq!(runtime.read_since(0).unwrap().len(), 2, "exactly one row");
        drop(runtime);
        assert_eq!(
            opened(temp.path()).rows().len(),
            2,
            "the session still opens with exactly one row (landed: {landed})"
        );
    }
}

#[test]
fn an_oversized_row_is_refused_and_indexes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = opened(temp.path());
    // 17 MiB of row: the core refuses what its reader would call a bad line.
    let oversized = said("big", None, "user", &"x".repeat(17 * 1024 * 1024));
    assert!(matches!(
        runtime.append(&oversized, deadline()).unwrap(),
        Outcome::NotWritten {
            reason: crate::session_log::NotWritten::TooLong { .. }
        }
    ));
    assert_eq!(runtime.rows().len(), 0);
    assert_eq!(runtime.pending_seq(), None);
    drop(runtime);
    assert_eq!(opened(temp.path()).rows().len(), 0);
}

#[test]
fn a_header_row_without_an_id_is_carried_not_indexed() {
    let temp = tempfile::tempdir().unwrap();
    {
        // A real session log starts with the header: a row with a type and a
        // stamp, and no id.
        let mut log = SessionLog::open(temp.path(), ID, deadline()).unwrap();
        let header = json!({
            "type": "session",
            "version": 3,
            "timestamp": "2026-10-30T05:00:00.000Z",
        });
        assert_eq!(committed(log.append(&header, deadline())), 1);
        assert_eq!(
            committed(log.append(&said("root", None, "user", "hello"), deadline())),
            2
        );
    }

    let mut runtime = opened(temp.path());
    assert_eq!(runtime.rows().len(), 1, "the header is not a tree node");
    assert_eq!(runtime.last_seq(), 2, "but its seq is not re-used");
    assert_eq!(runtime.leaf(), Some("root"));
    assert_eq!(texts(&runtime), ["hello"]);
    assert_eq!(
        committed(
            runtime
                .append(&answered("answer", "root", "hi"), deadline())
                .unwrap()
        ),
        3
    );
}
