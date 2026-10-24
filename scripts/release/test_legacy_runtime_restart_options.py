#!/usr/bin/env python3
"""Lightweight CLI-source selection checks; no daemons are started."""
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "legacy_restart", Path(__file__).with_name("test_legacy_runtime_restart.py"))
restart = importlib.util.module_from_spec(spec)
spec.loader.exec_module(restart)


class FixtureBoundary(Exception):
    pass


class LegacySourceOptions(unittest.TestCase):
    def argv(self, *sources):
        return ["legacy-restart", *sources, "--candidate-binary", "/candidate",
                "--report", "/unused/receipt.json"]

    def test_each_source_is_accepted(self):
        for source in ("--legacy-archive", "--legacy-npm-prefix", "--legacy-binary"):
            with self.subTest(source=source), mock.patch("sys.argv", self.argv(source, "/legacy")):
                with mock.patch.object(restart.tempfile, "TemporaryDirectory", side_effect=FixtureBoundary):
                    with self.assertRaises(FixtureBoundary):
                        restart.main()

    def test_source_is_required(self):
        with mock.patch("sys.argv", self.argv()), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                restart.main()
            self.assertEqual(error.exception.code, 2)

    def test_sources_cannot_be_combined(self):
        sources = ("--legacy-archive", "--legacy-npm-prefix", "--legacy-binary")
        for index, left in enumerate(sources):
            for right in sources[index + 1:]:
                with self.subTest(left=left, right=right):
                    with mock.patch("sys.argv", self.argv(left, "/one", right, "/two")):
                        with contextlib.redirect_stderr(io.StringIO()):
                            with self.assertRaises(SystemExit) as error:
                                restart.main()
                            self.assertEqual(error.exception.code, 2)

    def test_binary_is_invoked_directly(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "legacy-wrapper"
            binary.write_text("wrapper sentinel")
            fixture = root / "fixture"
            fixture.mkdir()
            with mock.patch("sys.argv", self.argv("--legacy-binary", str(binary))):
                with mock.patch.object(restart.tempfile, "TemporaryDirectory") as temporary:
                    temporary.return_value.__enter__.return_value = str(fixture)
                    with mock.patch.object(restart.subprocess, "Popen", side_effect=FixtureBoundary) as launch:
                        with self.assertRaises(FixtureBoundary):
                            restart.main()
            self.assertEqual(launch.call_args.args[0], [str(binary), "--mode", "daemon",
                                                      "--daemon-socket", str(fixture / "tmp/daemon.sock")])


if __name__ == "__main__":
    unittest.main()
