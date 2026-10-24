#!/usr/bin/env python3
"""Offline behavior tests for workspace state/credential and kernel provenance gates."""
import json
import shutil
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import check_workspace as gate


class WorkspaceGateTests(unittest.TestCase):
    def runtime(self, root, text="runtime"):
        (root / "src/rlm/machines/demo").mkdir(parents=True)
        (root / "pyproject.toml").write_text("manifest")
        (root / "uv.lock").write_text("lock")
        (root / "src/rlm/__init__.py").write_text(text)
        (root / "src/rlm/machines/demo/MACHINE.md").write_text("machine")

    def observation(self, source, package=None):
        return json.dumps({"direct_url": {"url": source.as_uri()},
                           "package_root": str(package or source / "src/rlm")})

    def test_environment_scrubs_credentials_and_all_ambient_state_overrides(self):
        root = Path("/private")
        env = gate.isolated_environment(root / "repo", root / "state", root / "target",
                                        root / "cargo", {
            "PATH": "/pinned/bin", "NIX_CC": "/nix/store/compiler",
            "PA_WORKSPACE_BOOTSTRAP_PYTHON": "/nix/store/python311/bin/python3.11",
            "UV_PYTHON": "/production/python", "UV_PYTHON_DOWNLOADS": "automatic",
            "HOME": "/production", "OPENAI_API_KEY": "secret",
            "PRIME_AGENT_KERNEL_PYTHON": "/production/python",
            "PRIME_AGENT_CODING_AGENT_DIR": "/production/agent",
            "RLM_SESSION_DIR": "/production/sessions", "PYTHONPATH": "/production/modules",
            "CARGO_ENCODED_RUSTFLAGS": "unreviewed", "HTTP_PROXY": "secret-proxy",
        })
        self.assertFalse(any("secret" in value or "/production" in value for value in env.values()))
        self.assertEqual(env["PATH"], "/pinned/bin")
        self.assertEqual(env["NIX_CC"], "/nix/store/compiler")
        self.assertEqual(env["PA_WORKSPACE_BOOTSTRAP_PYTHON"], "/nix/store/python311/bin/python3.11")
        self.assertNotIn("UV_PYTHON", env)
        self.assertNotIn("UV_PYTHON_DOWNLOADS", env)
        self.assertNotIn("PRIME_AGENT_KERNEL_PYTHON", env)
        self.assertNotIn("PYTHONPATH", env)
        self.assertNotIn("PI_OFFLINE", env)
        self.assertNotIn("PI_SKIP_VERSION_CHECK", env)
        for variable in ("PI_PACKAGE_DIR", "PRIME_AGENT_SESSION_DIR",
                         "PRIME_AGENT_CODING_AGENT_SESSION_DIR", "RLM_SESSION_DIR",
                         "RLM_HARNESS_STATE_DIR", "RLM_GLOBAL_HARNESS_STATE_DIR"):
            self.assertNotIn(variable, env)
        self.assertEqual(env["PRIME_AGENT_CODING_AGENT_DIR"], "/private/state/agent")
        self.assertEqual(env["PRIME_AGENT_DAEMON_SOCKET"], "/private/state/run/daemon.sock")

    def test_network_wrapper_preserves_uid_and_requires_a_new_network_namespace(self):
        command = gate.loopback_command(["cargo", "test", "--workspace"])
        self.assertEqual(command[:7], ["unshare", "--user", "--map-current-user", "--net",
                                       "--keep-caps", "--", gate.sys.executable])
        self.assertEqual(command[-4:], ["--loopback-exec", "cargo", "test", "--workspace"])

    def test_loopback_exec_rejects_host_network_before_any_interface_mutation(self):
        with patch.object(gate.socket, "if_nameindex", return_value=[(1, "lo"), (2, "eth0")]), \
                patch.object(gate.subprocess, "run") as setup:
            with self.assertRaisesRegex(ValueError, "fresh loopback-only"):
                gate.exec_loopback(["cargo", "test"])
        setup.assert_not_called()

    def test_runtime_identity_tracks_lock_sources_and_machine_data_not_cache(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.runtime(root)
            before = gate.runtime_identity(root)
            (root / "src/rlm/cache.pyc").write_bytes(b"cache")
            self.assertEqual(gate.runtime_identity(root), before)
            for name in ("uv.lock", "src/rlm/__init__.py", "src/rlm/machines/demo/MACHINE.md"):
                path = root / name
                previous = path.read_text()
                path.write_text(previous + "changed")
                self.assertNotEqual(gate.runtime_identity(root), before)
                path.write_text(previous)

    def test_approved_kernel_requires_matching_source_before_exposing_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo, installed, state = root / "repo", root / "installed", root / "state"
            self.runtime(repo / "prime-agent-runtime")
            self.runtime(installed)
            env = gate.isolated_environment(repo, state, root / "target", root / "cargo", {})
            python = root / "approved/bin/python"
            copied = root / "site-packages/rlm"
            shutil.copytree(installed / "src/rlm", copied)
            result = self.observation(installed, copied)
            with patch.object(gate, "run", return_value=result) as command:
                gate.prepare_kernel(repo, env, time.monotonic() + 10, python)
            kernel = Path(env["PRIME_AGENT_KERNEL_VENV"])
            self.assertTrue(kernel.is_symlink())
            self.assertEqual(kernel.readlink(), python.parent.parent)
            self.assertEqual(env["PA_E2E_KERNEL_PYTHON"], str(kernel / "bin/python"))
            self.assertIn("-I", command.call_args.args[0])

    def test_mismatched_or_remote_runtime_cannot_be_used_as_kernel(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo, installed = root / "repo", root / "installed"
            self.runtime(repo / "prime-agent-runtime")
            self.runtime(installed, "different runtime")
            for metadata in ({"url": installed.as_uri()}, {"url": "https://example.invalid/runtime"}):
                env = gate.isolated_environment(repo, root / "state", root / "target", root / "cargo", {})
                with patch.object(gate, "run", return_value=json.dumps({
                        "direct_url": metadata, "package_root": str(installed / "src/rlm")})):
                    with self.assertRaises(ValueError):
                        gate.prepare_kernel(repo, env, time.monotonic() + 10, root / "approved/bin/python")
                self.assertFalse(Path(env["PRIME_AGENT_KERNEL_VENV"]).is_symlink())
                self.assertNotIn("PA_E2E_KERNEL_PYTHON", env)

    def test_stale_copied_package_is_rejected_even_when_direct_url_source_matches(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo = root / "repo"
            source = repo / "prime-agent-runtime"
            self.runtime(source)
            copied = root / "site-packages/rlm"
            shutil.copytree(source / "src/rlm", copied)
            for changed in ("__init__.py", "machines/demo/MACHINE.md"):
                path = copied / changed
                previous = path.read_text()
                path.write_text(previous + "stale installed bytes")
                env = gate.isolated_environment(repo, root / "state", root / "target", root / "cargo", {})
                with patch.object(gate, "run", return_value=self.observation(source, copied)):
                    with self.assertRaisesRegex(ValueError, "actually imported kernel runtime"):
                        gate.prepare_kernel(repo, env, time.monotonic() + 10, root / "approved/bin/python")
                self.assertFalse(Path(env["PRIME_AGENT_KERNEL_VENV"]).is_symlink())
                self.assertNotIn("PRIME_AGENT_KERNEL_PYTHON", env)
                self.assertNotIn("PA_E2E_KERNEL_PYTHON", env)
                path.write_text(previous)

    def test_cold_kernel_uses_locked_kernel_group_before_provenance_probe(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.runtime(root / "prime-agent-runtime")
            bootstrap = root / "nix-python311/bin/python3.11"
            bootstrap.parent.mkdir(parents=True)
            bootstrap.write_text("#!/bin/sh\nexit 1\n")  # Never executed by the mocked runner.
            bootstrap.chmod(0o755)
            env = gate.isolated_environment(root, root / "state", root / "target", root / "cargo", {
                "PA_WORKSPACE_BOOTSTRAP_PYTHON": str(bootstrap),
            })
            with patch.object(gate, "run", side_effect=[None, self.observation(root / "prime-agent-runtime")]) as command:
                gate.prepare_kernel(root, env, time.monotonic() + 10)
            self.assertEqual(command.call_count, 2)
            sync, probe = command.call_args_list
            self.assertEqual(sync.args[0], [
                "uv", "sync", "--locked", "--no-default-groups", "--group", "kernel",
                "--no-config", "--project", str(root / "prime-agent-runtime"),
                "--python", str(bootstrap), "--no-python-downloads",
            ])
            self.assertIs(sync.kwargs["env"], env)
            self.assertEqual(env["PA_WORKSPACE_BOOTSTRAP_PYTHON"], str(bootstrap))
            self.assertEqual(env["UV_PROJECT_ENVIRONMENT"], str(root / "state/home/.prime/agent/kernel-venv"))
            self.assertEqual(env["UV_CACHE_DIR"], str(root / "cargo/uv-cache"))
            self.assertEqual(env["UV_PYTHON_INSTALL_DIR"], str(root / "cargo/uv-python"))
            self.assertEqual(probe.args[0][:2], [str(Path(env["PRIME_AGENT_KERNEL_VENV"]) / "bin/python"), "-I"])

    def test_cold_kernel_rejects_missing_or_nonexecutable_declared_python_before_uv(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            nonexecutable = root / "python3.11"
            nonexecutable.write_text("not executable")
            for declared in (None, "python3.11", str(root / "missing"), str(root), str(nonexecutable)):
                with self.subTest(declared=declared):
                    ambient = {} if declared is None else {"PA_WORKSPACE_BOOTSTRAP_PYTHON": declared}
                    env = gate.isolated_environment(root, root / "state", root / "target", root / "cargo", ambient)
                    with patch.object(gate, "run") as command:
                        with self.assertRaisesRegex(ValueError, "PA_WORKSPACE_BOOTSTRAP_PYTHON"):
                            gate.prepare_kernel(root, env, time.monotonic() + 10)
                    command.assert_not_called()
                    self.assertNotIn("PRIME_AGENT_KERNEL_PYTHON", env)
                    self.assertNotIn("PA_E2E_KERNEL_PYTHON", env)


if __name__ == "__main__":
    unittest.main()
