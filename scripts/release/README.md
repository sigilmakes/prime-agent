# Downstream source releases

Mnemosyne (Forgejo `origin`) is the source of truth. GitHub is a mirror.
Downstream releases are **manual, source-only** Forgejo Releases with tags in
this form:

```text
fork-YYYY.MM.DD.N
```

`N` distinguishes releases on the same date. The tag identifies one exact,
reviewed commit. Record its full SHA and the package version in the release
notes. The Rust package version remains `0.10.0`; the source-release tag does
not restamp `Cargo.toml`, `Cargo.lock`, or packaged version metadata.

## Release gate

1. Review the candidate through a Forgejo PR against the current `origin/main`.
   Preserve `archive/typescript-2026-10-18` at
   `239dbe344fcaf671af9898906ff952be73995614` as the TypeScript rollback checkpoint.
2. Verify the exact candidate with `make check` in the pinned `nix develop`
   environment and with `nix flake check --no-update-lock-file`. The full Rust
   gate is formatting, workspace clippy with `-D warnings`, and workspace tests.
   The audited Nix test subset is additional evidence, not a replacement.
   Include the parity-diff evidence required by `AGENTS.md` for user-visible
   changes. Keep test state and credentials isolated.
3. Validate the installed Nix wrapper and its Python kernel in isolated state.
   A source build alone does not prove installed runtime readiness. Do not use
   a shared Cargo target as evidence without proving the executed binary's
   source identity.
4. Obtain explicit authorization for the release. Create the fork-date tag at
   the exact verified commit and publish a non-draft, non-prerelease Forgejo
   Release. Release notes must state the SHA, package version, verification,
   known limits, and rollback commit. Push only to `origin`; its mirror handles
   GitHub. Do not move a published release tag.

No binary archives, npm packages, catalog updates, installers, channel manifests,
or R2 assets are published. Forgejo's source archives are sufficient; no custom
release payload is required. CI validates commits but never publishes releases,
tags, packages, or deployment changes.

## Validation environment

`make check` is the full workspace merge gate. The active Forgejo workflow runs
it after the Nix checks, using the repository's pinned development environment.
The workspace job starts with no inherited credentials and uses private home,
agent, session, harness, daemon socket, and kernel paths. The stdlib driver
`scripts/check_workspace.py` reuses one Cargo target and dependency cache;
compilation uses two jobs, no incremental cache, and no dev/test debug
information. It prepares workspace binaries and a real locked Python kernel
before running the tests, including tests that discover the legacy kernel path
under the private home. It does not read or change an installed user's kernel.

The driver accepts `--target-dir` and `--cargo-home` to reuse approved build
caches rather than creating per-lane copies. A reused Cargo home must contain
neither credential files nor caller-owned registry configuration. Never run
another builder against that target during the gate; the driver's lock serializes only other driver
calls. Shared-target evidence still requires executed-binary source provenance.
`--state-root` must name a new private directory. `--kernel-python` may reuse an
explicitly approved test venv only after checking that its local runtime source
identity matches this checkout and all kernel dependency imports succeed. The
driver exposes it through the private home's legacy test-discovery path; it
must never point at a user's production venv. The driver has a total timeout and
retains its private state for diagnosis. Invoke it through the pinned shell:

```bash
nix develop --no-update-lock-file --command python3 -B scripts/check_workspace.py
```

Dependency registries and Python distributions must be reachable for a cold
run. That is dependency preparation, not permission to use live model-provider
or MCP credentials. Provider and MCP integration tests must use local fakes.
A missing prerequisite, failed test, or timeout blocks the release; do not turn
it into a green gate by filtering the workspace suite or silently skipping
kernel preparation. Some individual tests have their own skip conditions;
a workspace pass does not replace installed-kernel or parity evidence. Native
verification is required before claiming this job passes on the fleet runner.

For a minimal installed-kernel preparation probe, run the exact installed
wrapper with `--prime-agent-bootstrap` under an isolated home and
`PRIME_AGENT_KERNEL_VENV`. This uses the packaged runtime and locked Python
kernel dependencies. It can download dependencies; it needs no provider keys.
A real kernel cell must also pass before cutover. The installed-wrapper probe
boots a fresh kernel and imports the bundled `edit` skill without provider keys:

```bash
python3 nix/runtime_kernel.py /nix/store/<package>/bin/prime-agent \
    --state-root /tmp/prime-installed-<unique-id> \
    --skill-root /path/to/approved/ollama-web
```

`--skill-root` is optional. When supplied, the probe also imports that skill.
The state directory must not exist. Neither probe may reuse the user's live
Python venv or daemon.

## Fleet installation is separate

`/config` owns the fleet source pin and installation. Its guarded updater accepts
a published stable `fork-YYYY.MM.DD.N` release, or an explicit reviewed full SHA:

```text
cd /config
scripts/update-prime-agent <40-character-commit-SHA>
```

Review and validate that separate flake/lock change. Follow `/config`'s commit,
push, and deployment rules; publishing a source release does not authorize a
fleet pin update or deployment. Keep the TypeScript checkpoint available for a
reviewed rollback. Nix-owned installations refuse self-update and rollback
through the upstream installer.

## Reference material, not publication authority

`.github/upstream-workflows/`, the portable tools in `scripts/release/`,
`scripts/package_release.py`, `install-rust.sh`, `install.ps1`, and the Homebrew
drafts in `packaging/` preserve upstream implementation and test references.
They are not a usable downstream release or installation procedure. Some retain
upstream URLs, placeholder hashes, and unrelated version/tag conventions.
Do not activate or run their publication/install paths for this fork.

The active downstream workflow is `.forgejo/workflows/ci.yml`.
`nix/check_workflows.py` rejects active GitHub mirror workflows and automatic
publication commands. A future portable distribution needs a separate design,
review, and explicit authorization; it is outside this source-only release.
