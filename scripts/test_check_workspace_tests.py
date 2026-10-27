#!/usr/bin/env python3
"""Offline scheduling, selection and state isolation regressions."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import check_workspace_tests as gate


class WorkspaceTestScheduling(unittest.TestCase):
    def test_default_inventory_keeps_units_integrations_examples_and_exclusives(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "Cargo.toml"
            manifest.write_text('[package]\nname="sample"\nversion="0.1.0"\n')
            targets = [
                {"kind": ["lib"], "name": "sample", "test": True},
                {"kind": ["bin"], "name": "app", "test": True},
                {"kind": ["test"], "name": "ordinary", "test": True},
                {"kind": ["test"], "name": "kernel_dispose_e2e", "test": True},
                {"kind": ["example"], "name": "opted-in", "test": True},
                {"kind": ["example"], "name": "default-example", "test": False},
                {"kind": ["bench"], "name": "bench", "test": False},
            ]
            package = {"id": "sample", "manifest_path": str(manifest), "targets": targets}
            ordinary, exclusive = gate.target_groups({"workspace_members": ["sample"],
                                                       "packages": [package, package]})
            self.assertEqual(ordinary, [("workspace-units", ["--lib", "--bins"]),
                                        ("test-ordinary", ["--test", "ordinary"]),
                                        ("example-opted-in", ["--example", "opted-in"])])
            self.assertEqual(exclusive, [("test-kernel_dispose_e2e", ["--test", "kernel_dispose_e2e"]) ])
            manifest.write_text('[[test]]\nname="custom"\nharness=false\n')
            with self.assertRaisesRegex(ValueError, "custom test harness"):
                gate.target_groups({"workspace_members": ["sample"], "packages": [package]})

    def test_private_home_has_the_qualified_fallback_kernel(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            kernel = root / "qualified/bin/python"
            kernel.parent.mkdir(parents=True)
            kernel.write_text("approved fixture")
            ambient = {"PATH": "/tools", "CARGO_TARGET_DIR": str(root / "target"),
                       "CARGO_HOME": str(root / "cargo"),
                       "PRIME_AGENT_KERNEL_PYTHON": str(kernel),
                       "PA_E2E_KERNEL_PYTHON": str(kernel), "OPENAI_API_KEY": "must-not-leak"}
            env = gate.target_environment(root, root / "isolated", ambient)
            self.assertEqual(Path(env["HOME"]).joinpath(".prime/agent/kernel-venv/bin/python").resolve(),
                             kernel.resolve())
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertEqual(env["RUST_TEST_THREADS"], "4")
            self.assertEqual(env["CARGO_HOME"], ambient["CARGO_HOME"])

    def test_disabled_and_feature_gated_manifests_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "Cargo.toml"
            package = {"id": "sample", "manifest_path": str(manifest), "targets": []}
            for declaration in ('test=false', 'required-features=["optional"]'):
                manifest.write_text('[[bin]]\nname="app"\n' + declaration + '\n')
                with self.assertRaisesRegex(ValueError, "disabled or feature-gated"):
                    gate.target_groups({"workspace_members": ["sample"], "packages": [package]})

    def test_selector_qualified_state_names_do_not_collide(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "Cargo.toml"
            manifest.write_text('[package]\nname="sample"\nversion="0.1.0"\n')
            targets = [{"kind": [kind], "name": "same", "test": True}
                       for kind in ("test", "example")]
            ordinary, _ = gate.target_groups({"workspace_members": ["sample"], "packages": [
                {"id": "sample", "manifest_path": str(manifest), "targets": targets}]})
            self.assertEqual([name for name, _ in ordinary], ["test-same", "example-same"])

    def test_cleanup_uses_one_budget_and_signals_exited_leader_groups(self):
        from unittest.mock import Mock, call
        children = [Mock(pid=101), Mock(pid=102)]
        children[0].wait.side_effect = [subprocess.TimeoutExpired("cargo", 2), 0]
        children[1].wait.side_effect = [subprocess.TimeoutExpired("cargo", 0.5), 0]
        with patch.object(gate.os, "killpg") as killpg, \
                patch.object(gate.time, "monotonic", side_effect=[100, 100, 101.5]):
            gate.cleanup(children)
        self.assertEqual(children[0].wait.call_args_list[0], call(timeout=2))
        self.assertEqual(children[1].wait.call_args_list[0], call(timeout=0.5))
        self.assertEqual(killpg.call_args_list, [
            call(101, gate.signal.SIGTERM), call(102, gate.signal.SIGTERM),
            call(101, gate.signal.SIGKILL), call(102, gate.signal.SIGKILL)])

    def test_expired_deadline_does_not_launch_pending_targets(self):
        with patch.object(gate.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(TimeoutError, "deadline expired"):
                gate.run_groups([("pending", ["--test", "pending"])],
                    repo=Path("/fixture"), state=Path("/private"), env={},
                    deadline=time.monotonic() - 1, jobs=2)
        launch.assert_not_called()

    def test_signal_handler_enters_exception_cleanup_path(self):
        with self.assertRaises(KeyboardInterrupt):
            gate.interrupted(gate.signal.SIGTERM, None)

    def test_sigterm_cleans_a_term_resistant_cargo_group(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo = root / "cargo"
            cargo.write_text(f"#!{sys.executable}\n" +
                "import os, signal, socket\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                "s=socket.socket(socket.AF_UNIX);s.connect(os.environ['PROBE_SOCKET'])\n"
                "s.sendall(b'ready');s.recv(1)\n")
            cargo.chmod(0o700)
            server = socket.socket(socket.AF_UNIX)
            server.bind(str(root / "ready.sock"))
            server.listen(1)
            server.settimeout(5)
            code = (
                "import os, signal, time; from pathlib import Path; "
                "import check_workspace_tests as g; "
                "g.target_environment=lambda *args: dict(os.environ); "
                "signal.signal(signal.SIGTERM, g.interrupted); "
                "g.run_groups([('probe', ['--test', 'probe'])], "
                "repo=Path.cwd(), state=Path(os.environ['PROBE_ROOT']), "
                "env=dict(os.environ), deadline=time.monotonic()+30, jobs=2)"
            )
            env = dict(os.environ, PATH=str(root), PROBE_SOCKET=str(root / "ready.sock"),
                       PROBE_ROOT=str(root))
            driver = subprocess.Popen([sys.executable, "-B", "-c", code],
                                      cwd=Path(gate.__file__).parent, env=env,
                                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            peer = None
            try:
                peer, _ = server.accept()
                peer.settimeout(5)
                self.assertEqual(peer.recv(100), b"ready")
                started = time.monotonic()
                driver.send_signal(gate.signal.SIGTERM)
                driver.communicate(timeout=4)
                self.assertNotEqual(driver.returncode, 0)
                self.assertLess(time.monotonic() - started, 4)
                self.assertEqual(peer.recv(1), b"")
            finally:
                if driver.poll() is None:
                    driver.kill()
                    driver.communicate()
                if peer is not None:
                    peer.close()
                server.close()

    def run_probe(self, fail):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo = root / "cargo"
            cargo.write_text(f"#!{sys.executable}\n" +
                "import os, socket, sys\n"
                "s=socket.socket(socket.AF_UNIX);s.connect(os.environ['PROBE_SOCKET'])\n"
                "s.sendall(sys.argv[-1].encode()+b'\\n')\n"
                "reply=s.recv(1);sys.exit(7 if reply==b'f' else 0)\n")
            cargo.chmod(0o700)
            server = socket.socket(socket.AF_UNIX)
            server.bind(str(root / "ready.sock"))
            server.listen(3)
            server.settimeout(5)
            env = dict(os.environ, PATH=str(root), PROBE_SOCKET=str(root / "ready.sock"))
            peers = []
            try:
                with patch.object(gate, "target_environment", return_value=env) as private, \
                        ThreadPoolExecutor(max_workers=1) as executor:
                    future = executor.submit(gate.run_groups,
                        [(name, ["--test", name]) for name in ("a", "b", "c")],
                        repo=root, state=root, env=env, deadline=time.monotonic()+10, jobs=2)
                    for _ in range(2):
                        peer, _ = server.accept()
                        peers.append(peer)
                        self.assertIn(peer.recv(100).strip(), (b"a", b"b"))
                    # Both targets reached readiness before either was released.
                    peers[0].sendall(b"f" if fail else b"x")
                    if fail:
                        with self.assertRaises(subprocess.CalledProcessError) as error:
                            future.result(timeout=5)
                        self.assertEqual(error.exception.returncode, 7)
                        self.assertFalse((root / "c.log").exists())
                        self.assertEqual(peers[1].recv(1), b"")
                    else:
                        peer, _ = server.accept()
                        peers.append(peer)
                        self.assertEqual(peer.recv(100).strip(), b"c")
                        for peer in peers[1:]:
                            peer.sendall(b"x")
                        future.result(timeout=5)
                    homes = [call.args[1] for call in private.call_args_list]
                    self.assertEqual(len(homes), len(set(homes)))
                    # A target's private root carries `TMPDIR`, so it stays short whatever
                    # the state depth is, and it does not outlive its group.
                    self.assertTrue(all(home.parent == Path("/tmp")
                                        and home.name.startswith("pa-t-")
                                        for home in homes))
                    self.assertTrue(all(not home.exists() for home in homes))
            finally:
                for peer in peers:
                    peer.close()
                server.close()

    def test_only_two_targets_run_and_a_slot_is_reused_after_completion(self):
        self.run_probe(False)

    def test_failure_stops_active_target_and_does_not_start_pending_target(self):
        self.run_probe(True)

    def test_exclusive_phase_and_doctests_follow_parallel_phase(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with patch.object(sys, "argv", ["gate", "--state-root", str(root / "targets"),
                                            "--deadline", str(time.monotonic() + 30)]), \
                    patch.object(gate, "run", return_value=json.dumps({})), \
                    patch.object(gate, "target_groups", return_value=([("ordinary", [])],
                                                                    [("exclusive", [])])), \
                    patch.object(gate, "run_groups") as run_groups, \
                    patch.object(gate, "target_environment", return_value={}), \
                    patch.object(gate, "run") as run:
                run.return_value = json.dumps({})
                self.assertEqual(gate.main(), 0)
                self.assertEqual([call.kwargs["jobs"] for call in run_groups.call_args_list], [2, 1])
                self.assertEqual([call.args[0] for call in run_groups.call_args_list],
                                 [[("ordinary", [])], [("exclusive", [])]])
                self.assertEqual(run.call_args.args[0],
                                 ["cargo", "test", "--locked", "--offline", "--workspace", "--doc"])


if __name__ == "__main__":
    unittest.main()
