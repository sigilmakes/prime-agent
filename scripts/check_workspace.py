#!/usr/bin/env python3
"""Run the full workspace gate with private state and no inherited credentials.

Enter `nix develop --no-update-lock-file` first. Dependency downloads are allowed;
provider/MCP credentials are not. One target/cache is reused, never per-lane copies.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from urllib.parse import unquote, urlparse


# Keep the pinned shell's compiler/linker inputs, not the caller's credentials.
TOOL_ENV = (
    "PATH", "LANG", "LC_ALL", "TZ", "NIX_CC", "NIX_BINTOOLS",
    "NIX_CFLAGS_COMPILE", "NIX_LDFLAGS", "NIX_SSL_CERT_FILE",
    "SSL_CERT_FILE", "SSL_CERT_DIR", "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR",
    "LIBRARY_PATH", "LD_LIBRARY_PATH", "RUST_SRC_PATH", "PA_WORKSPACE_BOOTSTRAP_PYTHON",
    "PA_TS_BINARY", "PA_TS_REFERENCE",
)
KERNEL_IMPORTS = (
    "rlm", "dill", "requests", "httpx", "yaml", "tomli", "dotenv", "pandas",
    "numpy", "scipy", "bs4", "lxml", "pydantic", "tyro", "mcp",
)


def runtime_identity(source):
    source = Path(source)
    files = [source / "pyproject.toml", source / "uv.lock"]
    files.extend((source / "src/rlm").rglob("*.py"))
    machines = source / "src/rlm/machines"
    if machines.is_dir():
        files.extend(path for path in machines.rglob("*")
                     if path.is_file() and path.suffix != ".py")
    digest = hashlib.sha256()
    for path in sorted(files):
        digest.update(path.relative_to(source).as_posix().encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def package_identity(package):
    """Hash imported package bytes independently of editable/wheel installation layout."""
    package = Path(package)
    if not (package / "__init__.py").is_file():
        raise ValueError("imported kernel runtime package is missing __init__.py")
    files = set(package.rglob("*.py"))
    machines = package / "machines"
    if machines.is_dir():
        files.update(path for path in machines.rglob("*") if path.is_file())
    digest = hashlib.sha256()
    for path in sorted(files):
        relative = path.relative_to(package)
        if "__pycache__" in relative.parts or path.suffix == ".pyc":
            continue
        digest.update(relative.as_posix().encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def isolated_environment(repo, state, target, cargo_home, ambient):
    env = {key: ambient[key] for key in TOOL_ENV if key in ambient}
    home = state / "home"
    kernel = home / ".prime/agent/kernel-venv"
    env.update({
        "HOME": str(home), "USER": "workspace-fixture", "LOGNAME": "workspace-fixture",
        "TMPDIR": str(state / "tmp"), "TMP": str(state / "tmp"), "TEMP": str(state / "tmp"),
        "XDG_CONFIG_HOME": str(state / "config"), "XDG_CACHE_HOME": str(state / "cache"),
        "XDG_DATA_HOME": str(state / "data"), "XDG_STATE_HOME": str(state / "state"),
        "XDG_RUNTIME_DIR": str(state / "run"),
        "PRIME_AGENT_CODING_AGENT_DIR": str(state / "agent"),
        "PRIME_AGENT_KERNEL_VENV": str(kernel),
        "PRIME_AGENT_DAEMON_SOCKET": str(state / "run/daemon.sock"),
        "DO_NOT_TRACK": "1", "PRIME_AGENT_TELEMETRY": "0",
        "CARGO_TARGET_DIR": str(target), "CARGO_HOME": str(cargo_home),
        "CARGO_BUILD_JOBS": "2", "CARGO_INCREMENTAL": "0",
        "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
        "RUST_TEST_THREADS": "2", "PYTHONDONTWRITEBYTECODE": "1",
        "UV_CACHE_DIR": str(cargo_home / "uv-cache"),
        "UV_PYTHON_INSTALL_DIR": str(cargo_home / "uv-python"),
        "UV_PROJECT_ENVIRONMENT": str(kernel),
    })
    return env


def require_ts_reference(env):
    binary = Path(env.get("PA_TS_BINARY", ""))
    if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("PA_TS_BINARY must name an existing absolute executable for required parity")
    reference = Path(env.get("PA_TS_REFERENCE", ""))
    catalog = reference / "packages/coding-agent/src/core/prime-inference-model-catalog.ts"
    if not reference.is_absolute() or not catalog.is_file():
        raise ValueError("PA_TS_REFERENCE must name the TypeScript source checkout for required parity")


def run(command, *, repo, env, deadline, capture=False):
    print("+ " + " ".join(map(str, command)), flush=True)
    child = subprocess.Popen(command, cwd=repo, env=env, start_new_session=True,
                             stdout=subprocess.PIPE if capture else None,
                             text=True)
    try:
        output, _ = child.communicate(timeout=max(0.01, deadline - time.monotonic()))
    except (subprocess.TimeoutExpired, KeyboardInterrupt):
        os.killpg(child.pid, signal.SIGTERM)
        try:
            child.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate()
        raise
    if child.returncode:
        raise subprocess.CalledProcessError(child.returncode, command)
    return output


def prepare_kernel(repo, env, deadline, approved_python=None):
    kernel = Path(env["PRIME_AGENT_KERNEL_VENV"])
    kernel.parent.mkdir(parents=True, exist_ok=True)
    if approved_python is None:
        declared_python = env.get("PA_WORKSPACE_BOOTSTRAP_PYTHON")
        if not declared_python:
            raise ValueError("PA_WORKSPACE_BOOTSTRAP_PYTHON must name the Nix dev shell's Python 3.11 executable")
        bootstrap_python = Path(declared_python)
        if not bootstrap_python.is_absolute() or not bootstrap_python.is_file() or not os.access(bootstrap_python, os.X_OK):
            raise ValueError(f"PA_WORKSPACE_BOOTSTRAP_PYTHON must be an existing absolute executable: {declared_python}")
        run(["uv", "sync", "--locked", "--no-default-groups", "--group", "kernel",
             "--no-config", "--project", str(repo / "prime-agent-runtime"),
             "--python", str(bootstrap_python), "--no-python-downloads"],
            repo=repo, env=env, deadline=deadline)
        python = kernel / "bin/python"
    else:
        python = approved_python.absolute()
    # -I ignores PYTHONPATH and user site packages. A noneditable install can
    # retain matching direct_url metadata while its copied package is stale.
    # Check the bytes actually imported, not just the recorded source directory.
    code = ("import importlib, importlib.metadata, json; from pathlib import Path; "
            f"[importlib.import_module(name) for name in {KERNEL_IMPORTS!r}]; "
            "d=importlib.metadata.distribution('prime-agent-runtime'); "
            "print(json.dumps({'direct_url': json.loads(d.read_text('direct_url.json')), "
            "'package_root': str(Path(importlib.import_module('rlm').__file__).resolve().parent)}))")
    observation = json.loads(run([str(python), "-I", "-c", code], repo=repo, env=env,
                                 deadline=deadline, capture=True))
    url = urlparse(observation["direct_url"]["url"])
    if url.scheme != "file" or url.netloc not in ("", "localhost"):
        raise ValueError("kernel runtime must have local-source installation metadata")
    source = Path(unquote(url.path))
    if runtime_identity(source) != runtime_identity(repo / "prime-agent-runtime"):
        raise ValueError("approved kernel runtime does not match this workspace's source identity")
    if package_identity(observation["package_root"]) != package_identity(repo / "prime-agent-runtime/src/rlm"):
        raise ValueError("actually imported kernel runtime does not match this workspace's package bytes")
    if approved_python is not None:
        # Tests without an override still find the same approved interpreter.
        kernel.symlink_to(python.parent.parent, target_is_directory=True)
    env["PRIME_AGENT_KERNEL_PYTHON"] = str(kernel / "bin/python")
    env["PA_E2E_KERNEL_PYTHON"] = str(kernel / "bin/python")
    print("Verified kernel runtime source and kernel dependency imports", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", type=Path,
                        help="new private directory; retained for logs/diagnosis")
    parser.add_argument("--target-dir", type=Path,
                        help="one existing Cargo target to reuse (default: workspace target)")
    parser.add_argument("--cargo-home", type=Path,
                        help="dedicated credential-free Cargo cache (default: target/workspace-cargo)")
    parser.add_argument("--kernel-python", type=Path,
                        help="approved test venv; source identity and kernel extras are checked")
    parser.add_argument("--require-ts-reference", action="store_true",
                        help="fail unless explicit TS binary and source reference are provisioned")
    parser.add_argument("--timeout", type=float, default=10800,
                        help="total deadline in seconds (default: 10800)")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    repo = Path(__file__).resolve().parents[1]
    target = (args.target_dir or repo / "target").absolute()
    cargo_home = (args.cargo_home or target / "workspace-cargo").absolute()
    if args.state_root:
        state = args.state_root.absolute()
        state.mkdir(mode=0o700)  # Refuse reuse of an old state/credential tree.
    else:
        state = Path(tempfile.mkdtemp(prefix="pa-workspace-", dir="/tmp"))
    print(f"Workspace state: {state}\nCargo target: {target}\nCargo cache: {cargo_home}", flush=True)
    target.mkdir(parents=True, exist_ok=True)
    cargo_home.mkdir(parents=True, exist_ok=True)
    if any((cargo_home / name).exists() for name in ("credentials", "credentials.toml", "config", "config.toml")):
        raise ValueError("Cargo cache must not contain credentials or caller-owned registry configuration")
    with (target / ".downstream-workspace-check.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        env = isolated_environment(repo, state, target, cargo_home, os.environ)
        if args.require_ts_reference:
            require_ts_reference(env)
        for key in ("HOME", "TMPDIR", "XDG_RUNTIME_DIR", "PRIME_AGENT_CODING_AGENT_DIR"):
            Path(env[key]).mkdir(parents=True, exist_ok=True, mode=0o700)
        deadline = time.monotonic() + args.timeout
        prepare_kernel(repo, env, deadline, args.kernel_python)
        # Fetch the complete lockfile, including test-only dependencies, before
        # the offline all-target Clippy and test commands run in a cold CI cache.
        run(["cargo", "fetch", "--locked"], repo=repo, env=env, deadline=deadline)
        run(["cargo", "build", "--locked", "--workspace", "--bins"],
            repo=repo, env=env, deadline=deadline)
        test_env = dict(env, CARGO_NET_OFFLINE="true", UV_OFFLINE="1")
        run(["make", "workspace-check"], repo=repo, env=test_env, deadline=deadline)
    print("Full workspace gate passed", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Workspace gate failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
