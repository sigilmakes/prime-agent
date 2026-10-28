#!/usr/bin/env bash
# Compatibility wrapper for the README asset refresh xtask.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"

exec cargo run --quiet --manifest-path "$REPO/Cargo.toml" -p xtask -- readme-assets "$@"
