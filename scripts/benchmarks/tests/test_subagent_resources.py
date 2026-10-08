from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from subagent_resources import (
    Protocol,
    guard_resources,
    isolated_environment,
    parse_counts,
    read_process,
    stop_group,
    summarize_window,
)


class ResourceBenchmarkTests(unittest.TestCase):
    def test_issue9_resource_guard_rejects_actual_tree_over_small_rss_limit(self):
        with self.assertRaisesRegex(RuntimeError, "RSS safety limit"):
            guard_resources(os.getpid(), 1)

    def test_issue9_watchdog_samples_during_protocol_and_exit_waits(self):
        for operation in ("receive", "wait_for_exit"):
            with self.subTest(operation=operation):
                code = 'import sys; print(\'{"event":"ready"}\',flush=True); sys.stdin.read()'
                process = subprocess.Popen(
                    [sys.executable, "-c", code],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    start_new_session=True,
                )
                protocol = Protocol(process, time.monotonic() + 5, monitor_interval=0.01)
                calls = 0

                def monitor():
                    nonlocal calls
                    calls += 1
                    if calls == 3:
                        raise RuntimeError("watchdog sentinel")

                try:
                    self.assertEqual(protocol.receive(), {"event": "ready"})
                    protocol.monitor = monitor
                    with self.assertRaisesRegex(RuntimeError, "watchdog sentinel"):
                        getattr(protocol, operation)()
                    self.assertEqual(calls, 3)
                finally:
                    stop_group(process)
                    protocol.selector.close()
                    process.stdin.close()
                    process.stdout.close()

    def test_issue9_counts_have_no_fixed_child_or_trial_ceiling(self):
        self.assertEqual(parse_counts("1,2,4,65,1000,9007199254740991"), [1, 2, 4, 65, 1000, 2**53 - 1])
        self.assertEqual(parse_counts(",".join(["1"] * 17)), [1] * 17)
        for value in ("", "0", "-1", "1.5", "1,", "9007199254740992", "NaN", "Infinity"):
            with self.subTest(value=value):
                with self.assertRaises(argparse.ArgumentTypeError):
                    parse_counts(value)

    def test_issue9_environment_drops_live_credentials_and_daemon_routing(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            with patch.dict(
                os.environ,
                {
                    "ANTHROPIC_API_KEY": "must-not-pass",
                    "PRIME_AGENT_DAEMON_SOCKET": "/live/socket",
                    "PRIME_AGENT_KERNEL_PYTHON": "/live/python",
                    "NODE_OPTIONS": "--require /live/hook.js",
                },
            ):
                env = isolated_environment(home, Path("/repo"))
            self.assertNotIn("ANTHROPIC_API_KEY", env)
            self.assertNotIn("PRIME_AGENT_DAEMON_SOCKET", env)
            self.assertNotIn("PRIME_AGENT_KERNEL_PYTHON", env)
            self.assertNotIn("NODE_OPTIONS", env)
            for name in (
                "HOME",
                "TMPDIR",
                "XDG_CONFIG_HOME",
                "XDG_RUNTIME_DIR",
                "PRIME_AGENT_CODING_AGENT_DIR",
                "PRIME_AGENT_SESSION_DIR",
            ):
                self.assertTrue(Path(env[name]).is_relative_to(home))
                self.assertTrue(Path(env[name]).is_dir())

    def test_issue9_proc_stat_handles_spaces_parentheses_and_missing_pss(self):
        with tempfile.TemporaryDirectory() as directory:
            proc = Path(directory)
            process = proc / "123"
            process.mkdir()
            fields = ["S", "42"] + ["0"] * 22
            fields[11], fields[12], fields[19], fields[21] = "12", "8", "567", "10"
            (process / "stat").write_text("123 (worker (name)) " + " ".join(fields))
            (process / "comm").write_text("worker (name)\n")
            actual = read_process(123, proc)
            self.assertEqual(actual["ppid"], 42)
            self.assertEqual(actual["cpu_ticks"], 20)
            self.assertEqual(actual["start_ticks"], 567)
            self.assertEqual(actual["rss_bytes"], 10 * os.sysconf("SC_PAGE_SIZE"))
            self.assertIsNone(actual["pss_bytes"])
            (process / "smaps_rollup").write_text("Rss: 40 kB\nPss: 21 kB\n")
            self.assertEqual(read_process(123, proc)["pss_bytes"], 21 * 1024)

    def test_issue9_cpu_identity_and_pss_unavailability(self):
        def process(start, cpu, rss, pss):
            return {"pid": 123, "start_ticks": start, "cpu_ticks": cpu, "rss_bytes": rss, "pss_bytes": pss}

        samples = [
            {"elapsed_seconds": 0, "processes": [process(1, 10, 100, 80)]},
            {"elapsed_seconds": 1, "processes": [process(1, 30, 200, 180)]},
            {"elapsed_seconds": 2, "processes": [process(2, 900, 300, None)]},
        ]
        result = summarize_window(samples)
        self.assertEqual(result["observed_cpu_seconds"], 20 / os.sysconf("SC_CLK_TCK"))
        self.assertEqual(result["peak_sampled_rss_bytes"], 300)
        self.assertIsNone(result["peak_sampled_pss_bytes"])
        self.assertFalse(result["pss_complete"])

    def test_issue9_protocol_observes_event_then_cleans_process_group(self):
        code = "import json,sys; print(json.dumps({'event':'ready'}),flush=True); sys.stdin.read()"
        process = subprocess.Popen(
            [sys.executable, "-c", code],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            start_new_session=True,
        )
        protocol = Protocol(process, time.monotonic() + 5)
        try:
            self.assertEqual(protocol.receive(), {"event": "ready"})
        finally:
            stop_group(process)
            protocol.selector.close()
            process.stdin.close()
            process.stdout.close()
        self.assertIsNotNone(process.returncode)
        with self.assertRaises(ProcessLookupError):
            os.killpg(process.pid, 0)

    def test_issue9_protocol_eof_and_deadline_fail_not_measure(self):
        for code, budget, error in [
            ("pass", 5, RuntimeError),
            ("import sys; sys.stdin.read()", 0, TimeoutError),
        ]:
            with self.subTest(code=code):
                process = subprocess.Popen(
                    [sys.executable, "-c", code],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    start_new_session=True,
                )
                protocol = Protocol(process, time.monotonic() + budget)
                try:
                    with self.assertRaises(error):
                        protocol.receive()
                finally:
                    stop_group(process)
                    protocol.selector.close()
                    process.stdin.close()
                    process.stdout.close()


if __name__ == "__main__":
    unittest.main()
