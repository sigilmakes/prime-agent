from __future__ import annotations

import asyncio
import os
import sys
import tempfile
import unittest

from test_repl import SRC, ReplProcess, one


class PluginLifecycleTest(unittest.TestCase):
    def setUp(self):
        sys.path.insert(0, SRC)
        self.addCleanup(sys.path.remove, SRC)

    def test_refresh_is_deferred_until_a_user_frame_and_keeps_old_targets(self):
        repl = ReplProcess()
        self.addCleanup(repl.close)
        repl.ready()
        boot = repl.execute("bootstrap", "from rlm.plugins import PluginNamespace\n"
                            "_prime_agent_plugin_namespace = PluginNamespace('session-a')\n"
                            "plugin = _prime_agent_plugin_namespace")
        self.assertEqual(boot[-1]["status"], "ok")
        self.assertIsNone(one(boot, "host_request"))

        def catalog(rid, code, revision, generation):
            repl.send({"type": "execute", "id": rid, "code": code, "refresh_plugins": True})
            request = repl.read_event()
            self.assertEqual(request["event"], "host_request")
            self.assertEqual(request["data"], {"type": "plugin.catalog", "session": "session-a"})
            repl.send({"type": "host_reply", "id": request["id"], "data": {
                "status": "ok", "result": {"session": "session-a", "revision": revision, "exports": [{
                    "plugin": "counter", "generation": generation, "name": "read",
                    "description": "Read", "mode": "unary",
                }]},
            }})

        catalog("first", "saved = plugin.counter.read\nowner = id(plugin)", 1, "old")
        self.assertEqual(repl.until_done("first")[-1]["status"], "ok")
        catalog("second", "assert id(plugin) == owner\nassert plugin.counter.read.descriptor.generation == 'new'\nawait saved()", 2, "new")
        request = repl.read_event()
        self.assertEqual(request["event"], "host_request")
        self.assertEqual(request["data"], {
            "type": "plugin.invoke", "session": "session-a", "plugin": "counter",
            "generation": "old", "export": "read", "input": {},
        })
        repl.send({"type": "host_reply", "id": request["id"], "data": {
            "status": "error", "error": "plugin export generation is not active",
        }})
        failed = repl.until_done("second")
        self.assertEqual(one(failed, "error")["evalue"], "plugin export generation is not active")
        self.assertEqual(failed[-1]["status"], "error")

    def test_snapshot_excludes_plugin_aliases_containers_defaults_and_closures(self):
        from rlm import repl
        from rlm.plugins import PluginNamespace

        async def transport(kind, payload):
            return {"session": "old-session", "revision": 1, "exports": [{
                "plugin": "counter", "generation": "retired", "name": "read",
                "description": "Read", "mode": "unary",
            }]}

        plugin = PluginNamespace("old-session", transport=transport)
        asyncio.run(plugin.refresh())
        saved = plugin.counter.read

        def closed_over():
            return saved

        def with_default(target=saved):
            return target

        ns = {
            "plugin": plugin, "alias": plugin, "view": plugin.counter, "saved": saved,
            "nested": {"calls": [saved]}, "closure": closed_over, "default": with_default,
            "descriptor": saved.descriptor, "keep": {"answer": 42},
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "snapshot")
            result = repl._snapshot_state(ns, path, path + ".json", 1 << 20, 1 << 20, False)
            self.assertEqual(result["saved"], ["keep"])
            self.assertEqual({entry["name"] for entry in result["skipped"]}, set(ns) - {"plugin", "keep"})
            self.assertTrue(all("live plugin references" in entry["reason"] for entry in result["skipped"]))
            current = PluginNamespace("new-session")
            restored = {"plugin": current, "_prime_agent_plugin_namespace": current}
            outcome = repl._restore_state(restored, path)
            self.assertEqual(outcome, {"restored": ["keep"], "failed": []})
            self.assertIs(restored["plugin"], current)
            self.assertIs(restored["_prime_agent_plugin_namespace"], current)

    def test_legacy_snapshots_cannot_restore_old_namespace_or_bound_objects(self):
        import dill
        from rlm import repl
        from rlm.plugins import PluginExport, PluginNamespace

        # Build the legacy shape without using the guarded snapshot writer.
        legacy = {
            "plugin": dill.dumps(PluginNamespace("old-session")),
            "_prime_agent_plugin_namespace": dill.dumps(PluginNamespace("old-session")),
            "saved": dill.dumps(PluginExport("old-session", "counter", "old", "read", "", "unary")),
            "keep": dill.dumps(42),
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "snapshot")
            with open(path, "wb") as fh:
                dill.dump(legacy, fh)
            current = PluginNamespace("new-session")
            ns = {"plugin": current, "_prime_agent_plugin_namespace": current}
            result = repl._restore_state(ns, path)
            self.assertEqual(result["restored"], ["keep"])
            self.assertEqual([entry["name"] for entry in result["failed"]], ["saved"])
            self.assertIs(ns["plugin"], current)
            self.assertIs(ns["_prime_agent_plugin_namespace"], current)

    def test_catalog_failure_prevents_user_code(self):
        from rlm import repl
        from rlm.plugins import PluginNamespace

        async def transport(kind, payload):
            raise RuntimeError("host unavailable")

        namespace = PluginNamespace("session", transport=transport)
        ns = {"_prime_agent_plugin_namespace": namespace}
        codes, _ = repl._compile_cell("ran = True", "<cell-test>")
        with self.assertRaisesRegex(RuntimeError, "host unavailable"):
            asyncio.run(repl._run_codes(codes, ns, refresh_plugins=True))
        self.assertNotIn("ran", ns)
