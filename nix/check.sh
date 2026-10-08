#!/usr/bin/env bash
set -euo pipefail

package=$1
version=$2
verifier=$3
state=$4

export HOME="$state/home"
export TMPDIR="$state/tmp"
export XDG_CONFIG_HOME="$state/config"
export XDG_CACHE_HOME="$state/cache"
export XDG_DATA_HOME="$state/data"
export XDG_STATE_HOME="$state/state"
export XDG_RUNTIME_DIR="$state/runtime"
export PRIME_AGENT_CODING_AGENT_DIR="$state/agent"
export PRIME_AGENT_SESSION_DIR="$state/sessions"
export PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 DO_NOT_TRACK=1
export NODE_DISABLE_COMPILE_CACHE=1 PRIME_AGENT_INSTALL_METHOD=node
mkdir -p "$HOME" "$TMPDIR" "$state/work" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
cd "$state/work"

if ! "$package/bin/prime-agent" --version < /dev/null > "$state/version" 2>&1; then
    cat "$state/version"
    echo "Version command failed" >&2
    exit 1
fi
grep -Fx "$version" "$state/version"
if ! "$package/bin/prime-agent" --help < /dev/null > "$state/help" 2>&1; then
    cat "$state/help"
    exit 1
fi
if find "$state" -type s -print -quit | grep -q .; then
    echo "Information commands created a socket" >&2
    exit 1
fi
if [ -e "$PRIME_AGENT_CODING_AGENT_DIR/auth.json" ] || [ -e "$PRIME_AGENT_SESSION_DIR" ]; then
    echo "Information commands created authentication or session state" >&2
    exit 1
fi
node "$verifier" verify --out "$package/lib/prime-agent/packages/coding-agent/dist"
cd "$package/lib/prime-agent"
node --input-type=module <<'JS'
import { createRequire } from "node:module";
const require = createRequire(process.cwd() + "/package.json");
for (const name of ["koffi", "@mariozechner/clipboard", "@silvia-odwyer/photon-node", "undici"]) {
    require(name);
}
await import("@earendil-works/pi-ai/bedrock-provider");
JS
