# AGENTS.md

Development rules for the downstream Prime Agent Rust fork at `sigilzero/prime-agent`.
Every contributor (human or agent) must read this before working on this repo.

## Repository

- **Never set git identity for commits to this repo's history.** Agents must not set
  `user.name`/`user.email` (config or `-c`) or `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`/
  `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` for commits that land in this repository.
  Those commits carry the identity of the ambient git config. If such a commit fails for a
  missing identity, report it instead of adding config. Allowed: the `*_DATE` variables
  (the golden corpus pins them for reproducibility), throwaway temp repos created by
  tests, and the release automation's bot identity.
- **Never add `Co-authored-by` trailers** — not in commit messages, not in squash
  suggestions, not as attribution for work merged from branches. A PR's commits carry
  one identity: the ambient git config's. Fabricated or hand-written trailers (including
  noreply forms) misattribute commits to unrelated GitHub accounts.
- Mnemosyne `origin` is the source of truth. Forgejo mirrors branches and tags to GitHub.
  Push only to `origin`; do not publish to `github` or `upstream`.
- Keep this repository, its history, issues, and remotes. `upstream/main` is reference
  material, not an automatic merge target. Preserve a TypeScript rollback checkpoint.
- Use Forgejo issues and PRs. Upstream release scripts and archived workflows are
  reference material, not this fork's publication procedure.
- Routine validated commits and pushes are authorized for this fork. Releases and
  installation changes need explicit authorization for the change being deployed.
- Test with isolated home, agent, session, kernel-venv, harness, temporary, and daemon
  socket paths. Use local fake providers and MCP servers, never live user credentials.
- TypeScript parity reference for downstream behavior is the preserved fork checkpoint
  `archive/typescript-2026-10-18` (`239dbe344fcaf671af9898906ff952be73995614`), not
  upstream `v0.9.8`. Set `PA_TS_REFERENCE` to a read-only checkout of that checkpoint.

## Style and structure

- Workspace crates are prefixed `pa-`. The hard ownership rules: one owned area per crate,
  pa-types is the only shared crate, cycle-free dependency direction, minimal public APIs,
  no god-modules. The dependency direction is pinned in the Crates table below; each crate's
  README.md states its scope, non-goals, and public API surface.
- Prefer private modules with an explicitly exported public crate API. Internals are `pub(crate)`.
- Aim for files under 2,000 lines, tests included. The 2,000-2,100 range is a soft review
  signal, not a merge limit: when adding to an already-large file, consider splitting by
  responsibility rather than growing it further. Move related tests and docs with extracted code.
- Name modules for what they own, not `utils` or `common`. Follow existing structure: in
  `crates/pa-core/src/kernel/manager/`, `mod.rs` connects `execution.rs`, `requests.rs`, and
  `teardown.rs`; put a new kernel-manager concern beside them. In
  `crates/pa-core/src/cron/store/`, `mod.rs` connects `heartbeat.rs`, `jobs.rs`, and
  `session_artifacts.rs`; keep storage work there rather than growing the scheduler.
- Inline format args: always prefer `format!("{x}")` over positional.
- Collapse if statements per clippy::collapsible_if.
- Prefer method references over closures per clippy::redundant_closure_for_method_calls.
- Make `match` statements exhaustive; avoid wildcard arms.
- New traits need doc comments explaining their role and how implementations are expected to behave.
- No opaque positional `bool`/`Option` parameters (`foo(false)` is unreadable). Prefer enums,
  named methods, or newtypes. If you must pass an opaque literal by position, use an exact
  `/*param_name*/` comment matching the callee signature.
- Prefer native RPITIT trait methods with explicit `Send` bounds
  (`fn foo(&self) -> impl Future<Output = T> + Send;`) over `#[async_trait]` or
  `#[allow(async_fn_in_trait)]`. Implementations may use `async fn` when they satisfy the contract.
- No single-use helper methods. Do not create a helper referenced only once.
- Instrument async work at the definition (`#[tracing::instrument(...)]`), not with
  `.instrument(...)` at call sites. Check whether the callee is already instrumented first.

## Change hygiene

- One logical change per PR. Fixes ride with the change that introduced them when possible; a PR
  that mixes a feature with unrelated lint fixes or refactors hides both from review.
- If you change dependencies (`Cargo.toml`), regenerate/commit `Cargo.lock` in the same change.
  A new dependency is an architectural change: state in the PR why the existing crates cannot
  host the functionality, and `make deny` must stay clean for it.
- If a change starts forcing edits across many crate internals, stop and fix the boundary instead.

## Tests

- Prefer whole-object equality comparisons over field-by-field checks.
- Do not add tests for statically defined values.
- Do not add negative tests for logic that was removed.
- Bug fixes need a regression test that fails without the fix.
- Verifiers over self-assessment: tmux user-level tests, differential tests against the TS binary
  on PATH, golden corpora replayed against real captured data. No lane merges without its verifier
  passing, rerun by the reviewer where feasible.
- Tests must wait for observable readiness, not fixed sleeps or retry-to-green loops. If a
  test must be disabled, give a reason at the test. Use isolated ports and temporary paths, and
  restore shared state. Run focused tests when changing their behavior.

## Lint discipline

- Zero-warning posture: the merge gates run fmt + clippy + tests at `-D warnings`, so a `warn`
  lint is a merge blocker. Keep the enabled set deliberate and selective; enable a new lint family
  only with every current site fixed in the same change.
- Explain lint exceptions next to the `#[allow(...)]` or workspace configuration; keep
  exceptions narrow. Advisory ignores in `deny.toml` need a reason and review date.

## Performance and the critical path

First paint and interactive frames must not wait for network calls, disk sync, or
background bookkeeping. Catalog discovery, refresh, and cache publication run away
from the paint path. Explicit refresh can await its job while the UI stays responsive.
Use hanging-provider regression tests to verify this boundary; offline mocks alone
do not establish it.

## Merge gates

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` and the downstream Nix checks must pass before integration is
  published. `make check` mirrors the local checks; `.forgejo/workflows/ci.yml` is the
  downstream CI entry point. Archived upstream workflows are reference material, not
  publication authority. Growth past 2,000 lines is review guidance, not a merge limit.
- **Parity-diff evidence is a merge gate** (the port's definition, not optional polish): every PR
  that touches a user-visible surface must include a "parity-diff evidence" section in its
  description showing the TS-binary comparison for what it changed: (1) rendered output —
  frame-diff vs the TS binary;
  (2) interactive behavior — the same input handled identically (keys, mouse, timing); 
  (3) wire parity — byte-compare the TS daemon's traffic for protocol changes; (4) user-visible
  invariants — every user action produces the same visible reaction as TS (`/compact` shows
  started+completed; `/model` shows the selector; a refinement shows its decoration). A feature
  that "works" but was never diffed against the TS binary does not pass review. If TS shows it,
  Rust shows it identically; if Rust shows something TS does not, that is also a parity bug.
- PRs must state ownership compliance (crate README scope/non-goals/public API, dependency
  direction) and classify the change: internals, new `pub` surface, or a new
  dependency/cross-crate re-export. The latter two are architectural changes — an innocent-looking
  `pub use` is a very simple way to break encapsulation.
- The compiled fallback model catalog `crates/pa-ai/src/models_generated.rs` is
  hand-maintained Rust mirroring the TS `packages/ai/src/models.generated.ts` object
  literal — edit it by hand, exactly like its TS source (there is no generator). The
  parity fixture `crates/pa-models/tests/fixtures/catalog.v1.json` refreshes with
  `scripts/generate-catalog-fixture.py`. Bulk data files are exempt from the size guidance.

## Privacy

This fork removes vendor analytics, installation identity generation, trace-only
credentials, and transcript uploaders from source and shipped artifacts. An opt-out
setting is not equivalent. Preserve local sessions, ledgers, harness data, diagnostics,
and actual model-provider and MCP authentication.

Validate release artifacts as well as lifecycle behavior with old opt-in settings
present. Debug-build behavior alone is not privacy evidence.

## Branding

The product is Prime Agent - we are not a pi fork. Scrub "pi"/"pi-mono"/"pi-ai"/
"Prime Intellect"-style naming from all user-visible surfaces (docs, READMEs,
CLI help text, error messages, splash/onboarding strings, keybinding hints, TUI
labels, and code comments that quote user-facing strings); brand everything
Prime Agent. Audit with a repo-wide grep and classify every hit (user-visible
vs wire-internal vs comment) before scrubbing, and list the preserved wire
identifiers in the PR body so the reviewer can verify none were wrongly scrubbed.

EXPLICIT EXCEPTION: wire-protocol identifiers that must stay byte-compatible with the TS product (e.g. the PI_PACKAGE_DIR env var, settings keys, provider IDs like prime-inference, harness _meta namespaces like ai.primeintellect.prime-agent, lockfile names) stay until/unless the TS side renames them — PARITY BEATS BRANDING ON THE WIRE.



## Surface contract

- Read `GLOSSARY.md` when changing session, plugin, provider, context, compaction, or continual-harness behavior. Keep domain definitions there and implementation decisions in the authorized vault plan.
- Tools exposed to the model: `bash`, `edit`, `python_repl` (internal helpers: `rename`, `stdout`).
- Preserve core RLM execution and messaging: `rlm.spawn/find_models/collect/list_subagents/delete_subagent/create_session/progress_note`, `agent_message.send`, `agent_observe`, `compact`, `goal`, `attach_image`, and the markdown/Python skill contract.
- Willow explicitly authorized replacing the harness interface with plugin-owned `plugin.<name>.*` exports and plugin slash commands. Preserve accumulated data, not obsolete function names. A disabled plugin contributes no callable namespace, commands, instructions, hooks, jobs, or context; its saved data remains intact.
- The continual harness is an optional first-party plugin. Core supplies generic execution, storage, scheduling, and context contribution capabilities; harness policy must not remain unconditionally active behind Lua glue.
- Keep canonical history independent of the model-facing context view. A session selects one context strategy. That strategy may use an explicit ordered compaction chain; server-side compaction, Snapcompact, and standard summarization can be fallback strategies. OptChat-style summary-tree context is a separate context strategy, not a compaction fallback.
- Keep cache-stable mandatory prompt layers followed by a dynamic tail. Plugin instructions, callable inventories, and digest contributions reflect active plugins. `prime-agent prompt` exposes the assembled prompt and its layer breakdown without requiring the continual harness.
- Preserve existing CLI/headless behavior except for the explicitly authorized plugin/harness changes. Negotiate optional daemon command/metadata changes; keep startup usable with older peers.

## Crates

One owned area per crate; `pa-types` is the only shared vocabulary crate.
Dependencies flow from the composition root toward lower crates, never back upward.
The table summarizes each crate's current direct workspace dependencies:

| Crate | Purpose | Direct workspace dependencies |
| --- | --- | --- |
| `pa-types` | Shared wire and domain types | none |
| `pa-agent` | Provider-independent agent loop | none |
| `pa-ai` | Providers, model registry, streaming | `pa-types` |
| `pa-models` | Live model catalog and transport | `pa-ai`, `pa-types` |
| `pa-core` | Session engine, tools, skills, kernel, settings | `pa-types`, `pa-ai`, `pa-models`, `pa-agent` |
| `pa-daemon` | Supervisor, workers, wire protocol | `pa-types`, `pa-core`, `pa-agent`, `pa-ai`, `pa-models` |
| `pa-tui` | Terminal UI, daemon-wire client | `pa-types` |
| `pa-cli` | `prime-agent` binary, composition root | `pa-types`, `pa-core`, `pa-ai`, `pa-agent`, `pa-daemon`, `pa-tui` |
| `pa-perf` | Performance corpora + pty bench harness (dev/CI tooling, never shipped) | `pa-types`, `pa-core`, `pa-daemon` |

Layering rules:

- Dependencies only point from higher crates to lower crates shown above. No cycles or
  reverse edges. The leaf crates are peers; add only the dependencies needed for their work.
  Shared wire/domain vocabulary belongs in `pa-types`, not in a higher crate or a duplicate.
- `pa-tui` renders from wire types and events; it does not link the session engine.
- `pa-cli` is a composition root: it wires crates together and contains no business logic.

## Reliability model

- The daemon is a supervisor: it spawns one worker process per active session instead of hosting sessions in-process. Workers are supervised, restarted with backoff, and sessions persist on disk (append-only JSONL, same layout as `~/.prime/agent/sessions`) so reattach works even if the supervisor restarts.
- No stubs, no `todo!()`, no swallowed errors (`anyhow` bubbling to UI is fine).

## References

- Cursor Bugbot's PR review rules (`.cursor/BUGBOT.md`) mirror the standards in this file - update both whenever either changes.
