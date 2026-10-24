#!/usr/bin/env python3
"""Run the full workspace gate with private state and no inherited credentials.

Enter `nix develop --no-update-lock-file` first. Dependency downloads are allowed;
provider/MCP credentials are not. One target/cache is reused, never per-lane copies.
"""
import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
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
    "LIBRARY_PATH", "LD_LIBRARY_PATH", "RUST_SRC_PATH",
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


def loopback_command(command):
    """Keep real HTTP semantics while blocking all external test egress."""
    return ["unshare", "--user", "--map-current-user", "--net", "--keep-caps", "--",
            sys.executable, str(Path(__file__).resolve()), "--loopback-exec", *command]


def exec_loopback(command):
    if not command or {name for _, name in socket.if_nameindex()} != {"lo"}:
        raise ValueError("workspace gate requires a fresh loopback-only network namespace")
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    # Verify readiness, not elapsed time. No test starts with a down loopback.
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        with socket.create_connection(listener.getsockname(), timeout=2) as client:
            peer, _ = listener.accept()
            with peer:
                peer.sendall(b"ok")
                if client.recv(2) != b"ok":
                    raise ValueError("loopback fixture transport is not ready")
    with socket.socket() as external:
        external.settimeout(2)
        try:
            external.connect(("192.0.2.1", 9))  # RFC 5737 documentation address.
        except OSError as error:
            if error.errno != errno.ENETUNREACH:
                raise ValueError("external egress is not proven unreachable") from error
        else:
            raise ValueError("external egress unexpectedly available")
    print(f"Loopback-only test network verified: {os.readlink('/proc/self/ns/net')}",
          file=sys.stderr, flush=True)
    os.execvpe(command[0], command, os.environ)


def run(command, *, repo, env, deadline, capture=False, loopback_only=False):
    if loopback_only:
        command = loopback_command(command)
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
        run(["uv", "sync", "--locked", "--no-default-groups", "--group", "kernel",
             "--no-config", "--project", str(repo / "prime-agent-runtime"), "--python", "3.11"],
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
        for key in ("HOME", "TMPDIR", "XDG_RUNTIME_DIR", "PRIME_AGENT_CODING_AGENT_DIR"):
            Path(env[key]).mkdir(parents=True, exist_ok=True, mode=0o700)
        deadline = time.monotonic() + args.timeout
        prepare_kernel(repo, env, deadline, args.kernel_python)
        run(["cargo", "build", "--locked", "--workspace", "--bins"],
            repo=repo, env=env, deadline=deadline)
        test_env = dict(env, CARGO_NET_OFFLINE="true", UV_OFFLINE="1")
        run(["make", "workspace-check"], repo=repo, env=test_env, deadline=deadline,
            loopback_only=True)
    print("Full workspace gate passed", flush=True)
    return 0


if __name__ == "__main__":
    try:
        if sys.argv[1:2] == ["--loopback-exec"]:
            exec_loopback(sys.argv[2:])
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Workspace gate failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
