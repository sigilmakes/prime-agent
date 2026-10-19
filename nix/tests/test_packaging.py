import importlib.util
from pathlib import Path
import tempfile
import unittest


def load(name):
    path = Path(__file__).resolve().parents[1] / f"{name}.py"
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


privacy = load("check_privacy")
payload = load("stage_payload")
workflows = load("check_workflows")


class PackagingTests(unittest.TestCase):
    def test_workflow_mutants_reject_upstream_activation_and_publication(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            forgejo = root / ".forgejo" / "workflows"
            forgejo.mkdir(parents=True)
            ci = forgejo / "ci.yml"
            ci.write_text("jobs: {check: {steps: [{run: nix flake check}]}}")
            self.assertEqual(workflows.errors(root), [])
            for command in ("git push origin main", "npm publish", "gh release create v1"):
                ci.write_text(f"jobs: {{release: {{steps: [{{run: {command}}}]}}}}")
                with self.subTest(command=command):
                    self.assertTrue(workflows.errors(root))
            ci.write_text("jobs: {check: {steps: [{run: nix flake check}]}}")
            github = root / ".github" / "workflows"
            github.mkdir(parents=True)
            (github / "release.yml").write_text("on: push")
            self.assertTrue(workflows.errors(root))

    def test_artifact_mutants_reject_endpoints_callbacks_and_trace_auth(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            binary = root / "prime-agent"
            binary.write_bytes(b"\x7fELF clean payload")
            self.assertEqual(privacy.artifact_errors(root), [])
            for marker in privacy.FORBIDDEN:
                binary.write_bytes(b"\x7fELF\x00" + marker + b"\x00")
                with self.subTest(marker=marker):
                    self.assertTrue(privacy.artifact_errors(root))

    def test_artifact_follows_binary_symlink_outside_payload(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            package = root / "payload"
            package.mkdir()
            executable = root / "executable"
            executable.write_bytes(b"agent-analytics/events")
            (package / "prime-agent").symlink_to(executable)
            self.assertTrue(privacy.artifact_errors(package))

    def test_source_mutants_reject_reporter_wiring_but_allow_compatibility_keys(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source = root / "crates" / "pa-core" / "src"
            source.mkdir(parents=True)
            (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/pa-core"]\n')
            module = source / "engine.rs"
            module.write_text('// ContinuousTraceUpload was removed\nconst KEY: &str = "agentTraces";')
            self.assertEqual(privacy.source_errors(root), [])
            for mutation in (
                'use pa_telemetry::TelemetryClient;',
                'ContinuousTraceUpload::install();',
                'install_session_telemetry();',
                'AnalyticsSink::new();',
            ):
                module.write_text(mutation)
                with self.subTest(mutation=mutation):
                    self.assertTrue(privacy.source_errors(root))

    def test_stage_keeps_lock_and_runtime_but_excludes_developer_state(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            source, target = root / "source", root / "payload"
            source.mkdir()
            (source / "Cargo.toml").write_text('[workspace.package]\nversion = "0.10.0"\n')
            for name in ("README.md", "LICENSE"):
                (source / name).write_text(name)
            runtime = source / "prime-agent-runtime"
            (runtime / "src" / "rlm" / "__pycache__").mkdir(parents=True)
            (runtime / "src" / "rlm" / "__init__.py").write_text("# runtime")
            (runtime / "src" / "rlm" / "__pycache__" / "bad.pyc").write_bytes(b"bad")
            for name in ("pyproject.toml", "uv.lock"):
                (runtime / name).write_text(name)
            (source / "skills" / "sample").mkdir(parents=True)
            (source / "skills" / "sample" / "SKILL.md").write_text("sample")
            assets = source / "crates" / "pa-core" / "src" / "snapcompact" / "assets"
            (assets / "licenses").mkdir(parents=True)
            (assets / "licenses" / "NOTICE").write_text("Poppy Works attribution")
            (assets / "Silver.ttf").write_bytes(b"embedded font")
            vendor = source / "vendor" / "mlua"
            vendor.mkdir(parents=True)
            (vendor / "LICENSE").write_text("vendored MIT attribution")
            payload.stage(source, target)
            self.assertEqual((target / "prime-agent-runtime" / "uv.lock").read_text(), "uv.lock")
            self.assertTrue((target / "prime-agent-runtime" / "src" / "rlm" / "__init__.py").is_file())
            self.assertFalse(list(target.rglob("*.pyc")))
            self.assertTrue((target / "skills" / "sample" / "SKILL.md").is_file())
            self.assertEqual(
                (target / "licenses" / "snapcompact" / "NOTICE").read_text(),
                "Poppy Works attribution",
            )
            self.assertFalse(list(target.rglob("*.ttf")))
            self.assertEqual(
                (target / "licenses" / "mlua" / "LICENSE").read_text(),
                "vendored MIT attribution",
            )


if __name__ == "__main__":
    unittest.main()
