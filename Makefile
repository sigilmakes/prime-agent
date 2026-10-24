# Downstream local tools. Enter `nix develop --no-update-lock-file` first.
# Nothing publishes, installs globally, rewrites sources, or stages files.
.DEFAULT_GOAL := help
# Bound local and CI compilation without creating incremental/debug caches.
export CARGO_BUILD_JOBS ?= 2
export CARGO_INCREMENTAL := 0
export CARGO_PROFILE_DEV_DEBUG := 0
export CARGO_PROFILE_TEST_DEBUG := 0
PACKAGES ?= pa-cli
PACKAGE_FLAGS = $(foreach package,$(PACKAGES),-p $(package))

help:
	@printf '%s\n' \
		'check           required full-workspace formatting, clippy, tests, and hooks' \
		'lint            read-only formatting and clippy checks (PACKAGES=pa-cli)' \
		'hooks-check     offline Python hook regression tests' \
		'package         build the downstream Nix source package locally' \
		'package-check   run the downstream Nix flake checks (may build)' \
		'workspace-check alias of the required full Rust workspace gate' \
		'Optional: deny, windows-cross, windows-msvc-cross, audit-build, perf' \
		'Install hooks manually: git config core.hooksPath .husky'

check: workspace-check

# Explicit check mode only; never run a global formatter or restage its output.
fmt-check:
	cargo fmt $(PACKAGE_FLAGS) --check

lint: fmt-check
	cargo clippy --locked $(PACKAGE_FLAGS) --all-targets -- -D warnings

typecheck:
	cargo check --locked $(PACKAGE_FLAGS) --all-targets

test:
	cargo test --locked $(PACKAGE_FLAGS)

hooks-check:
	python3 scripts/test_downstream_hooks.py
	python3 -B scripts/test_check_workspace.py
	python3 -B scripts/release/test_legacy_runtime_restart_options.py

# These are intentional, manual source builds; no upstream release scripts,
# binary downloads, npm commands, tag creation, or publishing steps.
package:
	nix build --no-update-lock-file .#prime-agent

package-check:
	nix flake check --no-update-lock-file

workspace-check: hooks-check
	cargo fmt --all --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	cargo test --locked --workspace

# Optional gates fail when their tools or target toolchains are unavailable.
deny:
	@command -v cargo-deny >/dev/null 2>&1 || { echo 'cargo-deny unavailable; enter a Nix shell that provides it' >&2; exit 1; }
	cargo deny --all-features --workspace check advisories licenses

windows-cross:
	cargo check --locked --workspace --target x86_64-pc-windows-gnu --all-targets
	cargo clippy --locked --workspace --target x86_64-pc-windows-gnu --all-targets -- -D warnings

windows-msvc-cross:
	@command -v cargo-xwin >/dev/null 2>&1 || { echo 'cargo-xwin unavailable; enter a Nix shell that provides it and its toolchain' >&2; exit 1; }
	cargo xwin check --locked --workspace --target x86_64-pc-windows-msvc --all-targets

audit-build:
	@command -v cargo-auditable >/dev/null 2>&1 || { echo 'cargo-auditable unavailable; enter a Nix shell that provides it' >&2; exit 1; }
	cargo auditable build --release --locked -p pa-cli --bin prime-agent

# Optional local performance evidence; does not publish or fetch artifacts.
perf:
	cargo build --release --locked -p pa-cli --bin prime-agent -p pa-perf --bin pa-perf
	./target/release/pa-perf run --binary target/release/prime-agent \
		--profile small --out target/perf --runs 10
	./target/release/pa-perf report --baseline crates/pa-perf/baseline.json \
		--results target/perf/results/*.json

.PHONY: help check fmt-check lint typecheck test hooks-check package package-check \
	workspace-check deny windows-cross windows-msvc-cross audit-build perf
