from __future__ import annotations

import asyncio
import copy
import unittest
from unittest.mock import AsyncMock

from rlm.plugins import PluginExport, PluginNamespace
from test_repl import ReplProcess


def descriptor(plugin="atlas", name="echo", generation="g1", **extra):
    return {
        "plugin": plugin, "name": name, "generation": generation,
        "description": "Echo structured data 🌿", "mode": "unary", **extra,
    }


def catalog(revision=1, exports=None, session="session-A"):
    return {
        "session": session, "revision": revision,
        "exports": [descriptor(), descriptor("notes", "save")] if exports is None else exports,
    }


class PluginNamespaceTest(unittest.IsolatedAsyncioTestCase):
    async def test_two_plugins_and_kwargs_json_roundtrip(self):
        transport = AsyncMock(side_effect=[catalog(), {"output": [None, "雪", {"ok": True}]}])
        plugin = PluginNamespace("session-A", transport=transport)
        self.assertIsNone(plugin.revision)
        self.assertEqual(plugin.inventory, ())
        self.assertTrue(await plugin.refresh())
        self.assertEqual(plugin.session, "session-A")
        self.assertEqual(plugin.revision, 1)
        self.assertEqual(dir(plugin.atlas), ["echo"])
        self.assertEqual(dir(plugin.notes), ["save"])
        self.assertEqual(plugin.inventory, (
            PluginExport("session-A", "atlas", "g1", "echo", "Echo structured data 🌿", "unary"),
            PluginExport("session-A", "notes", "g1", "save", "Echo structured data 🌿", "unary"),
        ))
        self.assertEqual(plugin.atlas.echo.__doc__, "Echo structured data 🌿")
        value = {"text": "雪🌿", "nested": [None, {"flag": False, "number": 4.5}]}
        result = await plugin.atlas.echo(**value)
        self.assertEqual(result, {"output": [None, "雪", {"ok": True}]})
        self.assertEqual(transport.await_args_list[0].args, ("plugin.catalog", {"session": "session-A"}))
        self.assertEqual(transport.await_args_list[1].args, ("plugin.invoke", {
            "session": "session-A", "generation": "g1", "plugin": "atlas",
            "export": "echo", "input": value,
        }))

    async def test_null_result_and_no_arguments(self):
        transport = AsyncMock(side_effect=[catalog(), None])
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        self.assertIsNone(await plugin.notes.save())
        self.assertEqual(transport.await_args.args[1]["input"], {})

    async def test_generation_update_disable_and_saved_callable_binding(self):
        transport = AsyncMock(side_effect=[
            catalog(), catalog(2, [descriptor(generation="g2")]),
            "old generation outcome", "new generation outcome", catalog(3, []),
        ])
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        old = plugin.atlas.echo
        view = plugin.notes
        await plugin.refresh()
        self.assertNotIn("notes", dir(plugin))
        self.assertFalse(hasattr(plugin, "notes"))
        self.assertFalse(hasattr(view, "save"))
        self.assertEqual(dir(view), [])
        self.assertEqual(await old(), "old generation outcome")
        self.assertEqual(await plugin.atlas.echo(), "new generation outcome")
        self.assertEqual([call.args[1]["generation"] for call in transport.await_args_list[2:4]], ["g1", "g2"])
        await plugin.refresh()
        self.assertFalse(hasattr(plugin, "atlas"))
        self.assertEqual(plugin.inventory, ())

    async def test_removed_function_disappears_from_saved_plugin_view(self):
        transport = AsyncMock(side_effect=[
            catalog(1, [descriptor(), descriptor(name="ping")]),
            catalog(2, [descriptor(name="ping", generation="g2")]),
        ])
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        view, saved = plugin.atlas, plugin.atlas.echo
        await plugin.refresh()
        self.assertFalse(hasattr(plugin.atlas, "echo"))
        self.assertFalse(hasattr(view, "echo"))
        self.assertEqual(dir(view), ["ping"])
        self.assertEqual([(e.name, e.generation) for e in plugin.inventory], [("ping", "g2")])
        self.assertEqual(saved.descriptor.generation, "g1")

    async def test_transport_errors_propagate_and_do_not_publish(self):
        failure = RuntimeError("plugin generation retired")
        transport = AsyncMock(side_effect=[catalog(), failure, failure])
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        with self.assertRaises(RuntimeError) as error:
            await plugin.atlas.echo()
        self.assertIs(error.exception, failure)
        before = plugin.inventory
        with self.assertRaises(RuntimeError) as error:
            await plugin.refresh()
        self.assertIs(error.exception, failure)
        self.assertEqual(plugin.inventory, before)
        self.assertEqual(plugin.revision, 1)

    async def test_wrong_session_catalog_and_same_name_descriptor_rejected(self):
        transport = AsyncMock(side_effect=[
            catalog(), catalog(2, session="session-B"),
            catalog(2, [descriptor(session="session-B")]),
        ])
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        saved = plugin.atlas.echo
        for _ in range(2):
            with self.assertRaisesRegex(ValueError, "session"):
                await plugin.refresh()
            self.assertIs(plugin.atlas.echo, saved)
            self.assertEqual(plugin.revision, 1)

    async def test_malformed_catalog_is_atomic(self):
        invalid = [
            None, [], {}, catalog(-1), catalog(True),
            {**catalog(), "exports": {}}, catalog(2, [None]),
            catalog(2, [descriptor(), descriptor("notes", "save", description=None)]),
            catalog(2, [descriptor(), descriptor()]),
            catalog(2, [descriptor(), descriptor(name="other", generation="g2")]),
            catalog(2, [descriptor(mode="unknown")]),
            catalog(2, [descriptor(generation="")]),
            catalog(2, [descriptor(kind="command")]),
            catalog(2, [descriptor(kind="hook")]),
            catalog(2, [descriptor(plugin="refresh")]),
            catalog(2, [descriptor(name="class")]),
            catalog(2, [descriptor(name="_hidden")]),
            catalog(2, [descriptor(plugin="not-a-name")]),
        ]
        transport = AsyncMock(return_value=catalog())
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        before, saved = plugin.inventory, plugin.atlas.echo
        for malformed in invalid:
            with self.subTest(catalog=malformed):
                transport.return_value = malformed
                with self.assertRaises(ValueError):
                    await plugin.refresh()
                self.assertEqual(plugin.inventory, before)
                self.assertIs(plugin.atlas.echo, saved)
                self.assertEqual(plugin.revision, 1)

    async def test_concurrent_refresh_does_not_publish_stale_reply(self):
        first_started, second_started = asyncio.Event(), asyncio.Event()
        first_reply = asyncio.get_running_loop().create_future()
        second_reply = asyncio.get_running_loop().create_future()
        calls = 0

        async def transport(kind, payload):
            nonlocal calls
            calls += 1
            if calls == 1:
                first_started.set()
                return await first_reply
            second_started.set()
            return await second_reply

        plugin = PluginNamespace("session-A", transport=transport)
        first = asyncio.create_task(plugin.refresh())
        await first_started.wait()
        second = asyncio.create_task(plugin.refresh())
        await second_started.wait()
        second_reply.set_result(catalog(2, [descriptor(generation="g2")]))
        self.assertTrue(await second)
        # Even an old request carrying a higher revision cannot replace a
        # later refresh's published view (e.g. a reordered host adapter).
        first_reply.set_result(catalog(3))
        self.assertFalse(await first)
        self.assertEqual(plugin.revision, 2)
        self.assertEqual(plugin.inventory[0].generation, "g2")
        self.assertFalse(hasattr(plugin, "notes"))

    async def test_revision_cannot_regress_or_change_without_increment(self):
        transport = AsyncMock(side_effect=[catalog(2), catalog(1, []), catalog(2, [])])
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        before = plugin.inventory
        self.assertFalse(await plugin.refresh())
        with self.assertRaisesRegex(ValueError, "revision"):
            await plugin.refresh()
        self.assertEqual(plugin.inventory, before)
        self.assertEqual(plugin.revision, 2)

    async def test_empty_catalog_is_normal_and_has_no_plugin_specific_names(self):
        transport = AsyncMock(return_value=catalog(0, []))
        plugin = PluginNamespace("session-A", transport=transport)
        self.assertTrue(await plugin.refresh())
        self.assertEqual(plugin.inventory, ())
        self.assertEqual(dir(plugin), ["inventory", "refresh", "revision", "session"])
        self.assertFalse(hasattr(plugin, "unknown"))

    async def test_streaming_is_explicitly_unsupported_without_invocation(self):
        transport = AsyncMock(return_value=catalog(1, [descriptor(mode="streaming")]))
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        with self.assertRaisesRegex(NotImplementedError, "streaming"):
            await plugin.atlas.echo()
        self.assertEqual(transport.await_count, 1)

    async def test_non_json_and_positional_arguments_fail_before_transport(self):
        transport = AsyncMock(return_value=catalog())
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        for value in (object(), {1: "silently coerced key"}, float("nan"), float("inf")):
            with self.subTest(value=value):
                with self.assertRaises((TypeError, ValueError)):
                    await plugin.atlas.echo(value=value)
        with self.assertRaises(TypeError):
            await plugin.atlas.echo("positional")
        self.assertEqual(transport.await_count, 1)

    async def test_caller_mutation_cannot_rewrite_published_descriptors(self):
        reply = catalog()
        transport = AsyncMock(return_value=reply)
        plugin = PluginNamespace("session-A", transport=transport)
        await plugin.refresh()
        before = copy.deepcopy(plugin.inventory)
        reply["exports"][0]["generation"] = "changed"
        reply["exports"].clear()
        self.assertEqual(plugin.inventory, before)

    async def test_invalid_session_is_rejected_without_transport(self):
        for session in (None, "", 4):
            with self.assertRaises(ValueError):
                PluginNamespace(session)


class PluginKernelFramingTest(unittest.TestCase):
    def setUp(self):
        self.repl = ReplProcess()
        self.addCleanup(self.repl.close)
        ready, _ = self.repl.ready()
        self.assertEqual(ready["event"], "ready")

    def next_host_request(self):
        for _ in range(50):
            event = self.repl.read_event(timeout=10)
            if event.get("event") == "host_request":
                return event
            self.assertNotEqual(event.get("event"), "error", event)
            self.assertNotEqual(event.get("event"), "done", event)
        self.fail("kernel did not issue a host request")

    def reply(self, request, result=None, error=None):
        data = {"status": "ok", "result": result} if error is None else {"status": "error", "error": error}
        self.repl.send({"type": "host_reply", "id": request["id"], "data": data})

    def done_ok(self, cell):
        events = self.repl.until_done(cell)
        self.assertEqual(events[-1]["status"], "ok", events)
        return events

    def test_native_kernel_framing_catalog_invoke_reload_and_host_error(self):
        # Real rlm.repl subprocess, real host_request framing and reply parser.
        # Only the native host is local/fake; no daemon, provider or files.
        self.repl.send({"type": "execute", "id": "catalog", "code": (
            "from rlm.plugins import PluginNamespace\n"
            "plugin = PluginNamespace('session-A')\n"
            "await plugin.refresh()\n"
            "saved = plugin.atlas.echo\n"
            "assert dir(plugin.notes) == ['save']"
        )})
        request = self.next_host_request()
        self.assertEqual(request["data"], {"type": "plugin.catalog", "session": "session-A"})
        self.reply(request, catalog())
        self.done_ok("catalog")

        value = {"text": "雪🌿", "nested": [None, {"flag": True, "count": 2}]}
        self.repl.send({"type": "execute", "id": "invoke", "code": (
            f"assert await plugin.atlas.echo(**{value!r}) == {value!r}\n"
            "assert await plugin.notes.save(value=None) is None"
        )})
        request = self.next_host_request()
        self.assertEqual(request["data"], {
            "type": "plugin.invoke", "session": "session-A", "generation": "g1",
            "plugin": "atlas", "export": "echo", "input": value,
        })
        self.reply(request, value)
        request = self.next_host_request()
        self.assertEqual(request["data"], {
            "type": "plugin.invoke", "session": "session-A", "generation": "g1",
            "plugin": "notes", "export": "save", "input": {"value": None},
        })
        self.reply(request, None)
        self.done_ok("invoke")

        self.repl.send({"type": "execute", "id": "reload", "code": (
            "await plugin.refresh()\n"
            "assert not hasattr(plugin, 'notes')\n"
            "assert 'notes' not in dir(plugin)\n"
            "assert plugin.inventory[0].generation == 'g2'\n"
            "await saved()"
        )})
        request = self.next_host_request()
        self.assertEqual(request["data"], {"type": "plugin.catalog", "session": "session-A"})
        self.reply(request, catalog(2, [descriptor(generation="g2")]))
        request = self.next_host_request()
        self.assertEqual(request["data"], {
            "type": "plugin.invoke", "session": "session-A", "generation": "g1",
            "plugin": "atlas", "export": "echo", "input": {},
        })
        self.reply(request, error="plugin generation retired")
        events = self.repl.until_done("reload")
        self.assertEqual(events[-1]["status"], "error", events)
        errors = [e for e in events if e.get("event") == "error"]
        self.assertEqual([(e["ename"], e["evalue"]) for e in errors], [
            ("RuntimeError", "plugin generation retired"),
        ])
        self.assertEqual(self.repl.shutdown(), 0)


if __name__ == "__main__":
    unittest.main()
