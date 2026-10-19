from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from test_repl import ReplProcess


class HarnessPluginRuntimeTest(unittest.TestCase):
    """Exercise the optional harness through the real kernel/host bridge.

    Existing stores belong to the native plugin. Kernel startup, bridge calls,
    and retirement must leave their bytes and file identities untouched.
    """

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.stores = [self.root / "local", self.root / "global"]
        for index, store in enumerate(self.stores):
            store.mkdir()
            (store / "harness_state.json").write_text(json.dumps({
                "version": 1,
                "entries": {"memory": {"keep": {
                    "id": "keep", "kind": "memory", "title": "Preserve me",
                    "content": f"Existing scope {index}",
                }}},
                "refinements": [],
            }), encoding="utf-8")
        agent = self.root / "agent"
        agent.mkdir()
        (agent / "settings.json").write_text(
            '{"harnessRefinements": true}', encoding="utf-8"
        )
        self.before = self.store_snapshot()
        self.repl = ReplProcess(env={
            "HOME": str(self.root),
            "PRIME_AGENT_CODING_AGENT_DIR": str(agent),
            "RLM_SESSION_DIR": str(self.root / "session"),
            "RLM_HARNESS_STATE_DIR": str(self.stores[0]),
            "RLM_GLOBAL_HARNESS_STATE_DIR": str(self.stores[1]),
        })
        self.addCleanup(self.repl.close)
        self.assertEqual(self.repl.ready()[0]["event"], "ready")
        boot = self.repl.execute("boot", (
            "from rlm.plugins import PluginNamespace\n"
            "plugin = PluginNamespace('session-test')"
        ))
        self.assertEqual(boot[-1]["status"], "ok", boot)

    def store_snapshot(self) -> dict:
        return {
            str(path.relative_to(self.root)): (
                path.read_bytes(), path.stat().st_mtime_ns, path.stat().st_ino
            )
            for store in self.stores for path in store.rglob("*") if path.is_file()
        }

    def request(self, cell: str, code: str, expected: dict, result: object) -> None:
        self.repl.send({"type": "execute", "id": cell, "code": code})
        request = self.repl.read_event()
        self.assertEqual(request["event"], "host_request", request)
        self.assertEqual(request["data"], expected)
        self.repl.send({
            "type": "host_reply", "id": request["id"],
            "data": {"status": "ok", "result": result},
        })
        events = self.repl.until_done(cell)
        self.assertEqual(events[-1]["status"], "ok", events)

    def refresh(self, cell: str, revision: int, exports: list) -> None:
        self.request(cell, "await plugin.refresh()", {
            "type": "plugin.catalog", "session": "session-test",
        }, {"session": "session-test", "revision": revision, "exports": exports})

    def test_empty_inventory_preserves_stores_and_core_execution(self) -> None:
        self.refresh("disabled", 1, [])
        events = self.repl.execute("inventory", (
            "assert plugin.inventory == ()\n"
            "assert not hasattr(plugin, 'harness')\n"
            "answer = 6 * 7\nassert answer == 42"
        ))
        self.assertEqual(events[-1]["status"], "ok", events)
        self.request("spawn", (
            "import rlm\n"
            "child = await rlm.spawn('Check arithmetic', name='checker')\n"
            "assert child.rlm_child_id == 'child-1'"
        ), {
            "type": "rlm.run", "prompt": "Check arithmetic", "kwargs": {"name": "checker"},
        }, {
            "rlm_child_id": "child-1", "name": "checker",
            "session_dir": str(self.root / "child"), "model": "fake/model",
        })
        self.assertEqual(self.repl.shutdown(), 0)
        self.assertEqual(self.store_snapshot(), self.before)

    def test_harness_export_routes_to_host_and_retirement_preserves_data(self) -> None:
        self.refresh("enabled", 1, [{
            "plugin": "harness", "generation": "generation-1", "name": "create_memory",
            "description": "Store a memory", "mode": "unary",
        }])
        self.request("create", (
            "result = await plugin.harness.create_memory(title='A fact', content='42')\n"
            "assert result == {'id': 'memory-1', 'content': '42'}"
        ), {
            "type": "plugin.invoke", "session": "session-test", "plugin": "harness",
            "generation": "generation-1", "export": "create_memory",
            "input": {"title": "A fact", "content": "42"},
        }, {"id": "memory-1", "content": "42"})
        saved = self.repl.execute("save", "saved_view = plugin.harness")
        self.assertEqual(saved[-1]["status"], "ok", saved)
        self.refresh("retired", 2, [])
        events = self.repl.execute("after", (
            "assert plugin.inventory == ()\n"
            "assert not hasattr(plugin, 'harness')\n"
            "assert dir(saved_view) == []"
        ))
        self.assertEqual(events[-1]["status"], "ok", events)
        self.assertEqual(self.repl.shutdown(), 0)
        self.assertEqual(self.store_snapshot(), self.before)


if __name__ == "__main__":
    unittest.main()
