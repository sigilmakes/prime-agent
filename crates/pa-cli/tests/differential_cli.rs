// large_futures: stack futures on hot paths by design. too_many_lines: style gate
// only. Casts: 64-bit targets; narrowing sits at bounded OS/protocol boundaries.
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Differential CLI tests: run a corpus of invocations against both the
//! installed TypeScript `prime-agent` binary (ground truth for parity) and
//! the Rust binary, and assert matching exit codes, stdout, and stderr.
//! Corpus policy: only invocations fully determined by the CLI surface
//! itself (help/version, validation errors, routing errors, MCP settings
//! reads/writes); the test is skipped when the TS binary is not installed.
#![cfg(unix)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const CORPUS: &[&[&str]] = &[
    // Help and version.
    &["--version"],
    &["-v"],
    &["--help"],
    &["-h"],
    &["help"],
    &["help", "help"],
    &["help", "agents"],
    &["help", "list"],
    &["help", "sessions"],
    &["help", "attach"],
    &["help", "stop"],
    &["help", "rename"],
    &["help", "send"],
    &["help", "schedule"],
    &["help", "schedule", "list"],
    &["help", "schedule", "add"],
    &["help", "schedule", "cancel"],
    &["help", "status"],
    &["help", "doctor"],
    &["help", "shutdown"],
    &["help", "mcp"],
    &["help", "mcp", "add"],
    &["help", "mcp", "list"],
    &["help", "mcp", "get"],
    &["help", "mcp", "remove"],
    &["help", "package"],
    &["help", "package", "install"],
    &["help", "package", "remove"],
    &["help", "package", "list"],
    &["help", "package", "update"],
    &["help", "update"],
    &["help", "model"],
    &["help", "model", "list"],
    &["help", "session"],
    &["help", "session", "export"],
    &["help", "config"],
    // Typos produce suggestions.
    &["help", "schedul"],
    &["help", "schedule", "bogus"],
    &["help", "help", "help"],
    // Argument parser diagnostics.
    &["-x"],
    &["-x", "--thinking", "bogus"],
    &["--mode"],
    &["--mode", "bogus"],
    &["--thinking", "bogus"],
    &["--thinking", "off", "--thinking", "bogus"],
    &["--provider"],
    &["--model"],
    &["--api-key"],
    &["--cwd"],
    &["--system-prompt"],
    &["--fork"],
    &["--session-dir"],
    &["--models"],
    &["--goal-token-budget", "5"],
    &["--goal", ""],
    &["--goal", "  "],
    &["--autonomous-max-turns", "0"],
    &["--autonomous-max-turns", "abc"],
    &["--autonomous-max-tokens", "-1"],
    &["--export", "foo"],
    &["--export=foo"],
    &["--list-models"],
    &["--list-models", "gpt"],
    &["--list-models=gpt"],
    // Daemon client connect failure against a socket that never exists: the
    // full error text is deterministic.
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "list",
    ],
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "list",
        "--json",
    ],
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "sessions",
    ],
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "sessions",
        "--all",
        "--json",
    ],
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "stop",
        "some-agent",
    ],
    // Flag interdependency validation.
    &["--fork", "x", "--continue"],
    &["--fork", "x", "--resume", "y"],
    &["--fork", "x", "--no-session"],
    &["--resume"],
    &["--resume="],
    &["--mode", "rpc", "@file.txt"],
    &["--mode", "daemon", "@file.txt"],
    &[
        "--cwd",
        "/nonexistent-differential-test-dir",
        "--mode",
        "json",
    ],
    // Removed commands.
    &["daemon"],
    &["daemon", "foo"],
    &["app"],
    &["app", "update"],
    &["install"],
    &["remove"],
    &["uninstall"],
    &["manage"],
    &["manage", "update"],
    // Daemon discovery against an empty sandbox state root: deterministic
    // output for both binaries.
    &["status"],
    &["status", "--json"],
    &["doctor"],
    &["doctor", "--json"],
    &["doctor", "--fix"],
    &["doctor", "--fix", "--json"],
    &["shutdown"],
    &["shutdown", "--json"],
    &["shutdown", "--force", "--json"],
    // Public command routing and validation.
    &["--offline", "status"],
    &["--verbose", "status", "--json"],
    &["--help", "status"],
    &["status", "--bogus"],
    &["status", "--offline"],
    &["doctor", "--bogus"],
    &["shutdown", "--bogus"],
    &["stop"],
    &["stop", "a", "b"],
    &["rename", "a"],
    &["send", "--help"],
    &["schedule"],
    &["schedule", "bogus"],
    &["schedule", "list", "--bogus"],
    &["schedule", "list", "a", "b"],
    &["schedule", "cancel"],
    &["schedule", "list", "--help"],
    &["attach"],
    &["attach", "--resume"],
    &["attach", "a", "b"],
    &["attach", "a", "--resume"],
    &["attach", "a", "-r"],
    &["attach", "a", "--resume=x"],
    &["attach", "a", "--continue"],
    &["attach", "a", "--fork"],
    // model/session command rewrites.
    &["model"],
    &["model", "bogus"],
    &["model", "list", "x", "y"],
    // Model catalog rows (`model list`): these need provider auth visible to
    // both binaries so the catalog, not the no-models guidance, prints;
    // PI_OFFLINE keeps it deterministic (bundled, no network refresh).
    &["model", "list"],
    &["model", "list", "gpt"],
    &["model", "list", "claude"],
    &["model", "list", "z-ai"],
    &["model", "list", "z-ai", "glm"],
    &["model", "list", "zzz-nomatch-xyz"],
    &["model", "list", "--json"],
    &["session"],
    &["session", "bogus"],
    &["session", "export"],
    &["session", "export", "a", "b", "c"],
    &["session", "export", "--x"],
    // config command validation (the interactive view itself is excluded:
    // it needs a terminal).
    &["config", "x"],
    // update command validation.
    &["update", "--self"],
    &["update", "package"],
    &["update", "--extensions"],
    &["update", "--self", "package"],
    &["update", "self", "pkg"],
    &["update", "--bogus"],
    &["update", "--force", "--self"],
    // package command validation.
    &["package"],
    &["package", "bogus"],
    &["package", "uninstall"],
    &["package", "list", "x"],
    &["package", "update", "--self"],
    &["package", "update", "a", "b"],
    &["package", "update", "self"],
    &["package", "update", "prime-agent"],
    &["package", "update", "pi"],
    &["package", "update", "--force"],
    &["package", "install"],
    &["package", "remove"],
    &["package", "install", "--bogus"],
    &["package", "install", "-l"],
    &["package", "install", "-h"],
    &["package", "remove", "-h"],
    &["package", "list", "-h"],
    &["package", "update", "-h"],
    &["package", "update", "--nightly", "--stable"],
    &["package", "update", "--extension", "x", "--self"],
    &["package", "update", "--extension", "x", "y"],
    &["package", "update", "--extensions", "--rollback"],
    &["package", "update", "--stable", "some-source"],
    // MCP management (stateful cases share one sandbox HOME per binary).
    &["mcp"],
    &["mcp", "bogus"],
    &["mcp", "list", "x"],
    &["mcp", "list"],
    &["mcp", "get"],
    &["mcp", "get", "x"],
    &["mcp", "get", "bad name!"],
    &["mcp", "remove", "x"],
    &["mcp", "add"],
    &["mcp", "add", "bad name!"],
    &["mcp", "add", "linear"],
    &["mcp", "add", "x"],
    &["mcp", "add", "x", "--url", "notaurl"],
    &["mcp", "add", "x", "--url", "ftp://foo"],
    &["mcp", "add", "x", "--url", "http://u:p@host"],
    &["mcp", "add", "x", "--url", "http://"],
    &["mcp", "add", "x", "--url"],
    &["mcp", "add", "x", "--bogus", "v"],
    &[
        "mcp",
        "add",
        "x",
        "--url",
        "https://e.com",
        "--url",
        "https://f.com",
    ],
    &[
        "mcp",
        "add",
        "x",
        "--url",
        "https://e.com",
        "--oauth",
        "--bearer-token-env-var",
        "V",
    ],
    &["mcp", "add", "x", "--env", "CHILD"],
    &["mcp", "add", "x", "--env", "CHILD=bad-name"],
    &["mcp", "add", "x", "--env", "=SOURCE"],
    &["mcp", "add", "x", "--cwd", "/tmp", "--url", "https://e.com"],
    &[
        "mcp",
        "add",
        "x",
        "--url",
        "https://e.com",
        "--",
        "run",
        "arg",
    ],
    &["mcp", "add", "y", "--url", "https://example.com/mcp"],
    &["mcp", "list"],
    &["mcp", "get", "y"],
    &["mcp", "add", "y", "--url", "https://example.com/mcp"],
    &[
        "mcp",
        "add",
        "y",
        "--force",
        "--url",
        "https://other.com/mcp",
    ],
    &["mcp", "remove", "y"],
    &["mcp", "list"],
    &["mcp", "remove", "y"],
];

fn ts_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PA_TS_BINARY") {
        return Some(PathBuf::from(path));
    }
    let found = Command::new("which")
        .arg("prime-agent")
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let path = String::from_utf8_lossy(&found.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

struct InvocationOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(binary: &Path, args: &[&str], sandbox: &Path) -> InvocationOutput {
    let output = Command::new("timeout")
        .arg("20")
        .arg(binary)
        .args(args)
        .env("HOME", sandbox.join("home"))
        // Isolate the agent state dir: the TS binary resolves the real home
        // through passwd rather than $HOME, so the env override is the only
        // reliable isolation for both binaries.
        .env("PRIME_AGENT_CODING_AGENT_DIR", sandbox.join("agent"))
        // Isolate TMPDIR too: without it the
        // TS daemon census escapes into the box's ambient daemons and
        // `shutdown --force` kills unrelated sockets (containment contract).
        .env("TMPDIR", sandbox.join("tmp"))
        .env("PI_OFFLINE", "1")
        .current_dir(sandbox.join("cwd"))
        .output()
        .expect("failed to spawn binary under test");
    InvocationOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Normalize version numbers so the TS product version (0.9.x) and the Rust
/// workspace version can be compared structurally.
fn normalize(text: &str, sandbox_roots: &[&Path]) -> String {
    let mut text = text.to_string();
    // Sandbox paths leak into cwd-related error messages; normalize them so
    // the per-binary sandbox roots compare equal.
    for root in sandbox_roots {
        text = text.replace(&root.display().to_string(), "<SANDBOX>");
    }
    // Docs paths in login guidance resolve to each binary's own install dir
    // (package dir); compare the shape, not the installation location.
    text = normalize_docs_paths(&text);
    // The `prompt` dump command is a Rust-first addition the TS product has
    // not adopted yet; normalize its help rows out so the rest of the command
    // surface still compares equal. When the TS product adopts the command,
    // drop this normalizer.
    text = normalize_prompt_command_rows(&text);
    normalize_versions(&text)
}

/// Remove the `prompt` command row from top-level help (the Rust binary
/// lists a command the TS binary does not have yet).
fn normalize_prompt_command_rows(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("prompt")
                && trimmed.contains("Print the assembled system prompt"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace `<any dir>/docs/providers.md|models.md` lines with a placeholder.
fn normalize_docs_paths(text: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            for doc in ["/docs/providers.md", "/docs/models.md"] {
                if trimmed.contains(doc) {
                    return line.replace(trimmed.trim_end(), &format!("<PACKAGE_DOCS>{doc}"));
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_versions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let token = &text[start..i];
            let trailing = token.ends_with('.');
            let version_candidate = token.trim_end_matches('.');
            let parts: Vec<&str> = version_candidate.split('.').collect();
            if parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            {
                let _ = write!(out, "X{}.X.X", if trailing { "." } else { "" });
            } else {
                // Non-version digit runs are kept verbatim: trimming here
                // would corrupt adjacent separators (e.g. git fetch ranges).
                out.push_str(token);
            }
        } else {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn sandbox(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "pa-cli-differential-{prefix}-{}",
        std::process::id()
    ));
    for dir in ["home", "cwd", "agent", "tmp"] {
        std::fs::create_dir_all(base.join(dir)).expect("create sandbox directory");
    }
    base
}

// These are declared native contracts, not stream fallbacks. Help snapshots are
// derived from each command registry and its formatter. Native update/package
// behavior is documented in docs/legacy-upgrade-contract.md. args.rs tests pin
// bounded u32 and unknown-option validation; model list declares only [search],
// not a --json option. ACP shares the native non-interactive @file rejection.
// TypeScript reference: archive/typescript-2026-10-18, commit
// 239dbe344fcaf671af9898906ff952be73995614.
fn declared_output_matches(
    output: &InvocationOutput,
    exit: i32,
    stdout: &str,
    stderr: &str,
    sandbox_roots: &[&Path],
) -> bool {
    let normalize_contract = |text: &str| {
        let mut text = text.to_owned();
        for root in sandbox_roots {
            text = text.replace(&root.display().to_string(), "<SANDBOX>");
        }
        normalize_versions(&normalize_docs_paths(&text))
    };
    output.exit_code == Some(exit)
        && if stdout.is_empty() {
            output.stdout.is_empty()
        } else {
            normalize_contract(&output.stdout) == normalize_contract(stdout)
        }
        && if stderr.is_empty() {
            output.stderr.is_empty()
        } else {
            normalize_contract(&output.stderr) == normalize_contract(stderr)
        }
}

fn declared_native_contract(
    case: &[&str],
    ts: Option<&InvocationOutput>,
    native: &InvocationOutput,
    roots: &[&Path],
) -> Option<bool> {
    match case {
        ["--help" | "-h"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 0, "", include_str!("fixtures/native-cli-contract/ts-help.txt"), roots))
                && declared_output_matches(native, 0, include_str!("fixtures/native-cli-contract/native-help.txt"), "", roots),
        ),
        ["help"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 0, include_str!("fixtures/native-cli-contract/ts-help.txt"), "", roots))
                && declared_output_matches(native, 0, include_str!("fixtures/native-cli-contract/native-help.txt"), "", roots),
        ),
        ["help", "package"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 0, include_str!("fixtures/native-cli-contract/ts-package-help.txt"), "", roots))
                && declared_output_matches(native, 0, include_str!("fixtures/native-cli-contract/native-package-help.txt"), "", roots),
        ),
        ["help", "update"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 0, include_str!("fixtures/native-cli-contract/ts-update-help.txt"), "", roots))
                && declared_output_matches(native, 0, include_str!("fixtures/native-cli-contract/native-update-help.txt"), "", roots),
        ),
        ["--autonomous-max-turns", "0" | "abc"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: --autonomous-max-turns must be a positive integer\n", roots))
                && declared_output_matches(native, 1, "", "Error: --autonomous-max-turns must be a positive integer between 1 and u32::MAX (4294967295)\n", roots),
        ),
        ["--mode", "rpc" | "daemon", "@file.txt"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: @file arguments are not supported in RPC or daemon mode\n", roots))
                && declared_output_matches(native, 1, "", "Error: @file arguments are not supported in RPC, daemon, or ACP mode\n", roots),
        ),
        ["model", "list", "--json"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 0, "", "No models available. Use /login to log into a provider via OAuth or API key. See:\n  <PACKAGE_DOCS>/docs/providers.md\n  <PACKAGE_DOCS>/docs/models.md\n", roots))
                && declared_output_matches(native, 1, "", "Error: Unknown option: --json\n", roots),
        ),
        ["update", "--self"] | ["update", "--force", "--self"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: An update target is no longer needed.\nUse \"prime-agent update [--force]\".\n", roots))
                && declared_output_matches(native, 1, "", "Error: Unknown option for update: --self\nRun \"prime-agent help update\" for usage.\n", roots),
        ),
        ["update", "package"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: Package updates moved to the package command.\nUse \"prime-agent package update [source]\".\n", roots))
                && declared_output_matches(native, 1, "", "Error: Unknown option for update: package\nRun \"prime-agent help update\" for usage.\n", roots),
        ),
        ["update", "--extensions"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: Package updates moved to the package command.\nUse \"prime-agent package update [source]\".\n", roots))
                && declared_output_matches(native, 1, "", "Error: Unknown option for update: --extensions\nRun \"prime-agent help update\" for usage.\n", roots),
        ),
        ["update", "--self", "package"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: Prime Agent and package updates are now separate.\nRun \"prime-agent update [--force]\" and \"prime-agent package update [source]\" separately.\n", roots))
                && declared_output_matches(native, 1, "", "Error: Unknown option for update: --self\nRun \"prime-agent help update\" for usage.\n", roots),
        ),
        ["update", "self", "pkg"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: Prime Agent and package updates are now separate.\nRun \"prime-agent update [--force]\" and \"prime-agent package update [source]\" separately.\n", roots))
                && declared_output_matches(native, 1, "", "Error: Unknown option for update: self\nRun \"prime-agent help update\" for usage.\n", roots),
        ),
        ["package", "update", "--self" | "--force"] | ["package", "update", "--extension", "x", "--self" | "y"] | ["package", "update", "--extensions", "--rollback"] => Some(
            ts.is_none_or(|ts| declared_output_matches(ts, 1, "", "Error: Package updates accept only an optional source. Use \"prime-agent update --force\" to update Prime Agent.\n", roots))
                && declared_output_matches(native, 1, "", "Error: Package updates accept only an optional source. Use \"prime-agent update\" to update Prime Agent.\n", roots),
        ),
        _ => None,
    }
}

// main.js takes over stdout only after early public-command dispatch. These
// exact late rewrite/flag cases use TS stderr and native stdout. Early public
// commands retain the original two-stream comparison below.
fn declared_late_console_case(case: &[&str]) -> bool {
    matches!(
        case,
        ["--version" | "-v"]
            | ["model", "list"]
            | [
                "model",
                "list",
                "gpt" | "claude" | "z-ai" | "zzz-nomatch-xyz"
            ]
    )
}

// These declared divergences are also native contracts when no TS reference is
// installed. None disables only the TS expectation, never the native assertion.
#[test]
fn native_cli_contract_fixtures() {
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let sandbox = sandbox("native-contract");
    for case in [
        &["--help"][..],
        &["-h"],
        &["help"],
        &["help", "package"],
        &["help", "update"],
        &["--autonomous-max-turns", "0"],
        &["--autonomous-max-turns", "abc"],
        &["--mode", "rpc", "@file.txt"],
        &["--mode", "daemon", "@file.txt"],
        &["model", "list", "--json"],
        &["update", "--self"],
        &["update", "--force", "--self"],
        &["update", "package"],
        &["update", "--extensions"],
        &["update", "--self", "package"],
        &["update", "self", "pkg"],
        &["package", "update", "--self"],
        &["package", "update", "--force"],
        &["package", "update", "--extension", "x", "--self"],
        &["package", "update", "--extension", "x", "y"],
        &["package", "update", "--extensions", "--rollback"],
    ] {
        let output = run(&rust, case, &sandbox);
        assert_eq!(
            declared_native_contract(case, None, &output, &[&sandbox]),
            Some(true),
            "native contract {case:?}: exit {:?}, stdout {:?}, stderr {:?}",
            output.exit_code,
            output.stdout,
            output.stderr,
        );
    }
}

#[test]
fn differential_corpus_matches_ts_binary() {
    let Some(ts) = ts_binary() else {
        eprintln!("SKIPPED: TS prime-agent binary not found (set PA_TS_BINARY)");
        return;
    };
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let ts_sandbox = sandbox("ts");
    let rs_sandbox = sandbox("rs");
    let sandbox_roots: Vec<&Path> = vec![&ts_sandbox, &rs_sandbox];

    let mut failures: Vec<String> = Vec::new();
    for case in CORPUS {
        let ts_out = run(&ts, case, &ts_sandbox);
        let rs_out = run(&rust, case, &rs_sandbox);
        let ok = if let Some(matches) =
            declared_native_contract(case, Some(&ts_out), &rs_out, &sandbox_roots)
        {
            matches
        } else if declared_late_console_case(case) {
            ts_out.exit_code == Some(0)
                && rs_out.exit_code == Some(0)
                && ts_out.stdout.is_empty()
                && rs_out.stderr.is_empty()
                && normalize(&ts_out.stderr, &sandbox_roots)
                    == normalize(&rs_out.stdout, &sandbox_roots)
        } else {
            ts_out.exit_code == rs_out.exit_code
                && normalize(&ts_out.stdout, &sandbox_roots)
                    == normalize(&rs_out.stdout, &sandbox_roots)
                && normalize(&ts_out.stderr, &sandbox_roots)
                    == normalize(&rs_out.stderr, &sandbox_roots)
        };
        if !ok {
            failures.push(format!(
                "argv: {:?}\n  ts exit {:?} rs exit {:?}\n  ts stdout: {:?}\n  rs stdout: {:?}\n  ts stderr: {:?}\n  rs stderr: {:?}",
                case,
                ts_out.exit_code,
                rs_out.exit_code,
                ts_out.stdout,
                rs_out.stdout,
                ts_out.stderr,
                rs_out.stderr
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "differential corpus mismatches ({} of {}):\n{}",
        failures.len(),
        CORPUS.len(),
        failures.join("\n---\n")
    );
}

fn run_with_env(
    binary: &Path,
    args: &[&str],
    sandbox: &Path,
    envs: &[(&str, &str)],
) -> InvocationOutput {
    let mut command = Command::new("timeout");
    command
        .arg("20")
        .arg(binary)
        .args(args)
        .env("HOME", sandbox.join("home"))
        .env("TMPDIR", sandbox.join("tmp"))
        .env("PRIME_AGENT_CODING_AGENT_DIR", sandbox.join("agent"))
        .env("PI_OFFLINE", "1")
        .current_dir(sandbox.join("cwd"));
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("failed to spawn binary under test");
    InvocationOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn differential_env_flag_cases_match_ts_binary() {
    let Some(ts) = ts_binary() else {
        eprintln!("SKIPPED: TS prime-agent binary not found (set PA_TS_BINARY)");
        return;
    };
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let ts_sandbox = sandbox("ts-env");
    let rs_sandbox = sandbox("rs-env");
    let sandbox_roots: Vec<&Path> = vec![&ts_sandbox, &rs_sandbox];

    // PI_STARTUP_BENCHMARK is rejected outside interactive mode.
    for case in [&["-p", "hi"][..], &["--print"][..], &["--mode", "json"][..]] {
        let ts_out = run_with_env(&ts, case, &ts_sandbox, &[("PI_STARTUP_BENCHMARK", "1")]);
        let rs_out = run_with_env(&rust, case, &rs_sandbox, &[("PI_STARTUP_BENCHMARK", "1")]);
        assert_eq!(
            ts_out.exit_code, rs_out.exit_code,
            "case {case:?} exit code"
        );
        assert_eq!(
            normalize(&ts_out.stderr, &sandbox_roots),
            normalize(&rs_out.stderr, &sandbox_roots),
            "case {case:?} stderr"
        );
    }
}

fn fixture_session() -> String {
    [
        r#"{"type":"session","id":"d7f6e5c4","version":3,"timestamp":"2026-09-01T10:00:00.000Z","cwd":"/tmp/project"}"#,
        r#"{"type":"message","id":"e1","parentId":null,"timestamp":"2026-09-01T10:00:01.000Z","message":{"role":"user","content":"export a fixture"}}"#,
        r#"{"type":"message","id":"e2","parentId":"e1","timestamp":"2026-09-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"the answer"}],"usage":{"inputTokens":10,"outputTokens":5}}}"#,
        r#"{"type":"message","id":"e3","parentId":"e2","timestamp":"2026-09-01T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"tc1","name":"bash","arguments":{"command":"ls"}}]}}"#,
        r#"{"type":"message","id":"e4","parentId":"e3","timestamp":"2026-09-01T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"tc1","toolName":"bash","content":[{"type":"text","text":"file.txt"}],"isError":false}}"#,
        r#"{"type":"label","id":"e5","parentId":"e4","targetId":"e1","label":"start","timestamp":"2026-09-01T10:00:05.000Z"}"#,
    ]
    .join("\n")
    + "\n"
}

/// The base64-embedded session data of an exported file, decoded and parsed
/// (the wire contract is the decoded JSON, not the base64 bytes).
fn exported_session_data(html: &str) -> serde_json::Value {
    use base64::Engine as _;
    let marker = "session-data\" type=\"application/json\">";
    let start = html.find(marker).expect("session data element");
    let blob = &html[start + marker.len()..];
    let blob = blob.split('<').next().expect("script close");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .expect("base64 session data");
    serde_json::from_slice(&decoded).expect("session data JSON")
}

/// The theme CSS custom properties as a sorted set (the TS exporter emits
/// them in file order, the Rust one sorted - the values are the contract).
fn exported_css_vars(html: &str) -> Vec<String> {
    let mut vars: Vec<String> = html
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("--") && line.ends_with(';'))
        .map(str::to_string)
        .collect();
    vars.sort();
    vars
}

/// Differential parity for `session export`: the same fixture exported by
/// both binaries prints the same success line and produces files carrying
/// the same session data, theme CSS variables, and template scaffolding.
#[test]
fn differential_session_export_matches_ts_binary() {
    let Some(ts) = ts_binary() else {
        eprintln!("SKIPPED: TS prime-agent binary not found (set PA_TS_BINARY)");
        return;
    };
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let ts_sandbox = sandbox("ts-export");
    let rs_sandbox = sandbox("rs-export");
    let sandbox_roots: Vec<&Path> = vec![&ts_sandbox, &rs_sandbox];
    let fixture = "fixture-session.jsonl";
    for sandbox in [&ts_sandbox, &rs_sandbox] {
        std::fs::write(sandbox.join("cwd").join(fixture), fixture_session())
            .expect("write fixture");
    }

    let ts_out = run(
        &ts,
        &["session", "export", fixture, "out.html"],
        &ts_sandbox,
    );
    let rs_out = run(
        &rust,
        &["session", "export", fixture, "out.html"],
        &rs_sandbox,
    );
    assert_eq!(ts_out.exit_code, Some(0), "TS export exit");
    assert_eq!(rs_out.exit_code, Some(0), "native export exit");
    // session export rewrites to --export, after TS main's stdout takeover.
    // Its declared TS payload is stderr; native informational output is stdout.
    assert!(ts_out.stdout.is_empty(), "TS export stdout must be unused");
    assert!(
        rs_out.stderr.is_empty(),
        "native export stderr must be unused"
    );
    assert_eq!(
        normalize(&ts_out.stderr, &sandbox_roots),
        normalize(&rs_out.stdout, &sandbox_roots),
        "declared export payload"
    );
    assert!(
        rs_out.stdout.contains("Exported to: out.html"),
        "success line: {}",
        rs_out.stdout
    );

    let ts_html =
        std::fs::read_to_string(ts_sandbox.join("cwd").join("out.html")).expect("TS export");
    let rs_html =
        std::fs::read_to_string(rs_sandbox.join("cwd").join("out.html")).expect("Rust export");

    // Same session data: header, entries, leaf.
    assert_eq!(
        exported_session_data(&ts_html),
        exported_session_data(&rs_html)
    );
    // Same theme CSS custom properties.
    assert_eq!(exported_css_vars(&ts_html), exported_css_vars(&rs_html));
    // Same template scaffolding (the product export template, verbatim).
    for marker in [
        "<title>Session Export</title>",
        "hamburger",
        "marked",
        "hljs",
    ] {
        assert!(ts_html.contains(marker), "TS export has {marker}");
        assert!(rs_html.contains(marker), "Rust export has {marker}");
    }

    // The missing-file error is the same surface too.
    let ts_missing = run(&ts, &["session", "export", "nope.jsonl"], &ts_sandbox);
    let rs_missing = run(&rust, &["session", "export", "nope.jsonl"], &rs_sandbox);
    assert_eq!(ts_missing.exit_code, rs_missing.exit_code);
    assert_eq!(ts_missing.exit_code, Some(1), "missing export must fail");
    assert!(
        ts_missing.stdout.is_empty(),
        "TS missing export has no stdout"
    );
    assert!(
        rs_missing.stdout.is_empty(),
        "native missing export has no stdout"
    );
    assert_eq!(
        normalize(&ts_missing.stderr, &sandbox_roots),
        normalize(&rs_missing.stderr, &sandbox_roots),
        "missing-file stderr"
    );
}
