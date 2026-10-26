#!/usr/bin/env python3
"""Run the full workspace gate with private state and no inherited credentials.

Enter `nix develop --no-update-lock-file` first. Dependency downloads are allowed;
provider/MCP credentials are not. One target/cache is reused, never per-lane copies.
"""
import argparse
import tomllib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import shlex
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


# Every bundled Python package, including skills used by scripted Rust fixtures.
SKILL_APIS = {
    "agent_message": ("send",),
    "agent_observe": ("list_agents", "get_agent", "recent_messages"),
    "attach_image": ("run",),
    "compact": ("status", "run"),
    "edit": ("run",),
    "goal": ("get", "create", "complete"),
    "refine": ("status", "run"),
    "rlm_heartbeat": ("list", "create", "update", "delete"),
    "websearch": ("run",),
}


def bundled_skills(repo):
    skills = []
    for manifest in sorted((repo / "skills").glob("*/pyproject.toml")):
        data = tomllib.loads(manifest.read_text())
        packages = data["tool"]["hatch"]["build"]["targets"]["wheel"]["packages"]
        for package in packages:
            module = Path(package).name
            if module not in SKILL_APIS:
                raise ValueError(f"bundled skill needs an API qualification contract: {module}")
            skills.append((data["project"]["name"], module, manifest.parent, package))
    if {module for _, module, _, _ in skills} != set(SKILL_APIS):
        raise ValueError("bundled Python skill manifests do not match qualification contracts")
    return skills


def skill_identity(source, package):
    digest = hashlib.sha256()
    digest.update((Path(source) / "pyproject.toml").read_bytes())
    digest.update(b"\0")
    digest.update(package_identity(Path(source) / package).encode())
    return digest.hexdigest()


def probe_kernel(python, code, repo, env, deadline):
    # -B suppresses writes, not reads. A fresh cache prefix also prevents
    # timestamp-valid stale bytecode in an approved venv from being loaded.
    private = Path(env["PRIME_AGENT_KERNEL_VENV"]).parent
    with tempfile.TemporaryDirectory(prefix="probe-pycache-", dir=private) as cache:
        return run([str(python), "-I", "-B", "-X", f"pycache_prefix={cache}", "-c", code],
                   repo=repo, env=env, deadline=deadline, capture=True)


def verify_skills(repo, python, env, deadline, skills):
    # Isolated imports suppress user-site/PYTHONPATH and bytecode writes even for
    # approved environments. Read metadata and the module that Python loaded.
    contracts = [(distribution, module, SKILL_APIS[module])
                 for distribution, module, _, _ in skills]
    code = f"""import importlib, importlib.metadata, json
from pathlib import Path
rows = {{}}
for distribution, name, apis in {contracts!r}:
    module = importlib.import_module(name)
    if not all(callable(getattr(module, api, None)) for api in apis):
        raise ValueError("bundled skill API missing: " + name)
    metadata = importlib.metadata.distribution(distribution)
    rows[name] = {{"direct_url": json.loads(metadata.read_text("direct_url.json")),
                  "package_root": str(Path(module.__file__).resolve().parent)}}
print(json.dumps(rows))
"""
    observations = json.loads(probe_kernel(python, code, repo, env, deadline))
    for _, module, expected, package in skills:
        observation = observations[module]
        url = urlparse(observation["direct_url"]["url"])
        if url.scheme != "file" or url.netloc not in ("", "localhost"):
            raise ValueError(f"bundled skill {module} requires local-source installation metadata")
        source = Path(unquote(url.path))
        if skill_identity(source, package) != skill_identity(expected, package):
            raise ValueError(f"bundled skill {module} source identity mismatch")
        if package_identity(observation["package_root"]) != package_identity(expected / package):
            raise ValueError(f"actually imported bundled skill {module} bytes mismatch")


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
    started = time.monotonic()
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
    print(f"Elapsed {time.monotonic() - started:.2f}s (exit {child.returncode}): "
          + " ".join(map(str, command)), flush=True)
    if child.returncode:
        raise subprocess.CalledProcessError(child.returncode, command)
    return output


def prepare_kernel(repo, env, deadline, approved_python=None):
    kernel = Path(env["PRIME_AGENT_KERNEL_VENV"])
    kernel.parent.mkdir(parents=True, exist_ok=True)
    skills = bundled_skills(repo)
    if approved_python is None:
        if kernel.exists() or kernel.is_symlink():
            raise ValueError("cold qualification requires a NEW disposable kernel directory")
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
        # The locked kernel group must provide every skill dependency first.
        # Install only exact checkout skill code; do not resolve dependencies or
        # reinstall the already-provenanced runtime outside its locked sync.
        run(["uv", "pip", "install", "--no-config", "--no-python-downloads",
             "--python", str(python), "--no-deps",
             *[str(source) for _, _, source, _ in skills]],
            repo=repo, env=env, deadline=deadline)
    else:
        python = approved_python.absolute()
    # Dependency consistency check is read-only in both cold and approved paths.
    run(["uv", "pip", "check", "--no-config", "--python", str(python)],
        repo=repo, env=env, deadline=deadline)
    # -I ignores PYTHONPATH and user site packages. A noneditable install can
    # retain matching direct_url metadata while its copied package is stale.
    # Check the bytes actually imported, not just the recorded source directory.
    code = ("import importlib, importlib.metadata, json; from pathlib import Path; "
            f"[importlib.import_module(name) for name in {KERNEL_IMPORTS!r}]; "
            "d=importlib.metadata.distribution('prime-agent-runtime'); "
            "print(json.dumps({'direct_url': json.loads(d.read_text('direct_url.json')), "
            "'package_root': str(Path(importlib.import_module('rlm').__file__).resolve().parent)}))")
    observation = json.loads(probe_kernel(python, code, repo, env, deadline))
    url = urlparse(observation["direct_url"]["url"])
    if url.scheme != "file" or url.netloc not in ("", "localhost"):
        raise ValueError("kernel runtime must have local-source installation metadata")
    source = Path(unquote(url.path))
    if runtime_identity(source) != runtime_identity(repo / "prime-agent-runtime"):
        raise ValueError("approved kernel runtime does not match this workspace's source identity")
    if package_identity(observation["package_root"]) != package_identity(repo / "prime-agent-runtime/src/rlm"):
        raise ValueError("actually imported kernel runtime does not match this workspace's package bytes")
    verify_skills(repo, python, env, deadline, skills)
    if approved_python is not None:
        # Tests without an override still find the same approved interpreter.
        kernel.symlink_to(python.parent.parent, target_is_directory=True)
    env["PRIME_AGENT_KERNEL_PYTHON"] = str(kernel / "bin/python")
    env["PA_E2E_KERNEL_PYTHON"] = str(kernel / "bin/python")
    print("Verified runtime and bundled skill source, imported bytes, and API contracts", flush=True)



VENDOR_MLUA_FEATURES = "lua54,vendored,async,send,serialize"
VENDOR_MLUA_CONTRACTS = (
    "route_migration_forwards_to_latest_parent_and_clears_on_drop",
    "real_wake_debt_before_during_after_ack_is_not_consumed_by_forwarding",
    "nested_waker_guards_restore_both_bindings",
    "never_ready_no_wake_future_does_not_schedule_marker_spin",
    "async_thread_migration_routes_registered_proxy_without_rust_repoll",
    "concurrent_route_registration_and_real_wake_forward_once",
    "actual_rust_poll_ack_preserves_concurrent_notification",
)
VENDOR_MLUA_HOOKS = (
    "zero_result_hook_yields_preserve_concat_arguments_and_frame_locals",
    "async_stream_keeps_ordinary_yield_payloads_and_initial_arguments",
    "async_c_yield_preserves_initial_arguments_and_return_values",
    "counted_hook_async_handoff_timeline",
    "counted_hook_timer_control",
)


def vendor_tree_identity(source):
    if source.is_symlink() or not source.is_dir():
        raise ValueError("vendored mlua source root must be a real directory")
    rows = {}
    for path in sorted(source.rglob("*")):
        if path.is_symlink():
            raise ValueError("vendored mlua source must not contain symlinks")
        if path.is_file():
            rows[path.relative_to(source).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    if not rows or "Cargo.toml" not in rows:
        raise ValueError("vendored mlua source tree is incomplete")
    return rows


def prepare_vendor_mlua(repo, state, env, deadline):
    # Keep the copied package outside the Rust workspace without manifest mutation.
    if state.resolve().is_relative_to(repo.resolve()):
        raise ValueError("vendor qualification state must be outside the workspace")
    source = repo / "vendor/mlua"
    expected = vendor_tree_identity(source)
    lock = repo / "vendor/mlua/Cargo.lock"
    provenance = json.loads((repo / "scripts/vendor-mlua-lock.json").read_text())
    if (provenance.get("schema") != 1 or provenance.get("backend") != "lua54"
            or provenance.get("features") != VENDOR_MLUA_FEATURES.split(",")
            or provenance.get("manifest_sha256") != expected["Cargo.toml"]
            or provenance.get("lock_sha256") != hashlib.sha256(lock.read_bytes()).hexdigest()):
        raise ValueError("vendored mlua committed lock provenance mismatch")
    copied = state / "vendor-mlua"
    shutil.copytree(source, copied)
    if vendor_tree_identity(copied) != expected or vendor_tree_identity(source) != expected:
        raise ValueError("vendored mlua copied source identity mismatch")
    (state / "vendor-mlua-source-identities.json").write_text(json.dumps(expected, sort_keys=True))
    run(["cargo", "fetch", "--locked", "--manifest-path", str(copied / "Cargo.toml")],
        repo=repo, env=env, deadline=deadline)
    return copied, expected


def verify_vendor_mlua_copy(repo, copied, expected):
    if (copied / "Cargo.lock").read_bytes() != (repo / "vendor/mlua/Cargo.lock").read_bytes():
        raise ValueError("vendored mlua copied lock identity mismatch")
    observed = vendor_tree_identity(copied)
    # Build outputs are in the existing shared target, never in copied source.
    if observed != expected or vendor_tree_identity(repo / "vendor/mlua") != expected:
        raise ValueError("vendored mlua source changed after preparation")


def check_vendor_mlua(repo, copied, expected, env, deadline):
    verify_vendor_mlua_copy(repo, copied, expected)
    command = ["cargo", "test", "--locked", "--offline", "--manifest-path",
               str(copied / "Cargo.toml"), "--no-default-features", "--features", VENDOR_MLUA_FEATURES]
    listing = run(command + ["--lib", "--", "--list"], repo=repo, env=env,
                  deadline=deadline, capture=True)
    for name in VENDOR_MLUA_CONTRACTS:
        if "thread::wake_debt_tests::" + name + ": test" not in listing.splitlines():
            raise ValueError("vendored mlua contract test missing: " + name)
    hooks = run(command + ["--test", "hooks", "--", "--list"], repo=repo, env=env,
                deadline=deadline, capture=True)
    for name in VENDOR_MLUA_HOOKS:
        if name + ": test" not in hooks.splitlines():
            raise ValueError("vendored mlua hook regression missing: " + name)
    for name in VENDOR_MLUA_CONTRACTS:
        run(command + ["--lib", "thread::wake_debt_tests::" + name, "--", "--exact"],
            repo=repo, env=env, deadline=deadline)
    # Full Lua54 hook compatibility, not only the newly introduced regressions.
    run(command + ["--test", "hooks"], repo=repo, env=env, deadline=deadline)
    run(command + ["--test", "async"], repo=repo, env=env, deadline=deadline)
    run(command + ["--test", "thread"], repo=repo, env=env, deadline=deadline)
    verify_vendor_mlua_copy(repo, copied, expected)


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
        vendor_copy, vendor_identity = prepare_vendor_mlua(repo, state, env, deadline)
        run(["cargo", "build", "--locked", "--workspace", "--bins"],
            repo=repo, env=env, deadline=deadline)
        test_env = dict(env, CARGO_NET_OFFLINE="true", UV_OFFLINE="1")
        check_vendor_mlua(repo, vendor_copy, vendor_identity, test_env, deadline)
        test_env["WORKSPACE_TEST_COMMAND"] = shlex.join([
            "python3", "-B", "scripts/check_workspace_tests.py", "--state-root",
            str(state / "test-targets"), "--deadline", str(deadline),
        ])
        run(["make", "workspace-check"], repo=repo, env=test_env, deadline=deadline)
    print("Full workspace gate passed", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Workspace gate failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
