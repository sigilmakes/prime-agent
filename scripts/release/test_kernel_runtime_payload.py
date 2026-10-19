"""Locked kernel sidecars must survive both release staging paths."""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import package_release
from assemble_artifacts import copy_runtime_tree


class KernelRuntimePayloadTest(unittest.TestCase):
    def test_both_packagers_keep_lock_manifest_sources_and_drop_caches(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "runtime"
            expected = {"pyproject.toml": "[project]\n", "uv.lock": "version = 1\n", "src/rlm/repl.py": "pass\n"}
            for name, content in {**expected, "test/test_runtime.py": "pass\n", ".venv/cache": "cache", "src/rlm/__pycache__/repl.pyc": "cache"}.items():
                path = source / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
            for name, copy in [
                ("assembled", copy_runtime_tree),
                ("local", lambda source, target: package_release.copy_tree(source, target, extra_excluded_names=package_release.RUNTIME_EXCLUDED_NAMES, extra_excluded_suffixes=package_release.RUNTIME_EXCLUDED_SUFFIXES)),
            ]:
                target = root / name
                copy(source, target)
                actual = {str(path.relative_to(target)): path.read_text() for path in target.rglob("*") if path.is_file()}
                self.assertEqual(actual, expected)

    def test_local_validation_requires_runtime_lockfile(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for directory in package_release.REQUIRED_DIRS:
                (root / directory).mkdir(parents=True, exist_ok=True)
            for name in package_release.REQUIRED_FILES:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("payload")
            (root / "prime-agent").chmod(0o755)
            package_release.validate(root, "0.10.0", "prime-agent")
            (root / "prime-agent-runtime/uv.lock").unlink()
            with self.assertRaisesRegex(SystemExit, "uv.lock"):
                package_release.validate(root, "0.10.0", "prime-agent")


if __name__ == "__main__":
    unittest.main()
