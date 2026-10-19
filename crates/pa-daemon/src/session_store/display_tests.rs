//! Display projection regressions use the on-disk public read seam.
use super::*;

fn display_fixture(rows: Vec<Value>) -> (tempfile::TempDir, SessionFile) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.jsonl");
    let mut lines = vec![json!({"type":"session","version":3,"id":"display","cwd":"/tmp","timestamp":"1970-01-01T00:00:01.000Z"}).to_string()];
    lines.extend(rows.into_iter().map(|row| row.to_string()));
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let store = SessionFile::open(&path).unwrap();
    (dir, store)
}

fn display_row(kind: &str, id: &str, parent: Option<&str>, fields: Value) -> Value {
    let mut row =
        json!({"type":kind,"id":id,"parentId":parent,"timestamp":"1970-01-01T00:00:01.000Z"});
    let Value::Object(fields) = fields else {
        panic!("fixture fields must be an object");
    };
    row.as_object_mut().unwrap().extend(fields);
    row
}

#[test]
fn display_projection_keeps_chronology_and_branch_local_receipts_without_changing_model_context() {
    let sent = json!({"id":"receipt","message":"sent","deliveryStatus":"queued","target":{"activeSessionId":"active","sessionId":"session","sessionName":"child"}});
    let existing = json!({"id":"existing","message":"live","deliveryStatus":"delivered","target":{"activeSessionId":"live","sessionId":"live"}});
    let user = json!({"role":"user","content":"before compaction","timestamp":1});
    let result = json!({"role":"toolResult","toolCallId":"call","toolName":"ipython","content":[],"details":{"kept":42,"sentAgentMessages":[existing]},"timestamp":2});
    let visible = json!({"role":"custom","customType":"visible","content":"shown","display":true,"details":{"n":1},"timestamp":1000});
    let embedded = json!({"role":"custom","customType":"embedded","display":true,"content":"visible embedded","timestamp":4});
    let bash = json!({"role":"toolResult","toolCallId":"call","toolName":"bash","content":[],"timestamp":5});
    let mut duplicate = existing.clone();
    duplicate["message"] = json!("must not replace live receipt");
    let (_dir, store) = display_fixture(vec![
        display_row("message", "root", None, json!({"message":user})),
        display_row(
            "custom",
            "sibling",
            Some("root"),
            json!({"customType":"ipython_sent_agent_message","data":{"toolCallId":"call","message":{"id":"SECRET","message":"SIBLING","deliveryStatus":"delivered","target":{"activeSessionId":"other","sessionId":"other"}}}}),
        ),
        display_row("message", "result", Some("root"), json!({"message":result})),
        display_row(
            "custom_message",
            "hidden",
            Some("result"),
            json!({"customType":"hidden","content":"hidden","display":false}),
        ),
        display_row(
            "custom_message",
            "shown",
            Some("hidden"),
            json!({"customType":"visible","content":"shown","display":true,"details":{"n":1}}),
        ),
        display_row(
            "compaction",
            "compact1",
            Some("shown"),
            json!({"summary":"first","tokensBefore":100,"firstKeptEntryId":"result","customInstructions":"instructions"}),
        ),
        display_row(
            "custom",
            "receipt",
            Some("compact1"),
            json!({"customType":"ipython_sent_agent_message","data":{"toolCallId":"call","message":sent}}),
        ),
        display_row(
            "custom",
            "duplicate",
            Some("receipt"),
            json!({"customType":"ipython_sent_agent_message","data":{"toolCallId":"call","message":duplicate}}),
        ),
        display_row(
            "custom",
            "invalid",
            Some("duplicate"),
            json!({"customType":"ipython_sent_agent_message","data":{"toolCallId":"call","message":{"id":"invalid","message":"bad","deliveryStatus":"failed","target":{"activeSessionId":"other","sessionId":"other"}}}}),
        ),
        display_row(
            "branch_summary",
            "branch",
            Some("invalid"),
            json!({"summary":"returned branch","fromId":"sibling"}),
        ),
        display_row(
            "message",
            "embedded_hidden",
            Some("branch"),
            json!({"message":{"role":"custom","display":false,"content":"secret"}}),
        ),
        display_row(
            "message",
            "embedded",
            Some("embedded_hidden"),
            json!({"message":embedded}),
        ),
        display_row(
            "compaction",
            "compact2",
            Some("embedded"),
            json!({"summary":"second","tokensBefore":200,"firstKeptEntryId":"embedded"}),
        ),
        display_row("message", "bash", Some("compact2"), json!({"message":bash})),
    ]);
    let model = store.messages();
    let mut enriched = result;
    enriched["details"]["sentAgentMessages"] = json!([existing, sent]);
    let expected = vec![
        user,
        enriched,
        visible,
        json!({"role":"compactionSummary","summary":"first","tokensBefore":100,"customInstructions":"instructions","timestamp":1000}),
        json!({"role":"branchSummary","summary":"returned branch","fromId":"sibling","timestamp":1000}),
        embedded,
        json!({"role":"compactionSummary","summary":"second","tokensBefore":200,"timestamp":1000}),
        bash,
    ];
    assert_eq!(store.display_messages().unwrap(), expected);
    assert_eq!(store.display_messages_before(2).unwrap(), expected[..2]);
    let window = store.windowed_display_messages(3).unwrap();
    assert_eq!(
        (window.omitted, window.messages),
        (4, expected[4..].to_vec())
    );
    assert_eq!(
        store.messages(),
        model,
        "display-only means no model mutation"
    );
    assert_eq!(model[0]["summary"], "second");
    assert_eq!(
        store.windowed_display_messages(0).unwrap().messages,
        Vec::<Value>::new()
    );
}

#[test]
fn display_projection_refuses_missing_parents_and_cycles() {
    for rows in [
        vec![display_row(
            "message",
            "leaf",
            Some("missing"),
            json!({"message":{"role":"user","content":"incomplete"}}),
        )],
        vec![
            display_row(
                "message",
                "a",
                Some("b"),
                json!({"message":{"role":"user","content":"a"}}),
            ),
            display_row(
                "message",
                "b",
                Some("a"),
                json!({"message":{"role":"user","content":"b"}}),
            ),
        ],
    ] {
        let (_dir, store) = display_fixture(rows);
        assert!(store.display_messages().is_err());
        assert!(store.display_messages_before(1).is_err());
        assert!(store.windowed_display_messages(1).is_err());
    }
}
