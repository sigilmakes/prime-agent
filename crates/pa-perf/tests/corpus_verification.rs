//! The corpus verification battery: every generated file loads through the
//! PRODUCT's own readers (pa-daemon SessionFile, pa-core parse, the RLM
//! ledger replay), the tree is seed-deterministic, and the marker discipline
//! holds. Kept small and fast: tiny corpora in temp dirs only.

use std::collections::HashSet;
use std::path::Path;

use pa_core::session::read::read_entries;
use pa_daemon::rlm_ledger::RlmSpawnLedger;
use pa_daemon::session_store::SessionFile;
use pa_perf::corpus::{generate, CacheMode, CorpusOptions, Profile};
use pa_types::session::{AgentMessage, FileEntry};

fn tiny(profile: Profile, out: &Path) -> CorpusOptions {
    CorpusOptions {
        out: out.to_path_buf(),
        ..profile.defaults()
    }
}

/// Every generated session file must parse through the daemon's own store.
#[test]
fn sessions_load_through_the_product_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = tiny(Profile::Small, dir.path());
    let manifest = generate(&options).expect("generate");
    let mut checked = 0;
    let mut rows = 0;
    for file in &manifest.files {
        let path = std::path::PathBuf::from(file);
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
            || path.starts_with(manifest.paths.agent_dir.join("rlm-ledger"))
            || path
                .file_name()
                .is_some_and(|name| name == "refinement_history.jsonl")
        {
            continue;
        }
        let store = SessionFile::open(&path).expect("the product store opens the file");
        let entries = read_entries(&path).expect("the SDK validates the same log");
        assert_eq!(store.header.id, path.file_stem().unwrap().to_string_lossy());
        assert!(!store.entries().is_empty(), "the session carried its rows");
        assert_eq!(entries.len(), store.entries().len() + 1);
        rows += store.entries().len();
        checked += 1;
    }
    assert_eq!(
        checked,
        options.sessions + options.archive + manifest.counts.child_sessions
    );
    assert_eq!(
        rows, manifest.counts.total_rows,
        "all native session rows counted"
    );
}

/// No row may degrade to an Unknown entry: the corpus speaks the wire format.
#[test]
fn rows_never_degrade_to_unknown_entries() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = tiny(Profile::Small, dir.path());
    let manifest = generate(&options).expect("generate");
    let huge = &manifest.markers.huge;
    let entries = read_entries(&huge.file).expect("validate the huge session log");
    assert!(
        entries.len() > options.huge_rows,
        "header plus generated rows"
    );
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry, pa_types::session::FileEntry::Unknown { .. })),
        "no unknown rows in the generated huge session"
    );
}

/// The marker lands on the final user row of each marker session.
#[test]
fn markers_land_on_the_final_user_message() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = tiny(Profile::Small, dir.path());
    let manifest = generate(&options).expect("generate");
    for marker in [
        &manifest.markers.huge,
        &manifest.markers.switch_a,
        &manifest.markers.switch_b,
    ] {
        let entries = read_entries(&marker.file).expect("validate marker session log");
        let last_user = entries
            .iter()
            .rev()
            .find_map(|entry| {
                if let FileEntry::Message {
                    message: AgentMessage::User(user),
                    ..
                } = entry
                {
                    Some(&user.content)
                } else {
                    None
                }
            })
            .expect("a final user message");
        let content = serde_json::to_value(last_user).expect("serialize user content");
        let text = content.as_str().or_else(|| content[0]["text"].as_str());
        assert_eq!(
            text,
            Some(marker.needle.as_str()),
            "the final user message is the needle"
        );
    }
}

/// The ledger the corpus writes replays through the product's own reader.
#[test]
fn the_ledger_replays_through_the_product_reader() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = tiny(Profile::Small, dir.path());
    let manifest = generate(&options).expect("generate");
    let ledger = RlmSpawnLedger::new(
        &manifest.paths.agent_dir,
        &manifest.paths.sessions_dir,
        |_| {},
    );
    let edges = ledger.live_edges().expect("the ledger replays");
    assert_eq!(
        edges.len(),
        manifest.counts.subagent_edges,
        "every spawn edge replays as live"
    );
    assert!(
        edges
            .iter()
            .all(|edge| edge.depth >= 1 && edge.child.contains("session-artifacts")),
        "edges carry the artifact-tree child files"
    );
}

/// Same seed, same tree: every file the generator owns is byte-identical
/// (the ledger's `at` stamps come from the product writer's wall clock; the
/// edge content is compared field-by-field instead).
#[test]
fn the_same_seed_rebuilds_the_identical_tree() {
    let first = tempfile::tempdir().expect("temp dir");
    let second = tempfile::tempdir().expect("temp dir");
    let first_root = first.path().canonicalize().expect("canonical first");
    let second_root = second.path().canonicalize().expect("canonical second");
    generate(&tiny(Profile::Small, &first_root)).expect("first generate");
    generate(&tiny(Profile::Small, &second_root)).expect("second generate");
    // The ledger file NAME is the hash of the canonical sessions dir, so
    // two corpus roots name it differently; normalize it for comparison.
    fn normalized(mut files: Vec<String>) -> Vec<String> {
        for file in &mut files {
            if let Some(hash) = file
                .strip_prefix("agent/rlm-ledger/")
                .and_then(|rest| rest.strip_suffix(".jsonl"))
            {
                if !hash.contains('/') {
                    *file = "agent/rlm-ledger/<hash>.jsonl".to_string();
                }
            }
        }
        files
    }
    let first_files = normalized(relative_files(&first_root));
    let second_files = normalized(relative_files(&second_root));
    let mut first_sorted = first_files.clone();
    first_sorted.sort();
    let mut second_sorted = second_files.clone();
    second_sorted.sort();
    assert_eq!(first_sorted, second_sorted, "the file sets match");

    for relative in &first_sorted {
        let a = first_root.join(relative);
        let b = second_root.join(relative);
        if relative.starts_with("agent/rlm-ledger/") {
            let a = ledger_of(&first_root);
            let b = ledger_of(&second_root);
            let edges = |path: &Path, root: &Path| -> Vec<(String, String, u32)> {
                std::fs::read_to_string(path)
                    .expect("read ledger")
                    .lines()
                    .filter_map(|line| {
                        let value: serde_json::Value = serde_json::from_str(line).ok()?;
                        (value["op"] == serde_json::json!("spawn")).then(|| {
                            let child = Path::new(value["child"].as_str().unwrap_or_default());
                            let child = child
                                .strip_prefix(root)
                                .map_or_else(|_| child.to_path_buf(), Path::to_path_buf);
                            (
                                value["childId"].as_str().unwrap_or_default().to_string(),
                                child.to_string_lossy().to_string(),
                                value["depth"].as_u64().unwrap_or_default() as u32,
                            )
                        })
                    })
                    .collect()
            };
            assert_eq!(
                edges(&a, &first_root),
                edges(&b, &second_root),
                "the ledger edges match"
            );
            continue;
        }
        // corpus.json embeds absolute paths and the ledger's hash name; the
        // ledger itself and child headers embed the corpus root in
        // parentSession/child paths. Normalize the root, skip the manifest.
        if relative == "corpus.json" {
            continue;
        }
        let read = |path: &Path, root: &Path| -> Vec<u8> {
            let bytes = std::fs::read(path).expect("read");
            let root = root.to_string_lossy().into_owned().into_bytes();
            let mut normalized = Vec::with_capacity(bytes.len());
            let mut index = 0;
            while index < bytes.len() {
                if bytes[index..].starts_with(&root) {
                    normalized.extend_from_slice(b"<root>");
                    index += root.len();
                } else {
                    normalized.push(bytes[index]);
                    index += 1;
                }
            }
            normalized
        };
        assert_eq!(
            read(&a, &first_root),
            read(&b, &second_root),
            "{relative} differs between seeds-equal runs"
        );
    }
}

/// Entry ids are unique 8-hex counters even past the ~22k-row collision
/// cliff the product's uuid-prefix ids hit.
#[test]
fn entry_ids_stay_unique_past_the_uuid_prefix_collision_cliff() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = CorpusOptions {
        sessions: 3,
        rows: 25_000,
        child_rows: 0,
        fanout: 0,
        depth: 0,
        tree_parents: 0,
        wide_children: 0,
        archive: 0,
        cwd_files: 5,
        ..tiny(Profile::Small, dir.path())
    };
    let manifest = generate(&options).expect("generate");
    let file = manifest
        .paths
        .sessions_dir
        .join(format!("{}.jsonl", manifest.markers.huge.id));
    let entries = read_entries(&file).expect("validate session log");
    let mut ids = HashSet::new();
    let mut rows = 0;
    for entry in entries.iter().skip(1) {
        let id = entry.id().expect("an id").to_string();
        assert_eq!(id.len(), 8, "the id is 8-hex");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "hex id: {id}");
        ids.insert(id);
        rows += 1;
    }
    assert_eq!(ids.len(), rows, "no id collides across {rows} rows");
}

/// The warm pass writes SDK sparse caches, not removed daemon scan caches.
#[test]
fn the_warm_pass_writes_product_sidecars() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut options = tiny(Profile::Small, dir.path());
    options.cache = CacheMode::Warm;
    let manifest = generate(&options).expect("generate");
    let sidecars = manifest
        .files
        .iter()
        .filter(|file| file.ends_with("window-cache.json"))
        .count();
    assert_eq!(sidecars, options.sessions + options.archive);
    for file in &manifest.files {
        let path = Path::new(file);
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
            || !(path.parent() == Some(&manifest.paths.sessions_dir)
                || path.parent() == Some(&manifest.paths.archive_dir))
        {
            continue;
        }
        let window = pa_core::session::window::WindowedSessionStore::open(path)
            .expect("open validated SDK window")
            .expect("corpus is sparse-cache eligible");
        assert!(path.with_extension("window-cache.json").is_file());
        assert!(window.read_stats().cache_hit, "{}", path.display());
        assert!(window.read_stats().cache_bytes > 0);
        if path == manifest.markers.huge.file.as_path() {
            assert!(
                !window.full_history(),
                "huge session retains a compacted suffix"
            );
            assert!(window.read_stats().jsonl_bytes < std::fs::metadata(path).unwrap().len());
            assert_eq!(window.compaction_count(), 1);
        }
    }
}

/// The switch-b row is the ONLY session the switch query can match. The
/// agents-view search is a fuzzy subsequence match over each row's name
/// AND message text, so a common token could rank another row first; the
/// guarantee needs the `zq` token to be literally impossible elsewhere —
/// no generated corpus text (names or message bodies) contains the letter
/// z at all (the ISO `Z` header suffix is the one allowed z).
#[test]
fn the_switch_query_can_only_match_switch_b() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = tiny(Profile::Small, dir.path());
    let manifest = generate(&options).expect("generate");
    assert_eq!(manifest.markers.switch_row, "switch target zqmark");
    assert!(manifest.markers.switch_query.contains("zqmark"));
    assert!(manifest.markers.switch_query != manifest.markers.switch_row);
    let mut scanned = 0;
    for file in &manifest.files {
        let path = std::path::PathBuf::from(file);
        if path.parent() == Some(&manifest.paths.sessions_dir)
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            // The search corpus folds MESSAGE text and names (not header
            // timestamps, whose ISO `Z` suffix is the one allowed z).
            let entries = read_entries(&path).expect("validate session log");
            for entry in entries.iter().skip(1) {
                let line = serde_json::to_string(entry).expect("serialize session row");
                if line.contains('z') && !line.contains(&manifest.markers.switch_row) {
                    panic!("a non-header row carries z: {line}");
                }
            }
            scanned += 1;
        }
    }
    assert_eq!(scanned, options.sessions, "every session checked");
}

/// The corpus settings disable BOTH archive rules: the daemon's boot sweep
/// would otherwise retire corpus sessions out from under the manifest
/// baseline (the defaults keep 200 files / 30 days).
#[test]
fn the_archive_policy_is_off() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = tiny(Profile::Small, dir.path());
    let manifest = generate(&options).expect("generate");
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(manifest.paths.agent_dir.join("settings.json")).expect("settings"),
    )
    .expect("a settings object");
    assert_eq!(
        settings["sessionArchiveMaxAgeDays"],
        serde_json::json!("off")
    );
    assert_eq!(
        settings["sessionArchiveMaxSessions"],
        serde_json::json!("off")
    );
}

/// The corpus manifest records the generated file set (the bench's
/// per-run restore baseline) and the archive dir holds the archived files.
#[test]
fn the_manifest_is_the_file_set_baseline() {
    let dir = tempfile::tempdir().expect("temp dir");
    let options = tiny(Profile::Small, dir.path());
    let manifest = generate(&options).expect("generate");
    let on_disk = relative_files(dir.path());
    assert_eq!(
        manifest.files.len() + 1, // corpus.json is written after the walk
        on_disk.len(),
        "the manifest lists every generated file"
    );
    let archived = on_disk
        .iter()
        .filter(|file| file.starts_with("agent/sessions-archive/") && file.ends_with(".jsonl"))
        .count();
    assert_eq!(archived, options.archive);
}

/// The one ledger file under a corpus root (its name is the sessions-dir
/// hash, which differs per root).
fn ledger_of(root: &Path) -> std::path::PathBuf {
    let dir = root.join("agent/rlm-ledger");
    std::fs::read_dir(dir)
        .expect("read the ledger dir")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("the ledger file")
}

fn relative_files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let entries = std::fs::read_dir(dir).expect("read dir");
        for entry in entries {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .expect("inside root")
                        .to_string_lossy()
                        .to_string(),
                );
            }
        }
    }
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort();
    files
}
