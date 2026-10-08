"""Opt-in Linux benchmark of real inline Prime child sessions; no inference or daemon."""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import platform
import selectors
import shutil
import signal
import subprocess
import tempfile
import time
from collections.abc import Callable
from pathlib import Path

PHASES = [
    "parent_baseline",
    "active_provider_wait_never_kernel",
    "completed_retained_idle_never_kernel",
    "children_deleted_not_passivated",
]


def isolated_environment(home: Path, repo: Path) -> dict[str, str]:
    # Never inherit provider credentials, daemon sockets, extensions, or kernel overrides.
    env = {key: os.environ[key] for key in ("PATH", "LANG", "TZ") if key in os.environ}
    env.update(
        HOME=str(home),
        TMPDIR=str(home / "tmp"),
        XDG_CONFIG_HOME=str(home / "config"),
        XDG_CACHE_HOME=str(home / "cache"),
        XDG_DATA_HOME=str(home / "data"),
        XDG_STATE_HOME=str(home / "state"),
        XDG_RUNTIME_DIR=str(home / "run"),
        PRIME_RESOURCE_BENCH_HOME=str(home),
        PRIME_AGENT_CODING_AGENT_DIR=str(home / "agent"),
        PI_CODING_AGENT_DIR=str(home / "agent"),
        PRIME_AGENT_SESSION_DIR=str(home / "sessions"),
        PI_OFFLINE="1",
        PI_SKIP_VERSION_CHECK="1",
        DO_NOT_TRACK="1",
        NODE_DISABLE_COMPILE_CACHE="1",
        TSX_TSCONFIG_PATH=str(repo / "tsconfig.json"),
    )
    for key, value in env.items():
        if key in {"HOME", "TMPDIR"} or key.startswith("XDG_") or key.endswith("_DIR"):
            Path(value).mkdir(parents=True, exist_ok=True, mode=0o700)
    return env


def read_process(pid: int, proc: Path = Path("/proc")) -> dict:
    directory = proc / str(pid)
    raw = (directory / "stat").read_text()
    fields = raw[raw.rindex(")") + 2 :].split()
    pss = None
    try:
        for line in (directory / "smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                pss = int(line.split()[1]) * 1024
    except (PermissionError, FileNotFoundError, ProcessLookupError):
        pass
    return {
        "pid": pid,
        "ppid": int(fields[1]),
        "state": fields[0],
        "start_ticks": int(fields[19]),
        "cpu_ticks": int(fields[11]) + int(fields[12]),
        "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
        "pss_bytes": pss,
        "command": (directory / "comm").read_text().strip(),
    }


def process_tree(pid: int) -> list[dict]:
    processes = {}
    for directory in Path("/proc").iterdir():
        if directory.name.isdecimal():
            try:
                pid_value = int(directory.name)
                raw = (directory / "stat").read_text()
                fields = raw[raw.rindex(")") + 2 :].split()
                processes[pid_value] = {"pid": pid_value, "ppid": int(fields[1])}
            except (FileNotFoundError, ProcessLookupError):
                pass
    selected = {pid}
    while True:
        children = {p["pid"] for p in processes.values() if p["ppid"] in selected}
        if children <= selected:
            break
        selected |= children
    if pid not in processes:
        raise ProcessLookupError("Benchmark runner disappeared while sampling")
    result = []
    for key in sorted(selected):
        try:
            result.append(read_process(key))
        except (FileNotFoundError, ProcessLookupError):
            if key == pid:
                raise ProcessLookupError("Benchmark runner disappeared while sampling") from None
    return result


def summarize_window(samples: list[dict]) -> dict:
    # Track identities, not reused PIDs. Short-lived processes between samples are not observable.
    observed: dict[tuple[int, int], list[int]] = {}
    for sample in samples:
        for process in sample["processes"]:
            key = (process["pid"], process["start_ticks"])
            observed.setdefault(key, []).append(process["cpu_ticks"])
    cpu = sum(max(values) - min(values) for values in observed.values()) / os.sysconf("SC_CLK_TCK")
    elapsed = samples[-1]["elapsed_seconds"] - samples[0]["elapsed_seconds"]
    rss = [sum(p["rss_bytes"] for p in s["processes"]) for s in samples]
    pss_complete = all(p["pss_bytes"] is not None for s in samples for p in s["processes"])
    pss = [sum(p["pss_bytes"] or 0 for p in s["processes"]) for s in samples]
    return {
        "sample_count": len(samples),
        "elapsed_seconds": elapsed,
        "observed_cpu_seconds": cpu,
        "observed_cpu_percent_one_core": 100 * cpu / elapsed,
        "peak_sampled_rss_bytes": max(rss),
        "peak_sampled_pss_bytes": max(pss) if pss_complete else None,
        "pss_complete": pss_complete,
    }


def guard_resources(pid: int, rss_limit: int) -> list[dict]:
    snapshot = process_tree(pid)
    if sum(process["rss_bytes"] for process in snapshot) > rss_limit:
        raise RuntimeError("Sampled process tree exceeded RSS safety limit")
    if any("python" in process["command"].lower() for process in snapshot):
        raise RuntimeError("Never-kernel scenario unexpectedly started Python")
    return snapshot


class Protocol:
    def __init__(
        self,
        process: subprocess.Popen,
        deadline: float,
        monitor: Callable[[], None] | None = None,
        monitor_interval: float = 0.1,
    ):
        if monitor_interval <= 0:
            raise ValueError("Monitor interval must be positive")
        self.process = process
        self.deadline = deadline
        self.monitor = monitor
        self.monitor_interval = monitor_interval
        self.pending = b""
        self.selector = selectors.DefaultSelector()
        self.selector.register(process.stdout, selectors.EVENT_READ)

    def check_budget(self) -> float:
        if self.monitor:
            self.monitor()
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Benchmark trial deadline exceeded")
        return remaining

    def receive(self) -> dict:
        while b"\n" not in self.pending:
            remaining = self.check_budget()
            # A timeout samples resources again; only pipe data establishes readiness.
            if not self.selector.select(min(remaining, self.monitor_interval)):
                continue
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("Runner exited before completing the benchmark protocol")
            self.pending += chunk
            if len(self.pending) > 4_000_000:
                raise RuntimeError("Benchmark protocol exceeded 4 MB")
        line, self.pending = self.pending.split(b"\n", 1)
        return json.loads(line)

    def wait_for_exit(self) -> None:
        while self.process.poll() is None:
            remaining = self.check_budget()
            try:
                self.process.wait(timeout=min(remaining, self.monitor_interval))
            except subprocess.TimeoutExpired:
                continue


def stop_group(process: subprocess.Popen) -> None:
    # Always signal the dedicated group, even if the leader already exited.
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        pass
    finally:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=3)


def run_trial(repo: Path, count: int, window: float, interval: float, budget: float, rss_limit: int) -> dict:
    with tempfile.TemporaryDirectory(prefix="prime-resource-") as directory:
        home = Path(directory)
        env = isolated_environment(home, repo)
        node = shutil.which("node")
        if not node:
            raise RuntimeError("Run through nix develop; node is required")
        command = [
            node,
            "--import",
            str(repo / "node_modules/tsx/dist/loader.mjs"),
            str(repo / "scripts/benchmarks/subagent-resource-runner.ts"),
            str(count),
        ]
        started = time.monotonic()
        deadline = started + budget
        phases = []
        with (home / "stderr.log").open("w+b") as stderr:
            process = subprocess.Popen(
                command,
                cwd=home,
                env=env,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=stderr,
                start_new_session=True,
                bufsize=0,
            )

            def monitor() -> None:
                if process.poll() is not None:
                    return
                try:
                    guard_resources(process.pid, rss_limit)
                except ProcessLookupError:
                    if process.poll() is None:
                        raise

            protocol = Protocol(process, deadline, monitor, interval)
            try:
                for expected in PHASES:
                    event = protocol.receive()
                    if event.get("event") != "phase" or event.get("name") != expected:
                        raise RuntimeError(f"Unexpected phase: {event}")
                    samples = []
                    window_start = time.monotonic()
                    while True:
                        now = time.monotonic()
                        if now >= deadline:
                            raise TimeoutError("Benchmark trial deadline exceeded")
                        snapshot = guard_resources(process.pid, rss_limit)
                        samples.append({"elapsed_seconds": now - window_start, "processes": snapshot})
                        if now - window_start >= window:
                            break
                        # This timer defines sampling cadence, never application readiness.
                        time.sleep(min(interval, max(0, window - (time.monotonic() - window_start))))
                    process.stdin.write((expected + "\n").encode())
                    measurement = protocol.receive()
                    if measurement.get("event") != "window_end" or measurement.get("name") != expected:
                        raise RuntimeError(f"Unexpected window result: {measurement}")
                    phases.append({**event, **measurement, **summarize_window(samples), "samples": samples})
                if protocol.receive() != {"event": "complete"}:
                    raise RuntimeError("Runner did not confirm cleanup")
                protocol.wait_for_exit()
                if process.returncode:
                    raise RuntimeError(f"Runner failed: {process.returncode}")
                return {"children": count, "duration_seconds": time.monotonic() - started, "phases": phases}
            except Exception as error:
                stderr.seek(0)
                detail = stderr.read(16384).decode(errors="replace")
                raise RuntimeError(f"{error}\n{detail}") from error
            finally:
                stop_group(process)
                protocol.selector.close()
                process.stdin.close()
                process.stdout.close()


def parse_counts(value: str) -> list[int]:
    try:
        counts = [int(part) for part in value.split(",")]
    except ValueError as error:
        raise argparse.ArgumentTypeError("Counts must be positive safe integers") from error
    # The Node protocol represents counts as IEEE-754 numbers, not arbitrary Python integers.
    if any(count < 1 or count > 2**53 - 1 for count in counts):
        raise argparse.ArgumentTypeError("Counts must be positive safe integers")
    return counts


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--counts", type=parse_counts, default="1,2,4")
    parser.add_argument("--window", type=float, default=1.0)
    parser.add_argument("--interval", type=float, default=0.1)
    parser.add_argument("--deadline", type=float, default=90)
    parser.add_argument("--rss-limit-mib", type=int, default=2048)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    counts = args.counts
    if not (0.1 <= args.window <= 30 and 0.01 <= args.interval <= args.window / 2):
        parser.error("Window must be 0.1–30 seconds; interval 0.01–window/2")
    if not (5 <= args.deadline <= 600 and 64 <= args.rss_limit_mib <= 8192):
        parser.error("Deadline must be 5–600 seconds; RSS limit 64–8192 MiB")
    if platform.system() != "Linux":
        parser.error("Linux /proc is required")
    repo = Path(__file__).resolve().parents[2]
    revision = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    dirty = bool(
        subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=repo,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    )
    result = {
        "source_revision": revision,
        "source_dirty": dirty,
        "schema": 1,
        "scope": "real inline AgentSession children with faux provider; no daemon or TUI",
        "unmeasured": ["kernel active/idle", "kernel snapshots", "passivation", "daemon worker overhead"],
        "platform": platform.platform(),
        "parameters": {**vars(args), "output": str(args.output)},
        "trials": [],
    }
    # Refuse to overwrite existing results. Preserve partial measurements on failure.
    with args.output.open("x") as output:
        try:
            for count in counts:
                result["trials"].append(
                    run_trial(
                        repo,
                        count,
                        args.window,
                        args.interval,
                        args.deadline,
                        args.rss_limit_mib * 1024 * 1024,
                    )
                )
                print(f"Measured {count} real children", flush=True)
        except BaseException as error:
            result["error"] = str(error)
            raise
        finally:
            json.dump(result, output, indent=2)
            output.write("\n")


def interrupted(_signum: int, _frame: object) -> None:
    raise KeyboardInterrupt("Benchmark interrupted")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    main()
