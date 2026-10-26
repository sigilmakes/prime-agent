#!/usr/bin/env python3
"""Run Cargo's default workspace test targets in bounded whole-binary groups.

Called only by check_workspace.py with an already-qualified kernel and private
state. Cargo remains the launcher (crate cwd, loader paths and feature resolution).
No individual tests are split, filtered, retried, or given different timeouts.
"""
import argparse
from collections import deque
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
from threading import current_thread, main_thread
import time
import tomllib

from check_workspace import isolated_environment


# Leave headroom inside the outer runner's five-second TERM-to-KILL grace.
CLEANUP_SECONDS = 2


def interrupted(signum, frame):
    raise KeyboardInterrupt(f"workspace tests interrupted by signal {signum}")


def cleanup(children):
    # Only these Cargo process groups are owned here. Fixture descendants that
    # create new sessions are not contained by this cleanup (nor by tini).
    previous = {}
    if current_thread() is main_thread():
        previous = {sig: signal.signal(sig, signal.SIG_IGN)
                    for sig in (signal.SIGTERM, signal.SIGINT)}
    try:
        for child in children:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        until = time.monotonic() + CLEANUP_SECONDS
        for child in children:
            try:
                child.wait(timeout=max(0, until - time.monotonic()))
            except subprocess.TimeoutExpired:
                pass
        # A group may outlive its leader. Never use leader.poll() as its oracle.
        for child in children:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for child in children:
            child.wait()
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def run(command, *, repo, env, deadline, capture=False):
    print("+ " + " ".join(map(str, command)), flush=True)
    child = subprocess.Popen(command, cwd=repo, env=env, start_new_session=True,
                             stdout=subprocess.PIPE if capture else None, text=True)
    try:
        output, _ = child.communicate(timeout=max(0.01, deadline - time.monotonic()))
    except (subprocess.TimeoutExpired, KeyboardInterrupt):
        cleanup([child])
        raise
    if child.returncode:
        cleanup([child])
        raise subprocess.CalledProcessError(child.returncode, command)
    return output


# These fixtures count ALL rlm.repl processes, not only their owned children.
# Their in-process Mutex cannot exclude kernel producers in another binary.
EXCLUSIVE = {"kernel_dispose_e2e", "replacement_kernel_e2e"}


def target_groups(metadata):
    groups = {}
    units = False
    members = set(metadata["workspace_members"])
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        manifest = tomllib.loads(Path(package["manifest_path"]).read_text())
        for kind in ("lib", "bin", "test", "example"):
            declared = manifest.get(kind, [])
            if isinstance(declared, dict):
                declared = [declared]
            if any(target.get("harness") is False for target in declared):
                raise ValueError("custom test harness needs an explicit scheduling review")
            if any(target.get("test") is False or target.get("required-features")
                   for target in declared):
                raise ValueError("disabled or feature-gated targets need an explicit scheduling review")
        for target in package["targets"]:
            if not target["test"]:
                continue
            kinds = target["kind"]
            if kinds == ["lib"] or kinds == ["bin"]:
                units = True
            elif kinds == ["test"] or kinds == ["example"]:
                selector = "--test" if kinds == ["test"] else "--example"
                # --workspace keeps feature unification identical to the full gate.
                # Duplicate target names across crates run together, exactly once.
                name = target["name"]
                groups[(selector, name)] = (f"{selector[2:]}-{name}", [selector, name])
            else:
                raise ValueError(f"unsupported default test target: {kinds}")
    if units:
        groups[("units", "")] = ("workspace-units", ["--lib", "--bins"])
    if not groups:
        raise ValueError("workspace has no default test targets")
    ordinary, exclusive = [], []
    for name, selectors in sorted(groups.values(), key=lambda group: group[0] != "workspace-units"):
        (exclusive if selectors[-1] in EXCLUSIVE else ordinary).append((name, selectors))
    return ordinary, exclusive


def target_environment(repo, state, ambient):
    state.mkdir(mode=0o700, exist_ok=True)
    env = isolated_environment(repo, state, Path(ambient["CARGO_TARGET_DIR"]),
                               Path(ambient["CARGO_HOME"]), ambient)
    for key in ("HOME", "TMPDIR", "XDG_RUNTIME_DIR", "PRIME_AGENT_CODING_AGENT_DIR"):
        Path(env[key]).mkdir(parents=True, exist_ok=True, mode=0o700)
    # Preserve fallback discovery as well as the explicit overrides. In particular,
    # kernel_teardown.rs uses HOME/.prime/agent/kernel-venv, not PA_E2E_KERNEL_PYTHON.
    kernel = Path(env["PRIME_AGENT_KERNEL_VENV"])
    kernel.parent.mkdir(parents=True, exist_ok=True)
    kernel.symlink_to(Path(ambient["PRIME_AGENT_KERNEL_PYTHON"]).parent.parent,
                      target_is_directory=True)
    env.update({key: ambient[key] for key in (
        "PRIME_AGENT_KERNEL_PYTHON", "PA_E2E_KERNEL_PYTHON",
    )})
    env.update(CARGO_NET_OFFLINE="true", UV_OFFLINE="1")
    return env


def run_groups(groups, *, repo, state, env, deadline, jobs):
    pending = deque(groups)
    active = {}
    private_dirs = []
    # Workers only wait on tracked Cargo children; the main thread owns scheduling,
    # logs and whole-process-group teardown, including on interrupt or failure.
    with ThreadPoolExecutor(max_workers=jobs) as workers:
        try:
            while pending or active:
                if time.monotonic() >= deadline:
                    raise TimeoutError("workspace test deadline expired")
                while pending and len(active) < jobs:
                    name, selectors = pending.popleft()
                    command = ["cargo", "test", "--locked", "--offline", "--workspace", *selectors]
                    # A test that binds a socket under `TMPDIR` needs the whole
                    # `sockaddr_un::sun_path` budget, so the private root stays short and
                    # independent of the caller's state depth. Logs retain the readable name.
                    private = Path(tempfile.mkdtemp(prefix="pa-t-", dir="/tmp"))
                    private_dirs.append(private)
                    private_env = target_environment(repo, private, env)
                    output = (state / f"{name}.log").open("w")
                    print(f"Test target started: {name}", flush=True)
                    try:
                        child = subprocess.Popen(command, cwd=repo, env=private_env,
                                                 start_new_session=True, stdout=output,
                                                 stderr=subprocess.STDOUT)
                    except BaseException:
                        output.close()
                        raise
                    future = workers.submit(child.wait)
                    active[future] = (child, output, name, command, time.monotonic())
                done, _ = wait(active, timeout=max(0, deadline - time.monotonic()),
                               return_when=FIRST_COMPLETED)
                if not done:
                    raise TimeoutError("workspace test deadline expired")
                for future in done:
                    child, output, name, command, started = active.pop(future)
                    output.close()
                    print((state / f"{name}.log").read_text(), end="", flush=True)
                    print(f"Test target elapsed: {name} {time.monotonic() - started:.2f}s "
                          f"(exit {child.returncode})", flush=True)
                    if child.returncode:
                        active[future] = (child, output, name, command, started)
                        raise subprocess.CalledProcessError(child.returncode, command)
        finally:
            cleanup([entry[0] for entry in active.values()])
            for _, output, _, _, _ in active.values():
                output.close()
            for private in private_dirs:
                shutil.rmtree(private, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", type=Path, required=True)
    parser.add_argument("--deadline", type=float, required=True)
    args = parser.parse_args()
    if args.deadline <= time.monotonic():
        parser.error("workspace deadline has expired")
    repo = Path(__file__).resolve().parents[1]
    args.state_root.mkdir(mode=0o700)
    deadline = args.deadline
    env = dict(os.environ)
    # Building once avoids competing compilation and retains the original default
    # cargo test target set (not --all-targets, which would add benches/examples).
    run(["cargo", "test", "--locked", "--offline", "--workspace", "--no-run"],
        repo=repo, env=env, deadline=deadline)
    metadata = json.loads(run(["cargo", "metadata", "--locked", "--offline", "--no-deps",
                               "--format-version", "1"],
                              repo=repo, env=env, deadline=deadline, capture=True))
    ordinary, exclusive = target_groups(metadata)
    print(f"Workspace tests: {len(ordinary)} parallel groups (2 slots), "
          f"{len(exclusive)} globally exclusive groups, then all doctests", flush=True)
    run_groups(ordinary, repo=repo, state=args.state_root, env=env, deadline=deadline, jobs=2)
    run_groups(exclusive, repo=repo, state=args.state_root, env=env, deadline=deadline, jobs=1)
    doc_env = target_environment(repo, args.state_root / "doctests", env)
    run(["cargo", "test", "--locked", "--offline", "--workspace", "--doc"],
        repo=repo, env=doc_env, deadline=deadline)
    print("All workspace test targets and doctests passed", flush=True)
    return 0


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"Workspace tests failed: {error}", flush=True)
        raise SystemExit(1) from error
