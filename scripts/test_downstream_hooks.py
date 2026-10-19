#!/usr/bin/env python3
"""Offline hook regressions. Run in the dev shell; no dependency installation."""

import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("downstream_hooks", ROOT / "scripts/downstream-hooks.py")
hooks = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hooks)
A = "a" * 40
B = "b" * 40
ZERO = "0" * 40


def payload(local="refs/heads/main", source=A, remote="refs/heads/main", target=B):
    return f"{local} {source} {remote} {target}\n".encode("ascii")


class PushValidation(unittest.TestCase):
    def validate(self, data=None, remote="origin", url=hooks.ORIGIN_URL, override=False):
        hooks.validate_push(remote, url, payload() if data is None else data, override)

    def test_exact_downstream_destinations(self):
        for url in sorted(hooks.ALLOWED_URLS):
            with self.subTest(url=url):
                self.validate(url=url)
        self.validate(b"")  # Git may have no updates to send.

    def test_other_destinations_are_never_authorized(self):
        cases = [
            ("upstream", hooks.ORIGIN_URL), ("github", hooks.ORIGIN_URL),
            ("scratch", hooks.ORIGIN_URL), (hooks.ORIGIN_URL, hooks.ORIGIN_URL),
            ("origin", "git@github.com:sigilzero/prime-agent.git"),
            ("origin", "https://github.com/PrimeIntellect-ai/prime-agent.git"),
            ("origin", "ssh://git@mnemosyne.sigilzero.dev/other/prime-agent.git"),
            ("origin", "ssh://git@mnemosyne.sigilzero.dev/sigilzero/prime-agent.git/extra"),
            ("origin", "https://mnemosyne.sigilzero.dev.evil/sigilzero/prime-agent.git"),
            ("origin", "https://mnemosyne.sigilzero.dev@evil/sigilzero/prime-agent.git"),
            ("origin", "https://evil/mnemosyne.sigilzero.dev/sigilzero/prime-agent.git"),
            ("origin", "file:///tmp/prime-agent.git"),
            ("origin", "ssh://git@mnemosyne.sigilzero.dev:2222/sigilzero/prime-agent.git"),
            ("origin", hooks.ORIGIN_URL + "\n"), ("origin", ""),
        ]
        for remote, url in cases:
            for override in (False, True):
                with self.subTest(remote=remote, url=url, override=override):
                    with self.assertRaises(hooks.Refused):
                        self.validate(remote=remote, url=url, override=override)

    def test_create_tag_detached_and_sha256_updates(self):
        self.validate(payload(target=ZERO))
        self.validate(payload(local="refs/tags/v1", remote="refs/tags/v1"))
        self.validate(payload(local="HEAD"))
        self.validate(payload(local=A))
        self.validate(payload(source="a" * 64, target="0" * 64))

    def test_legacy_destructive_gate_requires_explicit_override(self):
        many = b"".join(payload(remote=f"refs/heads/b{i}") for i in range(11))
        self.validate(b"".join(payload(remote=f"refs/heads/b{i}") for i in range(10)))
        for data in [many, payload(local="(delete)", source=ZERO), payload(remote="refs/notes/test")]:
            with self.subTest(data=data):
                with self.assertRaises(hooks.Refused):
                    self.validate(data)
                self.validate(data, override=True)

    def test_invalid_payloads_fail_closed_even_with_override(self):
        malformed = [
            b"\n", b" \n", b"garbage\n", payload().rstrip(b"\n"),
            payload().replace(b" ", b"\t"), payload().replace(b" ", b"  ", 1),
            payload() + b"\n", payload() + b"invalid\n", payload() + payload(),
            payload(source="a"), payload(target="z" * 40), payload(source="A" * 40),
            payload(source="a" * 64), payload(source=ZERO),
            payload() + payload(source="a" * 64, target="b" * 64, remote="refs/heads/other"),
            payload(local="(delete)"), payload(local="(delete)", source=ZERO, target=ZERO),
            payload(remote="main"), payload(remote="refs/heads/../main"),
            payload(remote="refs/heads/x.lock"), payload(remote="refs/heads/x\n"),
            payload(remote="refs/heads/"), payload(remote="refs/heads/x@{y"),
            payload(local="--help"), payload(local="refs/heads/../main"),
            payload().replace(b"main", b"ma\x00in"), b"\xff\n",
            b"x" * (hooks.MAX_PAYLOAD + 1),
        ]
        for data in malformed:
            for override in (False, True):
                with self.subTest(data=data[:160], override=override):
                    with self.assertRaises(hooks.Refused):
                        self.validate(data, override=override)


class HookIntegration(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="downstream-hook-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_OPTIONAL_LOCKS="0", PYTHONDONTWRITEBYTECODE="1")
        # Do not let an invoking Git hook redirect these disposable test repositories.
        for key in list(self.env):
            if key.startswith(("PRIME_AGENT_", "RLM_", "PI_", "GIT_CONFIG_COUNT",
                               "GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_")) or key in {
                "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR",
                "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "PRIME_AGENT_ALLOW_MIRROR_PUSH",
            }:
                self.env.pop(key)
        self.git("init", "--quiet")
        self.git("config", "core.hooksPath", ".husky")
        (self.repo / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/downstream-hooks.py", self.repo / "scripts/downstream-hooks.py")
        shutil.copytree(ROOT / ".husky", self.repo / ".husky")
        self.file = self.repo / "partial.txt"
        self.file.write_text("staged content\n")
        self.git("add", "partial.txt")
        self.file.write_text("staged content\nunstaged content  \n")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, env=self.env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True).stdout

    def snapshot(self):
        # Include byte-for-byte index, config, hook scripts, staged blob, and worktree.
        files = {str(p.relative_to(self.repo)): (p.read_bytes(), p.stat().st_mode)
                 for p in self.repo.rglob("*") if p.is_file()}
        return files, self.git("diff", "--cached", "--binary"), self.git("diff", "--binary")

    def hook(self, name, args=(), data=b"", override=None):
        before = self.snapshot()
        env = dict(self.env)
        if override is not None:
            env["PRIME_AGENT_ALLOW_MIRROR_PUSH"] = override
        result = subprocess.run([str(self.repo / ".husky" / name), *args],
                                cwd=self.repo, env=env, input=data,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        self.assertEqual(self.snapshot(), before, "hook mutated repository or index")
        return result

    def test_precommit_preserves_partial_staging_and_checks_only_index(self):
        self.assertEqual(self.hook("pre-commit").returncode, 0)
        self.file.write_text("bad staged whitespace  \n")
        self.git("add", "partial.txt")
        self.file.write_text("clean unstaged replacement\n")
        self.assertNotEqual(self.hook("pre-commit").returncode, 0)

    def test_prepush_paths_preserve_repository(self):
        args = ("origin", hooks.ORIGIN_URL)
        self.assertEqual(self.hook("pre-push", args, payload()).returncode, 0)
        self.assertNotEqual(self.hook("pre-push", args, b"bad\n").returncode, 0)
        self.assertNotEqual(self.hook("pre-push", ("github", hooks.ORIGIN_URL), payload(), "1").returncode, 0)
        delete = payload(local="(delete)", source=ZERO)
        self.assertNotEqual(self.hook("pre-push", args, delete).returncode, 0)
        self.assertNotEqual(self.hook("pre-push", args, delete, "true").returncode, 0)
        self.assertEqual(self.hook("pre-push", args, delete, "1").returncode, 0)
        self.assertNotEqual(self.hook("pre-push", args, b"bad\n", "1").returncode, 0)

    def test_invalid_hook_arguments_fail_closed(self):
        for args in [(), ("origin",), ("origin", hooks.ORIGIN_URL, "extra")]:
            self.assertNotEqual(self.hook("pre-push", args).returncode, 0)
        self.assertNotEqual(self.hook("pre-commit", ("extra",)).returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
