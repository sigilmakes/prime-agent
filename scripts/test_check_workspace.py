#!/usr/bin/env python3
"""Offline behavior tests for workspace state/credential and kernel provenance gates."""
import json
import shutil
import shlex
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import check_workspace as gate


class WorkspaceGateTests(unittest.TestCase):
    def setUp(self):
        # Runtime-only unit fixtures deliberately omit the skill tree.
        discovery = patch.object(gate, "bundled_skills", return_value=[])
        verification = patch.object(gate, "verify_skills")
        discovery.start()
        verification.start()
        self.addCleanup(discovery.stop)
        self.addCleanup(verification.stop)

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

    def test_explicit_parity_paths_survive_without_ambient_credentials_or_path_changes(self):
        root = Path("/private")
        env = gate.isolated_environment(root / "repo", root / "state", root / "target",
                                        root / "cargo", {
            "PATH": "/pinned/bin",
            "PA_TS_BINARY": "/nix/store/pinned-ts/bin/prime-agent",
            "PA_TS_REFERENCE": "/workspace/ts-reference",
            "OPENAI_API_KEY": "secret", "NODE_OPTIONS": "--require=/untrusted.js",
            "PRIME_AGENT_MANAGED_BY": "ambient-manager",
        })
        self.assertEqual(env["PA_TS_BINARY"], "/nix/store/pinned-ts/bin/prime-agent")
        self.assertEqual(env["PA_TS_REFERENCE"], "/workspace/ts-reference")
        self.assertEqual(env["PATH"], "/pinned/bin")
        for key in ("OPENAI_API_KEY", "NODE_OPTIONS", "PRIME_AGENT_MANAGED_BY"):
            self.assertNotIn(key, env)

    def test_required_parity_rejects_missing_relative_and_nonexecutable_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "prime-agent"
            binary.write_text("fixture; never executed")
            reference = root / "ts-reference"
            catalog = reference / "packages/coding-agent/src/core/prime-inference-model-catalog.ts"
            catalog.parent.mkdir(parents=True)
            catalog.write_text("fixture")
            valid = {"PA_TS_BINARY": str(binary), "PA_TS_REFERENCE": str(reference)}
            for declared in ({}, valid, dict(valid, PA_TS_BINARY="prime-agent"),
                             dict(valid, PA_TS_BINARY=str(root / "absent"))):
                with self.assertRaisesRegex(ValueError, "PA_TS_BINARY"):
                    gate.require_ts_reference(declared)
            binary.chmod(0o755)
            for declared in (dict(valid, PA_TS_REFERENCE="ts-reference"),
                             dict(valid, PA_TS_REFERENCE=str(root / "absent"))):
                with self.assertRaisesRegex(ValueError, "PA_TS_REFERENCE"):
                    gate.require_ts_reference(declared)
            gate.require_ts_reference(valid)

    def test_required_parity_fails_before_kernel_or_cargo_when_not_provisioned(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = ["check_workspace.py", "--state-root", str(root / "state"),
                    "--target-dir", str(root / "target"), "--require-ts-reference"]
            with patch.object(gate.sys, "argv", args), \
                    patch.dict(gate.os.environ, {}, clear=True), \
                    patch.object(gate, "prepare_kernel") as kernel, \
                    patch.object(gate, "run") as command:
                with self.assertRaisesRegex(ValueError, "PA_TS_BINARY"):
                    gate.main()
            kernel.assert_not_called()
            command.assert_not_called()

    def test_workspace_gate_runs_full_checks_without_nested_namespace(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = ["check_workspace.py", "--state-root", str(root / "state"),
                    "--target-dir", str(root / "target"), "--cargo-home", str(root / "cargo")]
            events = []
            vendor_copy, vendor_identity = root / "state/vendor-mlua", {"source.rs":"reviewed"}
            def record_command(command, **kwargs):
                events.append(command[0] + " " + command[1])
            def record_prepare(*arguments):
                events.append("vendor prepare")
                return vendor_copy, vendor_identity
            def record_check(*arguments):
                events.append("vendor check")
            with patch.object(gate.sys, "argv", args), \
                    patch.object(gate, "prepare_kernel"), \
                    patch.object(gate, "prepare_vendor_mlua", side_effect=record_prepare) as prepare_vendor, \
                    patch.object(gate, "check_vendor_mlua", side_effect=record_check) as check_vendor, \
                    patch.object(gate, "run", side_effect=record_command) as command:
                self.assertEqual(gate.main(), 0)
            self.assertEqual(command.call_count, 3)
            fetch, build, checks = command.call_args_list
            self.assertEqual(fetch.args[0], ["cargo", "fetch", "--locked"])
            self.assertNotIn("CARGO_NET_OFFLINE", fetch.kwargs["env"])
            self.assertEqual(build.args[0], ["cargo", "build", "--locked", "--workspace", "--bins"])
            self.assertEqual(checks.args[0], ["make", "workspace-check"])
            self.assertNotIn("loopback_only", checks.kwargs)
            self.assertEqual(checks.kwargs["env"]["CARGO_NET_OFFLINE"], "true")
            self.assertEqual(checks.kwargs["env"]["UV_OFFLINE"], "1")
            self.assertEqual(checks.kwargs["env"]["HOME"], str(root / "state/home"))
            self.assertEqual(shlex.split(checks.kwargs["env"]["WORKSPACE_TEST_COMMAND"]), [
                "python3", "-B", "scripts/check_workspace_tests.py", "--state-root",
                str(root / "state/test-targets"), "--deadline", str(checks.kwargs["deadline"]),
            ])

            self.assertEqual(events, ["cargo fetch", "vendor prepare", "cargo build", "vendor check", "make workspace-check"])
            prepare_vendor.assert_called_once()
            check_vendor.assert_called_once()
            prepared = prepare_vendor.call_args.args
            qualified = check_vendor.call_args.args
            self.assertEqual(prepared[0], fetch.kwargs["repo"])
            self.assertEqual(prepared[1], root / "state")
            self.assertIs(prepared[2], fetch.kwargs["env"])
            self.assertEqual(prepared[3], fetch.kwargs["deadline"])
            self.assertEqual(qualified[:3], (fetch.kwargs["repo"], vendor_copy, vendor_identity))
            self.assertIs(qualified[3], checks.kwargs["env"])
            self.assertEqual(qualified[4], checks.kwargs["deadline"])
            self.assertEqual(qualified[4], prepared[3])
            self.assertNotIn("CARGO_NET_OFFLINE", prepared[2])
            self.assertEqual(qualified[3]["CARGO_NET_OFFLINE"], "true")
            self.assertEqual(qualified[3]["UV_OFFLINE"], "1")
            self.assertEqual(qualified[3]["CARGO_TARGET_DIR"], prepared[2]["CARGO_TARGET_DIR"])
            self.assertEqual(qualified[3]["CARGO_HOME"], prepared[2]["CARGO_HOME"])

    def test_run_executes_the_requested_command_directly(self):
        with patch.object(gate.subprocess, "Popen") as launch:
            launch.return_value.communicate.return_value = (None, None)
            launch.return_value.returncode = 0
            gate.run(["make", "workspace-check"], repo=Path("/fixture"),
                     env={"HOME": "/private"}, deadline=time.monotonic() + 10)
        self.assertEqual(launch.call_args.args[0], ["make", "workspace-check"])
        self.assertEqual(launch.call_args.kwargs["env"], {"HOME": "/private"})

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
            with patch.object(gate, "run", side_effect=[None, None, None, self.observation(root / "prime-agent-runtime")]) as command:
                gate.prepare_kernel(root, env, time.monotonic() + 10)
            self.assertEqual(command.call_count, 4)
            sync, install, dependency_check, probe = command.call_args_list
            self.assertEqual(install.args[0][:3], ["uv", "pip", "install"])
            self.assertNotIn(str(root / "prime-agent-runtime"), install.args[0])
            self.assertIn("--no-deps", install.args[0])
            self.assertEqual(dependency_check.args[0][:3], ["uv", "pip", "check"])
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


class BundledSkillGateTests(unittest.TestCase):
    def test_probe_uses_fresh_private_bytecode_prefix_and_removes_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            env = {"PRIME_AGENT_KERNEL_VENV": str(root / "kernel")}
            caches = []
            def observe(command, **kwargs):
                self.assertEqual(command[:4], ["/approved/python", "-I", "-B", "-X"])
                cache = Path(command[4].removeprefix("pycache_prefix="))
                self.assertEqual(cache.parent, root)
                self.assertTrue(cache.is_dir())
                self.assertEqual(list(cache.iterdir()), [])
                caches.append(cache)
                return "{}"
            with patch.object(gate, "run", side_effect=observe):
                for _ in range(2):
                    gate.probe_kernel(Path("/approved/python"), "pass", root, env, 1)
            self.assertNotEqual(caches[0], caches[1])
            self.assertTrue(all(not cache.exists() for cache in caches))

    def test_checkout_manifest_set_includes_required_apis(self):
        skills = gate.bundled_skills(Path(gate.__file__).resolve().parents[1])
        self.assertEqual({module for _, module, _, _ in skills}, set(gate.SKILL_APIS))
        self.assertIn("complete", gate.SKILL_APIS["goal"])
        self.assertIn("send", gate.SKILL_APIS["agent_message"])

    def test_cold_installer_receives_all_checkout_skills_only_after_new_venv_sync(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "prime-agent-runtime"
            WorkspaceGateTests().runtime(source)
            bootstrap = root / "python"
            bootstrap.write_text("fixture")
            bootstrap.chmod(0o755)
            env = gate.isolated_environment(root, root / "state", root / "target", root / "cargo",
                                            {"PA_WORKSPACE_BOOTSTRAP_PYTHON": str(bootstrap)})
            skills = [(name, name, root / "skills" / name, "src/" + name)
                      for name in gate.SKILL_APIS]
            with patch.object(gate, "bundled_skills", return_value=skills), \
                    patch.object(gate, "verify_skills") as verify, \
                    patch.object(gate, "run", side_effect=[None, None, None, WorkspaceGateTests().observation(source)]) as command:
                gate.prepare_kernel(root, env, 1)
            install = command.call_args_list[1].args[0]
            for _, _, skill_source, _ in skills:
                self.assertIn(str(skill_source), install)
            self.assertNotIn(str(source), install)
            self.assertIn("--no-deps", install)
            self.assertNotIn("--reinstall", install)
            self.assertEqual(install[install.index("--python") + 1],
                             str(Path(env["PRIME_AGENT_KERNEL_VENV"]) / "bin/python"))
            verify.assert_called_once()

    def test_skill_provenance_rejects_stale_copies_and_wrong_sources(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "skills/goal"
            package = source / "src/goal"
            package.mkdir(parents=True)
            (source / "pyproject.toml").write_text("manifest")
            (package / "__init__.py").write_text("async def complete(): pass")
            copied = root / "installed/goal"
            shutil.copytree(package, copied)
            skills = [("goal", "goal", source, "src/goal")]
            observation = {"goal": {"direct_url": {"url": source.as_uri()},
                                     "package_root": str(copied)}}
            def verify():
                gate.verify_skills(root, Path("/approved/python"),
                                   {"PRIME_AGENT_KERNEL_VENV": str(root / "kernel")}, 1, skills)
            with patch.object(gate, "run", return_value=json.dumps(observation)) as command:
                verify()
                self.assertEqual(command.call_args.args[0][:3], ["/approved/python", "-I", "-B"])
            (copied / "__init__.py").write_text("stale")
            with patch.object(gate, "run", return_value=json.dumps(observation)):
                with self.assertRaisesRegex(ValueError, "actually imported bundled skill goal"):
                    verify()
            observation["goal"]["direct_url"]["url"] = "https://example.invalid/goal"
            with patch.object(gate, "run", return_value=json.dumps(observation)):
                with self.assertRaisesRegex(ValueError, "local-source"):
                    verify()
            other = root / "other"
            shutil.copytree(source, other)
            (other / "pyproject.toml").write_text("wrong manifest")
            observation["goal"]["direct_url"]["url"] = other.as_uri()
            with patch.object(gate, "run", return_value=json.dumps(observation)):
                with self.assertRaisesRegex(ValueError, "source identity"):
                    verify()

    def test_approved_missing_skills_fail_without_installer_or_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "prime-agent-runtime"
            WorkspaceGateTests().runtime(source)
            observation = WorkspaceGateTests().observation(source)
            env = gate.isolated_environment(root, root / "state", root / "target", root / "cargo", {})
            with patch.object(gate, "bundled_skills", return_value=[("goal", "goal", root, "src/goal")]), \
                    patch.object(gate, "run", return_value=observation) as command, \
                    patch.object(gate, "verify_skills", side_effect=ValueError("missing goal")):
                with self.assertRaisesRegex(ValueError, "missing goal"):
                    gate.prepare_kernel(root, env, 1, root / "approved/bin/python")
            self.assertEqual(command.call_count, 2)
            self.assertEqual(command.call_args_list[0].args[0][:3], ["uv", "pip", "check"])
            self.assertFalse(any("install" in call.args[0] for call in command.call_args_list))
            self.assertNotIn("PA_E2E_KERNEL_PYTHON", env)
            self.assertFalse(Path(env["PRIME_AGENT_KERNEL_VENV"]).exists())



class VendorMluaGateTests(unittest.TestCase):
    def fixture(self, root):
        repo, state = root / "repo", root / "state"
        source = repo / "vendor/mlua"
        source.mkdir(parents=True)
        state.mkdir()
        (repo / "scripts").mkdir()
        (source / "Cargo.toml").write_text("manifest")
        (source / "source.rs").write_text("reviewed source")
        lock = b"reviewed locked graph"
        (repo / "vendor/mlua/Cargo.lock").write_bytes(lock)
        provenance = {"schema":1,"backend":"lua54","features":gate.VENDOR_MLUA_FEATURES.split(","),
            "manifest_sha256":gate.hashlib.sha256(b"manifest").hexdigest(),
            "lock_sha256":gate.hashlib.sha256(lock).hexdigest()}
        (repo / "scripts/vendor-mlua-lock.json").write_text(json.dumps(provenance))
        return repo, state

    def listing(self, command, **kwargs):
        if command[-1] == "--list":
            if "--lib" in command:
                return "\n".join("thread::wake_debt_tests::" + name + ": test" for name in gate.VENDOR_MLUA_CONTRACTS)
            return "\n".join(name + ": test" for name in gate.VENDOR_MLUA_HOOKS)
        return ""

    def test_exact_copy_locked_fetch_offline_named_contracts_full_hooks_and_shared_context(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, state = self.fixture(Path(tmp))
            env, deadline = {"CARGO_TARGET_DIR":"/one-target", "CARGO_HOME":"/one-cache"}, 123
            with patch.object(gate, "run", side_effect=self.listing) as run:
                copied, identity = gate.prepare_vendor_mlua(repo, state, env, deadline)
                gate.check_vendor_mlua(repo, copied, identity, env, deadline)
            commands = [call.args[0] for call in run.call_args_list]
            self.assertEqual(commands[0], ["cargo","fetch","--locked","--manifest-path",str(copied / "Cargo.toml")])
            self.assertEqual(len(commands), 13)
            for command in commands[1:]:
                self.assertIn("--locked", command)
                self.assertIn("--offline", command)
                self.assertIn("--no-default-features", command)
                self.assertEqual(command[command.index("--features")+1], "lua54,vendored,async,send,serialize")
            self.assertEqual([command[-2:] for command in commands[-3:]], [["--test", "hooks"], ["--test", "async"], ["--test", "thread"]])
            for call in run.call_args_list:
                self.assertIs(call.kwargs["env"], env)
                self.assertEqual(call.kwargs["deadline"], deadline)
                self.assertEqual(call.kwargs["repo"], repo)
            self.assertEqual((copied / "source.rs").read_bytes(), (repo / "vendor/mlua/source.rs").read_bytes())

    def test_source_mutation_during_cargo_gate_fails_final_identity_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, state = self.fixture(Path(tmp))
            with patch.object(gate, "run", return_value=""):
                copied, identity = gate.prepare_vendor_mlua(repo, state, {}, 1)
            def mutate(command, **kwargs):
                answer = self.listing(command, **kwargs)
                if command[-2:] == ["--test", "thread"]:
                    (copied / "source.rs").write_text("changed during test")
                return answer
            with patch.object(gate, "run", side_effect=mutate):
                with self.assertRaisesRegex(ValueError, "source changed"):
                    gate.check_vendor_mlua(repo, copied, identity, {}, 1)

    def test_missing_contract_fails_before_any_contract_execution(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, state = self.fixture(Path(tmp))
            with patch.object(gate, "run", return_value=""):
                copied, identity = gate.prepare_vendor_mlua(repo, state, {}, 1)
            with patch.object(gate, "run", return_value="") as run:
                with self.assertRaisesRegex(ValueError, "contract test missing"):
                    gate.check_vendor_mlua(repo, copied, identity, {}, 1)
            self.assertEqual(run.call_count, 1)

    def test_missing_hook_fails_before_named_contracts_execute(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, state = self.fixture(Path(tmp))
            with patch.object(gate, "run", return_value=""):
                copied, identity = gate.prepare_vendor_mlua(repo, state, {}, 1)
            listing = "\n".join("thread::wake_debt_tests::" + name + ": test" for name in gate.VENDOR_MLUA_CONTRACTS)
            with patch.object(gate, "run", side_effect=[listing, ""]) as run:
                with self.assertRaisesRegex(ValueError, "hook regression missing"):
                    gate.check_vendor_mlua(repo, copied, identity, {}, 1)
            self.assertEqual(run.call_count, 2)

    def test_provenance_mismatch_and_source_symlink_fail_before_fetch(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, state = self.fixture(Path(tmp))
            (repo / "vendor/mlua/Cargo.lock").write_text("changed lock")
            with patch.object(gate, "run") as run:
                with self.assertRaisesRegex(ValueError, "provenance mismatch"):
                    gate.prepare_vendor_mlua(repo, state, {}, 1)
                run.assert_not_called()
            (repo / "vendor/mlua/link").symlink_to("source.rs")
            with patch.object(gate, "run") as run:
                with self.assertRaisesRegex(ValueError, "symlinks"):
                    gate.prepare_vendor_mlua(repo, state, {}, 1)
                run.assert_not_called()

    def test_copy_or_lock_mutation_fails_before_offline_execution(self):
        for changed in ("source.rs", "Cargo.lock"):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as tmp:
                repo, state = self.fixture(Path(tmp))
                with patch.object(gate, "run", return_value=""):
                    copied, identity = gate.prepare_vendor_mlua(repo, state, {}, 1)
                (copied / changed).write_text("mutated")
                with patch.object(gate, "run") as run:
                    with self.assertRaisesRegex(ValueError, "identity mismatch|source changed"):
                        gate.check_vendor_mlua(repo, copied, identity, {}, 1)
                    run.assert_not_called()

    def test_copied_package_cannot_join_outer_workspace_or_reuse_existing_copy(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, state = self.fixture(Path(tmp))
            with patch.object(gate, "run") as run:
                with self.assertRaisesRegex(ValueError, "outside the workspace"):
                    gate.prepare_vendor_mlua(repo, repo / "state", {}, 1)
                run.assert_not_called()
            with patch.object(gate, "run", return_value=""):
                gate.prepare_vendor_mlua(repo, state, {}, 1)
                with self.assertRaises(FileExistsError):
                    gate.prepare_vendor_mlua(repo, state, {}, 1)


if __name__ == "__main__":
    unittest.main()
