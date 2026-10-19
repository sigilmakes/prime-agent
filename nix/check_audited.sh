#!/usr/bin/env bash
set -euo pipefail

# This allowlist is intentionally offline; adding a suite requires auditing its
# subprocess/environment behavior, not inheriting upstream live-provider tests.
export HOME="$TMPDIR/audited-home"
export TMPDIR="$TMPDIR/audited-tmp"
mkdir -p "$HOME" "$TMPDIR"
chmod 700 "$HOME" "$TMPDIR"
export XDG_CONFIG_HOME="$HOME/config"
export XDG_CACHE_HOME="$HOME/cache"
export XDG_DATA_HOME="$HOME/data"
export PRIME_AGENT_CODING_AGENT_DIR="$HOME/agent"
export PRIME_AGENT_SESSION_DIR="$HOME/sessions"
export PRIME_AGENT_KERNEL_VENV="$HOME/kernel"
export RLM_GLOBAL_HARNESS_STATE_DIR="$HOME/global-harness"
export RLM_HARNESS_STATE_DIR="$HOME/local-harness"
export RLM_SESSION_DIR="$HOME/artifacts"
export DO_NOT_TRACK=1 PRIME_AGENT_TELEMETRY=0 PI_SKIP_VERSION_CHECK=1
export CARGO_BUILD_JOBS=2 CARGO_PROFILE_TEST_DEBUG=0
cargo test --offline --locked -p pa-core --lib settings::
cargo test --offline --locked -p pa-core --lib session::
cargo test --offline --locked -p pa-cli --test settings_mcp_save --test nix_managed_update
cargo test --offline --locked -p pa-core --lib update::managed::tests
cargo test --offline --locked -p pa-cli --test release_workflow --test windows_runtime_triage_workflow
cargo test --offline --locked -p pa-core --test release_channel_manifest
