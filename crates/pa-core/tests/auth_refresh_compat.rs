//! Process-level auth regressions. Every manager runs in an env-cleared child.
//! No provider transports, personal paths, sockets, or credential-bearing logs.

// Equality failures must not Debug-print credentials or complete auth documents.
// Keep the assertion context redacted instead of using assert_eq!/assert_ne!.
#![allow(clippy::manual_assert_eq)]
// These process fixtures verify the Unix identity/heartbeat protocol, not the
// explicitly unsupported portable rotating-refresh branch.
#![cfg(unix)]

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_core::auth::{
    AuthCredential, AuthStorage, AuthStorageBackend, AuthStorageData, FileAuthStorageBackend,
    OAuthIntegration, PrimeTeamAssignment, PrimeTeamCredential,
};
use serde_json::{json, Value};

const PROVIDER: &str = "auth-refresh-compat-dummy";
const FRESH_ACCESS: &str = "fixture-fresh-access";
const DEADLINE: Duration = Duration::from_secs(35);

fn wait_for(path: &Path) {
    let end = Instant::now() + DEADLINE;
    while !path.exists() {
        assert!(Instant::now() < end, "coordination deadline expired");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn mark(root: &Path, name: &str) {
    fs::write(root.join(name), b"ready").unwrap();
}

fn document(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("auth.json")).unwrap()).unwrap()
}

fn expired() -> Value {
    json!({"type": "oauth", "access": "fixture-expired-access",
        "refresh": "fixture-single-use-refresh", "expires": 1,
        "accountId": "fixture-account", "enterpriseUrl": "fixture.invalid",
        "opaqueOAuth": {"nested": [null, false, {"keep": "exact"}]}})
}

fn replacement() -> AuthCredential {
    serde_json::from_value(json!({"type": "api_key", "key": "fixture-replacement"})).unwrap()
}

fn fresh() -> AuthCredential {
    serde_json::from_value(json!({"type": "oauth", "access": FRESH_ACCESS,
        "refresh": "fixture-rotated-refresh", "expires": 4_000_000_000_000_i64,
        "accountId": "fixture-account", "enterpriseUrl": "fixture.invalid"}))
    .unwrap()
}

fn seed(root: &Path) -> Value {
    let initial = json!({
        (PROVIDER): expired(),
        "mcp:compat-static": {"type": "mcp_static_token", "bearer": "fixture-static-bearer",
            "endpoint": "https://mcp.fixture.invalid", "bearerFieldId": "fixture-bearer-field",
            "createdAt": "2026-01-01T00:00:00.000Z"},
        "prime-inference": {"type": "api_key", "key": "fixture-prime-key",
            "primeTeam": {"teamId": "old-team", "name": "Old team", "slug": null,
                "role": null, "createdAt": null,
                "opaquePrimeTeam": {"nested": [null, false, {"keep": "exact"}]}},
            "opaquePrime": {"nested": [0, null, {"keep": true}]}},
        "unrelated-provider": {"type": "future_kind", "opaque": [null, {"keep": 17}]}
    });
    FileAuthStorageBackend::new(root.join("auth.json"))
        .with_lock(&mut |_| Ok(((), Some(initial.to_string()))))
        .unwrap();
    initial
}

struct FakeOAuth {
    root: PathBuf,
    actor: String,
}

impl OAuthIntegration for FakeOAuth {
    fn api_key_for(&self, provider: &str, credential: &AuthCredential) -> Option<String> {
        // The real manager uses an empty provider for stored-value fingerprints.
        if !provider.is_empty() && provider != PROVIDER {
            return None;
        }
        match credential {
            AuthCredential::Oauth { access, .. } => Some(access.clone()),
            AuthCredential::ApiKey { .. } | AuthCredential::McpStaticToken { .. } => None,
        }
    }

    fn refresh(&self, provider: &str, credentials: &AuthStorageData) -> Option<AuthCredential> {
        assert!(provider == PROVIDER, "unexpected fake provider");
        assert!(
            credentials.get(PROVIDER) == Some(&expired()),
            "unexpected fake input"
        );
        mark(&self.root, &format!("entered-{}", self.actor));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join("spent"))
        {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                mark(&self.root, &format!("duplicate-{}", self.actor));
                return None;
            }
            Err(error) => panic!("fake token ledger failed: {error}"),
        }
        if !self.root.join("unheld").exists() {
            wait_for(&self.root.join("release"));
        }
        Some(fresh())
    }
}

fn manager(root: &Path, actor: &str) -> AuthStorage {
    let auth = AuthStorage::create_with_oauth(
        root,
        Arc::new(FakeOAuth {
            root: root.to_path_buf(),
            actor: actor.to_string(),
        }),
    );
    assert!(
        auth.load_error().is_none(),
        "isolated manager failed to load"
    );
    auth
}

/// No inherited credential variables, PATH lookup, package discovery, or personal home.
fn isolated_command(executable: &Path, root: &Path) -> Command {
    assert!(executable.is_absolute(), "child executable must be pinned");
    let mut command = Command::new(executable);
    command.env_clear().current_dir(root).stdin(Stdio::null());
    for key in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "PRIME_CODING_AGENT_DIR",
        "PI_CODING_AGENT_DIR",
    ] {
        let path = root.join(key);
        fs::create_dir_all(&path).unwrap();
        command.env(key, path);
    }
    command.env("TMPDIR", root).env("PA_AUTH_COMPAT_ROOT", root);
    // These two explicit test inputs are the complete inherited allowlist.
    for key in ["PA_TS_PACKAGE_DIR", "PA_TEST_NODE"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

fn spawn_rust(root: &Path, mode: &str, actor: &str) -> Child {
    isolated_command(&std::env::current_exe().unwrap(), root)
        .args([
            "--exact",
            "auth_refresh_compat_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PA_AUTH_COMPAT_MODE", mode)
        .env("PA_AUTH_COMPAT_ACTOR", actor)
        .spawn()
        .unwrap()
}

fn finish(child: &mut Child) {
    let end = Instant::now() + Duration::from_secs(50);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated auth child failed: {status}");
            return;
        }
        assert!(Instant::now() < end, "child completion deadline expired");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run(mode: &str) {
    let base = PathBuf::from(std::env::var_os("TMPDIR").expect("explicit TMPDIR required"));
    assert!(base.is_absolute(), "TMPDIR must be absolute");
    let root = tempfile::Builder::new()
        .prefix("auth-refresh-compat-")
        .tempdir_in(base)
        .unwrap();
    let mut child = spawn_rust(root.path(), mode, "driver");
    finish(&mut child);
}

#[test]
fn native_processes_spend_refresh_once_and_both_receive_fresh_credentials() {
    run("race");
}

#[test]
fn cooperative_remove_cannot_resurrect_after_held_refresh() {
    run("remove");
}

#[test]
fn cooperative_replacement_wins_after_held_refresh() {
    run("replace");
}

#[test]
fn external_removal_during_fetch_is_not_resurrected() {
    run("external-remove");
}

#[test]
fn external_replacement_during_fetch_is_not_overwritten() {
    run("external-replace");
}

#[test]
fn manager_refresh_and_team_rebind_preserve_touched_and_unrelated_metadata() {
    run("metadata");
}

#[test]
#[ignore = "requires explicit PA_TS_PACKAGE_DIR and pinned PA_TEST_NODE; no ambient lookup"]
fn typescript_async_lock_is_not_stolen_after_ten_seconds() {
    run("ts-lock");
}

#[test]
#[ignore = "requires explicit PA_TS_PACKAGE_DIR and pinned PA_TEST_NODE; no ambient lookup"]
fn typescript_first_refresh_interoperates_with_rust_process() {
    run("ts-first");
}

#[test]
#[ignore = "requires explicit PA_TS_PACKAGE_DIR and pinned PA_TEST_NODE; no ambient lookup"]
fn rust_first_refresh_interoperates_with_typescript_process() {
    run("rust-first");
}

fn refresh_worker(root: &Path, actor: &str) {
    // Warm both managers before either refresh starts, including the real backend cache.
    let mut auth = manager(root, actor);
    mark(root, &format!("ready-{actor}"));
    wait_for(&root.join(format!("go-{actor}")));
    mark(root, &format!("attempting-{actor}"));
    let result = auth.get_api_key(PROVIDER);
    let outcome = match result.as_deref() {
        Some(FRESH_ACCESS) => "fresh",
        None => "none",
        Some(_) => "other",
    };
    fs::write(root.join(format!("result-{actor}")), outcome).unwrap();
    mark(root, &format!("done-{actor}"));
}

fn writer_worker(root: &Path, action: &str) {
    let mut auth = manager(root, "writer");
    mark(root, "ready-writer");
    for phase in ["first", "retry"] {
        wait_for(&root.join(format!("write-{phase}")));
        match action {
            "remove" => auth.remove(PROVIDER),
            "replace" => auth.set(PROVIDER, replacement()),
            _ => panic!("unknown writer action"),
        }
        fs::write(
            root.join(format!("errors-{phase}")),
            auth.drain_errors().len().to_string(),
        )
        .unwrap();
        mark(root, &format!("wrote-{phase}"));
    }
}

fn spawn_ts(root: &Path, mode: &str, actor: &str) -> Child {
    let node =
        PathBuf::from(std::env::var_os("PA_TEST_NODE").expect("explicit pinned Node required"));
    let package =
        PathBuf::from(std::env::var_os("PA_TS_PACKAGE_DIR").expect("explicit TS package required"));
    assert!(
        node.is_absolute() && package.is_absolute(),
        "TS inputs must be absolute"
    );
    let script = root.join(format!("typescript-{actor}.mjs"));
    fs::write(&script, include_str!("auth_refresh_compat/typescript.mjs")).unwrap();
    isolated_command(&node, root)
        .arg(script)
        .env("PA_AUTH_COMPAT_MODE", mode)
        .env("PA_AUTH_COMPAT_ACTOR", actor)
        .spawn()
        .unwrap()
}

fn race(root: &Path, mode: &str) {
    let initial = seed(root);
    let mut first = if mode == "ts-first" {
        spawn_ts(root, "refresh", "first")
    } else {
        spawn_rust(root, "refresh", "first")
    };
    let mut second = if mode == "rust-first" {
        spawn_ts(root, "refresh", "second")
    } else {
        spawn_rust(root, "refresh", "second")
    };
    wait_for(&root.join("ready-first"));
    wait_for(&root.join("ready-second"));
    mark(root, "go-first");
    wait_for(&root.join("entered-first"));
    mark(root, "go-second");
    wait_for(&root.join("attempting-second"));
    // A bounded negative observation, not a readiness sleep: the second process must
    // remain blocked while the first owns the refresh. The unfixed implementation
    // reaches duplicate-second immediately and fails the final one-attempt check.
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end
        && !root.join("done-second").exists()
        && !root.join("duplicate-second").exists()
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    mark(root, "release");
    finish(&mut first);
    finish(&mut second);
    assert!(
        !root.join("duplicate-second").exists(),
        "single-use refresh spent twice"
    );
    assert!(
        !root.join("entered-second").exists(),
        "second process called refresh instead of reusing result"
    );
    assert!(root.join("spent").exists(), "refresh ledger was not spent");
    for actor in ["first", "second"] {
        assert!(
            fs::read_to_string(root.join(format!("result-{actor}"))).unwrap() == "fresh",
            "both processes must receive fresh credentials"
        );
    }
    let final_doc = document(root);
    assert!(
        final_doc["unrelated-provider"] == initial["unrelated-provider"],
        "unrelated metadata changed"
    );
    assert!(
        final_doc[PROVIDER]["access"] == FRESH_ACCESS,
        "fresh credential not persisted"
    );
}

fn cooperative_write(root: &Path, action: &str) {
    let initial = seed(root);
    let mut refresher = spawn_rust(root, "refresh", "first");
    let mut writer = spawn_rust(root, &format!("writer-{action}"), "writer");
    wait_for(&root.join("ready-first"));
    wait_for(&root.join("ready-writer"));
    mark(root, "go-first");
    wait_for(&root.join("entered-first"));
    mark(root, "write-first");
    wait_for(&root.join("wrote-first"));
    let refused = fs::read_to_string(root.join("errors-first")).unwrap() != "0";
    let unchanged = document(root) == initial;
    // Always release before assertions, so a failing regression leaves no held child.
    mark(root, "release");
    finish(&mut refresher);
    mark(root, "write-retry");
    wait_for(&root.join("wrote-retry"));
    finish(&mut writer);
    assert!(
        refused && unchanged,
        "cooperative writer must refuse while refresh owns document lock"
    );
    assert!(
        fs::read_to_string(root.join("errors-retry")).unwrap() == "0",
        "retry must succeed after refresh"
    );
    let final_doc = document(root);
    assert!(
        final_doc["unrelated-provider"] == initial["unrelated-provider"],
        "unrelated metadata changed"
    );
    match action {
        "remove" => assert!(
            final_doc.get(PROVIDER).is_none(),
            "deleted credential resurrected"
        ),
        "replace" => assert!(
            final_doc[PROVIDER] == serde_json::to_value(replacement()).unwrap(),
            "new credential overwritten"
        ),
        _ => panic!("unknown writer action"),
    }
}

fn external_write(root: &Path, action: &str) {
    let mut expected = seed(root);
    let mut refresher = spawn_rust(root, "refresh", "first");
    wait_for(&root.join("ready-first"));
    mark(root, "go-first");
    wait_for(&root.join("entered-first"));
    match action {
        "external-remove" => {
            expected.as_object_mut().unwrap().remove(PROVIDER);
        }
        "external-replace" => {
            expected[PROVIDER] = serde_json::to_value(replacement()).unwrap();
        }
        _ => panic!("unknown external action"),
    }
    // Deliberately out of protocol: a genuine inode replacement, not a cooperative
    // backend writer. The held refresh must compare its pre-fetch identity at commit.
    let next = root.join("external-auth.json");
    fs::write(&next, expected.to_string()).unwrap();
    fs::rename(next, root.join("auth.json")).unwrap();
    mark(root, "release");
    finish(&mut refresher);
    assert!(
        document(root) == expected,
        "held refresh overwrote externally replaced document"
    );
    assert!(
        fs::read_to_string(root.join("result-first")).unwrap() != "fresh",
        "abandoned refresh returned credentials that were never committed"
    );
}

fn metadata(root: &Path) {
    let initial = seed(root);
    mark(root, "unheld");
    let mut auth = manager(root, "metadata");
    assert!(
        auth.get_api_key(PROVIDER).as_deref() == Some(FRESH_ACCESS),
        "fake refresh failed"
    );
    let mut expected_oauth = initial[PROVIDER].clone();
    for (key, value) in serde_json::to_value(fresh()).unwrap().as_object().unwrap() {
        expected_oauth[key].clone_from(value);
    }
    let after_refresh = document(root);
    assert!(
        after_refresh[PROVIDER] == expected_oauth,
        "refresh lost touched unknown OAuth metadata"
    );
    let team = PrimeTeamCredential {
        team_id: "new-team".to_string(),
        name: "New team".to_string(),
        slug: Some("new".to_string()),
        role: None,
        created_at: None,
    };
    let mut expected = after_refresh;
    expected["prime-inference"]["primeTeam"] = serde_json::to_value(&team).unwrap();
    auth.set_prime_inference_team_selection(Some(team), Some("fixture-prime-key"));
    assert!(auth.drain_errors().is_empty(), "team rebind failed");
    assert!(
        document(root) == expected,
        "team rebind lost touched or unrelated metadata"
    );
    let static_credential = auth.get_all().credential("mcp:compat-static").unwrap();
    auth.set("mcp:compat-static", static_credential);
    assert!(
        auth.drain_errors().is_empty(),
        "static MCP token roundtrip failed"
    );
    assert!(
        document(root) == expected,
        "typed static MCP token roundtrip lost bearerFieldId or createdAt"
    );
    let mut reloaded = manager(root, "reload");
    assert!(
        serde_json::to_value(reloaded.get_all()).unwrap() == expected,
        "manager reload lost metadata"
    );
    assert!(
        reloaded.get_api_key(PROVIDER).as_deref() == Some(FRESH_ACCESS),
        "reloaded key changed"
    );
}

#[cfg(unix)]
fn symlink_alias(root: &Path) -> PathBuf {
    let alias = root.join("auth-alias.json");
    std::os::unix::fs::symlink("auth.json", &alias).unwrap();
    alias
}

#[cfg(unix)]
fn symlink_write(root: &Path) {
    let mut expected = seed(root);
    let alias = symlink_alias(root);
    expected["unrelated-provider"]["aliasWrite"] = json!({"kept": true});
    FileAuthStorageBackend::new(&alias)
        .with_lock(&mut |_| Ok(((), Some(expected.to_string()))))
        .unwrap();
    assert!(
        fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink(),
        "backend replaced symlink alias instead of target"
    );
    assert!(
        document(root) == expected,
        "alias write did not update real target"
    );
    let from_alias: Value = serde_json::from_slice(&fs::read(alias).unwrap()).unwrap();
    assert!(
        from_alias == expected,
        "alias no longer reads target content"
    );
}

fn callback_failure(root: &Path, mode: &str) {
    let initial = seed(root);
    let backend = FileAuthStorageBackend::new(root.join("auth.json"));
    match mode {
        "callback-error" => {
            assert!(
                backend
                    .with_lock(&mut |_| anyhow::bail!("intentional callback error"))
                    .is_err(),
                "callback error was swallowed"
            );
        }
        "callback-panic" => {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = backend.with_lock(&mut |_| panic!("intentional callback panic"));
            }));
            assert!(result.is_err(), "callback panic was swallowed");
        }
        _ => panic!("unknown callback failure mode"),
    }
    assert!(
        !root.join("auth.json.lock").exists(),
        "callback failure leaked document lock"
    );
    backend
        .with_lock(&mut |_| Ok(((), Some(initial.to_string()))))
        .unwrap();
    assert!(
        document(root) == initial,
        "callback failure changed document"
    );
}

fn lock_replacement(root: &Path) {
    let initial = seed(root);
    let lock = root.join("auth.json.lock");
    let retired = root.join("retired-owner.lock");
    let backend = FileAuthStorageBackend::new(root.join("auth.json"));
    let result = backend.with_lock(&mut |_| {
        assert!(lock.is_dir(), "real backend did not acquire protocol lock");
        fs::rename(&lock, &retired)?;
        // Keep the replacement empty: a nonempty marker would hide the old
        // unconditional remove_dir-on-drop bug by making removal fail.
        fs::create_dir(&lock)?;
        Ok(((), None))
    });
    assert!(result.is_err(), "backend accepted a compromised lock");
    assert!(lock.is_dir(), "old guard removed a new owner's empty lock");
    assert!(
        fs::read_dir(&lock).unwrap().next().is_none(),
        "replacement lock should remain empty"
    );
    assert!(
        retired.is_dir(),
        "replaced lock identity unexpectedly disappeared"
    );
    assert!(
        document(root) == initial,
        "lock replacement changed document"
    );
}

fn ts_lock(root: &Path, mode: &str) {
    let initial = seed(root);
    let mut ts = spawn_ts(root, "hold-lock", "ts");
    wait_for(&root.join("ts-lock-held"));
    // The TS callback signals its measured 11.5s age: older than Rust's old
    // 10s stale window, but before proper-lockfile's 15s first heartbeat.
    wait_for(&root.join("ts-old-window"));
    let path = match mode {
        "ts-lock" => root.join("auth.json"),
        #[cfg(unix)]
        "ts-alias-lock" => symlink_alias(root),
        _ => panic!("unknown TS lock mode"),
    };
    let backend = FileAuthStorageBackend::new(path);
    let mut entered = false;
    let result = backend.with_lock(&mut |_| {
        entered = true;
        Ok(((), None))
    });
    mark(root, "release");
    finish(&mut ts);
    assert!(
        result.is_err() && !entered,
        "Rust stole a live TS async lock"
    );
    assert!(
        document(root) == initial,
        "content changed during cross-runtime lock probe"
    );
}

#[test]
#[cfg(unix)]
fn symlink_alias_writes_preserve_alias_and_update_real_target() {
    run("symlink");
}

#[test]
fn callback_error_releases_real_backend_lock() {
    run("callback-error");
}

#[test]
fn callback_panic_releases_real_backend_lock() {
    run("callback-panic");
}

#[test]
fn drop_does_not_remove_another_owners_replacement_lock() {
    run("lock-replacement");
}

#[test]
#[cfg(unix)]
#[ignore = "requires explicit PA_TS_PACKAGE_DIR and pinned PA_TEST_NODE; no ambient lookup"]
fn typescript_target_lock_blocks_rust_symlink_alias_writer() {
    run("ts-alias-lock");
}

fn rust_heartbeat(root: &Path) {
    seed(root);
    let mut refresher = spawn_rust(root, "refresh", "first");
    wait_for(&root.join("ready-first"));
    mark(root, "go-first");
    wait_for(&root.join("entered-first"));
    // The TS process starts its elapsed timer only after the real Rust token
    // callback holds the document lock. Its sync backend has stale=10s.
    let mut ts = spawn_ts(root, "probe-rust-heartbeat", "ts");
    wait_for(&root.join("done-ts"));
    let result: Value = serde_json::from_slice(&fs::read(root.join("result-ts")).unwrap()).unwrap();
    // Release before checking outcomes, including the negative control failure.
    mark(root, "release");
    finish(&mut refresher);
    finish(&mut ts);
    assert!(
        result == json!({"refused": true, "entered": false}),
        "TS sync backend stole Rust's live refresh lock after ten seconds"
    );
    assert!(
        fs::read_to_string(root.join("result-first")).unwrap() == "fresh",
        "held Rust refresh did not complete"
    );
}

#[test]
#[ignore = "requires explicit PA_TS_PACKAGE_DIR and pinned PA_TEST_NODE; no ambient lookup"]
fn rust_refresh_heartbeat_prevents_typescript_sync_lock_steal() {
    run("rust-heartbeat");
}

fn sameprocess_writer_timeout(root: &Path) {
    let initial = seed(root);
    let mut refresh_auth = manager(root, "sameprocess");
    let mut writer_auth = manager(root, "sameprocess-writer");
    let refresher = std::thread::spawn(move || {
        refresh_auth.get_api_key(PROVIDER).as_deref() == Some(FRESH_ACCESS)
    });
    wait_for(&root.join("entered-sameprocess"));
    let (send, receive) = std::sync::mpsc::channel();
    let started = Instant::now();
    let writer = std::thread::spawn(move || {
        writer_auth.set(PROVIDER, replacement());
        let refused = !writer_auth.drain_errors().is_empty();
        send.send((refused, started.elapsed())).unwrap();
    });
    let result = receive.recv_timeout(Duration::from_secs(1));
    let unchanged = document(root) == initial;
    // The unbounded process-mutex mutant cannot finish the writer before this
    // release. Release first even on timeout, then collect both sibling threads.
    mark(root, "release");
    let resolved_key = refresher.join().unwrap();
    writer.join().unwrap();
    assert!(resolved_key, "held same-process refresh did not complete");
    assert!(
        unchanged,
        "same-process writer modified held refresh document"
    );
    assert!(
        matches!(result, Ok((true, elapsed)) if elapsed < Duration::from_secs(1)),
        "same-process ordinary writer did not refuse within one second"
    );
}

fn nested_team_metadata(root: &Path) {
    let initial = seed(root);
    let mut auth = manager(root, "nested-team");
    auth.set_prime_inference_api_key(
        "fixture-prime-key",
        PrimeTeamAssignment::PreserveWhenKeyMatches,
    );
    assert!(
        auth.drain_errors().is_empty(),
        "same-key preserve-team write failed"
    );
    assert!(
        document(root) == initial,
        "same-key preserve-team write lost nested opaquePrimeTeam metadata"
    );
    let credential = auth.get_all().credential("prime-inference").unwrap();
    auth.set("prime-inference", credential);
    assert!(
        auth.drain_errors().is_empty(),
        "typed Prime credential roundtrip failed"
    );
    assert!(
        document(root) == initial,
        "typed Prime credential roundtrip lost nested opaquePrimeTeam metadata"
    );
}

#[test]
fn sameprocess_regular_writer_refuses_within_one_second_of_held_refresh() {
    run("sameprocess-timeout");
}

#[test]
fn same_key_preserve_team_keeps_nested_unknown_team_metadata() {
    run("nested-team-metadata");
}

#[test]
fn coexistence_guard_never_spends_refresh_and_observes_fresh_peer_credentials() {
    run("coexistence-guard");
}

fn coexistence_guard(root: &Path) {
    // Only this env-cleared child changes the policy; no test-parent or user env.
    std::env::set_var("PRIME_AGENT_DISABLE_OAUTH_REFRESH", "1");
    let initial = seed(root);
    mark(root, "unheld");
    let mut auth = manager(root, "coexistence");
    auth.set_fallback_resolver(Arc::new(|_| {
        panic!("disabled OAuth must not select fallback")
    }));
    assert!(
        auth.get_api_key(PROVIDER).is_none(),
        "disabled expired OAuth resolved a key"
    );
    assert!(
        !root.join("spent").exists(),
        "disabled policy spent a rotating grant"
    );
    assert!(
        auth.drain_errors()
            .iter()
            .any(|error| error == "OAuth refresh disabled by PRIME_AGENT_DISABLE_OAUTH_REFRESH"),
        "disabled refresh did not explain the operational policy"
    );
    assert!(
        document(root) == initial,
        "denied refresh changed the stored grant"
    );

    auth.set("coexistence-static", replacement());
    assert!(
        auth.get_api_key("coexistence-static").as_deref() == Some("fixture-replacement"),
        "coexistence policy blocked an ordinary static key"
    );
    auth.remove("coexistence-static");
    assert!(
        auth.drain_errors().is_empty(),
        "coexistence policy blocked ordinary writes"
    );

    // This manager still has the expired snapshot. A real native backend peer
    // commits fresh credentials; the refused refresh path must observe them.
    let backend = FileAuthStorageBackend::new(root.join("auth.json"));
    backend
        .with_lock(&mut |current| {
            let mut data: Value = serde_json::from_str(current.as_deref().unwrap())?;
            for (key, value) in serde_json::to_value(fresh())?.as_object().unwrap() {
                data[PROVIDER][key].clone_from(value);
            }
            Ok(((), Some(serde_json::to_string(&data)?)))
        })
        .unwrap();
    assert!(
        auth.get_api_key(PROVIDER).as_deref() == Some(FRESH_ACCESS),
        "disabled refresh did not re-read a fresh peer credential"
    );
    // Fresh cached access remains usable on the next call too.
    assert!(
        auth.get_api_key(PROVIDER).as_deref() == Some(FRESH_ACCESS),
        "disabled refresh blocked fresh cached access"
    );
    assert!(
        !root.join("spent").exists() && !root.join("entered-coexistence").exists(),
        "disabled policy invoked the token refresh integration"
    );
}

#[test]
fn coexistence_guard_builtin_provider_dispatch_has_zero_transport_io() {
    run("coexistence-builtin");
}

struct CountingTransport(Arc<std::sync::atomic::AtomicUsize>);

impl pa_ai::oauth::CodexHttp for CountingTransport {
    fn post_form<'a>(
        &'a self,
        _url: &'a str,
        _body: &'a str,
        _timeout_ms: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<pa_ai::oauth::CodexHttpResponse, String>>
                + Send
                + 'a,
        >,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Err("fixture transport must not be called".into()) })
    }
}

impl pa_ai::oauth::ProviderHttp for CountingTransport {
    fn request(
        &self,
        _request: pa_ai::oauth::ProviderHttpRequest,
        _timeout_ms: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<pa_ai::oauth::ProviderHttpResponse, String>>
                + Send
                + '_,
        >,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Err("fixture transport must not be called".into()) })
    }
}

fn coexistence_builtin() {
    std::env::set_var("PRIME_AGENT_DISABLE_OAUTH_REFRESH", "1");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let builtin = pa_core::auth::ProviderOAuth::with_transports(
        Arc::new(CountingTransport(Arc::clone(&requests))),
        Arc::new(CountingTransport(Arc::clone(&requests))),
    );
    for provider in ["openai-codex", "anthropic", "github-copilot", "xai"] {
        let mut data = AuthStorageData::default();
        data.0.insert(provider.into(), expired());
        assert!(
            builtin.refresh(provider, &data).is_none(),
            "disabled builtin dispatch resolved refreshed credentials"
        );
    }
    assert!(
        requests.load(std::sync::atomic::Ordering::SeqCst) == 0,
        "disabled builtin dispatch contacted a token transport"
    );
}

#[test]
fn auth_refresh_compat_child() {
    let Ok(mode) = std::env::var("PA_AUTH_COMPAT_MODE") else {
        return;
    };
    // No signals or child killing: each isolated process enforces its own hard exit.
    let (finished, watchdog) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if matches!(
            watchdog.recv_timeout(Duration::from_secs(45)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            std::process::exit(124);
        }
    });
    let root = PathBuf::from(std::env::var_os("PA_AUTH_COMPAT_ROOT").unwrap());
    let actor = std::env::var("PA_AUTH_COMPAT_ACTOR").unwrap();
    match mode.as_str() {
        "refresh" => refresh_worker(&root, &actor),
        "writer-remove" => writer_worker(&root, "remove"),
        "writer-replace" => writer_worker(&root, "replace"),
        "race" | "ts-first" | "rust-first" => race(&root, &mode),
        "remove" | "replace" => cooperative_write(&root, &mode),
        "external-remove" | "external-replace" => external_write(&root, &mode),
        "metadata" => metadata(&root),
        "coexistence-guard" => coexistence_guard(&root),
        "coexistence-builtin" => coexistence_builtin(),
        "ts-lock" | "ts-alias-lock" => ts_lock(&root, &mode),
        #[cfg(unix)]
        "symlink" => symlink_write(&root),
        "callback-error" | "callback-panic" => callback_failure(&root, &mode),
        "lock-replacement" => lock_replacement(&root),
        "rust-heartbeat" => rust_heartbeat(&root),
        "sameprocess-timeout" => sameprocess_writer_timeout(&root),
        "nested-team-metadata" => nested_team_metadata(&root),
        _ => panic!("unknown isolated auth mode"),
    }
    finished.send(()).unwrap();
}
