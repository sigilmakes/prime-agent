"""Transport/isolation and red-first injection tests for the installed privacy gate.

The executable fixture tests the Python verifier, not the Rust product. Nix must
also invoke privacy_lifecycle.py on the actual installed prime-agent package.
"""

import importlib.util
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
from urllib.request import Request, urlopen


spec = importlib.util.spec_from_file_location(
    "privacy_lifecycle", Path(__file__).resolve().parents[1] / "privacy_lifecycle.py"
)
privacy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(privacy)


FIXTURE = r'''
import json
import os
from pathlib import Path
import sys
import threading
from urllib.request import Request, urlopen

MUTATION = __MUTATION__
home = Path(os.environ["HOME"])
agent = Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"])
assert (Path.cwd() / ".git").is_dir()
settings = json.loads((agent / "settings.json").read_text())
assert settings["telemetry"]["enabled"] is True
assert settings["agentTraces"]["enabled"] is True
assert not any(key in os.environ for key in ("DO_NOT_TRACK", "PI_OFFLINE", "PRIME_AGENT_TELEMETRY"))
assert os.environ["PRIME_AGENT_TRACES_API_KEY"].startswith("dummy-")
assert json.loads((agent / "auth.json").read_text())["prime-agent-traces"]["key"].startswith("dummy-")
for name in ("HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
             "XDG_RUNTIME_DIR", "TMPDIR", "PRIME_AGENT_DAEMON_SOCKET", "PRIME_AGENT_KERNEL_PYTHON",
             "PRIME_AGENT_KERNEL_VENV", "PRIME_AGENT_SESSION_DIR", "RLM_SESSION_DIR",
             "RLM_HARNESS_STATE_DIR", "RLM_GLOBAL_HARNESS_STATE_DIR"):
    assert Path(os.environ[name]).is_relative_to(home)
assert "AMBIENT_SECRET_API_KEY" not in os.environ
session = Path(os.environ["PRIME_AGENT_SESSION_DIR"]) / "fixture.jsonl"
resumed = "--resume" in sys.argv
if resumed:
    assert sys.argv[sys.argv.index("--resume") + 1] == str(session)
    messages = json.loads(session.read_text())
else:
    messages = []
if MUTATION == "timeout":
    threading.Event().wait(600)
if MUTATION == "telemetry":
    (agent / "telemetry.json").write_text("{}")
if MUTATION == "mirror":
    (agent / "telemetry.jsonl").write_text("{}\n")
if MUTATION == "sentinel":
    (agent / "agent-traces-outbox" / "sentinel.json").write_text("clobbered")
if MUTATION == "outbox":
    (agent / "agent-traces-outbox" / "new.json").write_text("{}")
if MUTATION == "collector":
    with urlopen(Request(os.environ["PRIME_AGENT_TRACES_BASE_URL"] + "/api/v1/agent-traces/sessions",
                         data=b"{}", headers={"Content-Type": "application/json"}), timeout=2) as response:
        response.read()

def emit(value):
    print(json.dumps(value), flush=True)

def stats():
    count = len(messages) // 2
    return {"sessionId": "fixture-id", "sessionFile": str(session),
            "userMessages": count, "assistantMessages": count, "messageCount": len(messages)}

for line in sys.stdin:
    frame = json.loads(line)
    kind = frame["type"]
    data = None
    if kind == "get_state":
        data = stats()
        if resumed and MUTATION == "bad-resume":
            data["sessionId"] = "wrong-session"
    elif kind == "get_session_stats":
        data = stats()
    elif kind == "prompt":
        messages.append({"role": "user", "content": frame["message"]})
        model = json.loads((agent / "models.json").read_text())["providers"]["privacy-fixture"]
        request = {"model": "privacy-test", "stream": True, "messages": messages}
        with urlopen(Request(model["baseUrl"] + "/chat/completions", data=json.dumps(request).encode(),
                             headers={"Content-Type": "application/json"}), timeout=2) as response:
            assert "privacy lifecycle reply" in response.read().decode()
        assistant = {"role": "assistant", "content": [{"type": "text", "text": "privacy lifecycle reply"}],
                     "stopReason": "stop"}
        if MUTATION == "bad-reply":
            assistant["content"] = [{"type": "text", "text": "incorrect"}]
        messages.append(assistant)
        session.write_text(json.dumps(messages))
    else:
        raise AssertionError(kind)
    response = {"type": "response", "id": frame["id"], "success": True}
    if data is not None:
        response["data"] = data
    emit(response)
    if kind == "prompt":
        emit({"type": "agent_end", "messages": [assistant]})
if MUTATION == "close":
    (agent / "telemetry.jsonl").write_text("created during close")
if MUTATION == "close-error":
    print("fixture close error", file=sys.stderr)
    sys.exit(4)
'''


class PrivacyLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privacy-verifier-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def binary(self, mutation="clean"):
        binary = self.root / "installed-prime-agent"
        binary.write_text("#!" + sys.executable + "\n" + FIXTURE.replace("__MUTATION__", repr(mutation)))
        binary.chmod(0o700)
        return binary

    def test_clean_fixture_runs_two_processes_and_restores_conversation(self):
        with patch.dict(os.environ, {
            "AMBIENT_SECRET_API_KEY": "must-not-leak", "DO_NOT_TRACK": "1",
            "PRIME_AGENT_TELEMETRY": "0", "PI_OFFLINE": "1",
            "RLM_SESSION_DIR": "/ambient", "PRIME_AGENT_DAEMON_SOCKET": "/ambient.sock",
            "PRIME_AGENT_FAUX_SCRIPT": "must-not-leak",
        }):
            result = privacy.run_lifecycle(self.binary(), timeout=10)
        self.assertIn("zero reporting", result)

    def test_reporter_injection_mutants_fail(self):
        for mutation, error in (
            ("telemetry", "reporter state created: agent/telemetry.json"),
            ("mirror", "reporter state created: agent/telemetry.jsonl"),
            ("close", "reporter state created: agent/telemetry.jsonl"),
            ("sentinel", "trace outbox changed"),
            ("outbox", "trace outbox changed"),
            ("collector", "outbound collector requests"),
        ):
            with self.subTest(mutation=mutation):
                with self.assertRaisesRegex(AssertionError, error):
                    privacy.run_lifecycle(self.binary(mutation), timeout=10)

    def test_protocol_false_positives_fail(self):
        for mutation, error in (
            ("bad-resume", "resume did not restore"),
            ("bad-reply", "did not finish with fixture reply"),
            ("close-error", "RPC EOF close returned 4"),
        ):
            with self.subTest(mutation=mutation):
                with self.assertRaisesRegex(AssertionError, error):
                    privacy.run_lifecycle(self.binary(mutation), timeout=10)

    def test_hard_deadline_kills_and_reaps_child(self):
        children = []
        original = subprocess.Popen

        def capture(*args, **kwargs):
            child = original(*args, **kwargs)
            children.append(child)
            return child

        start = time.monotonic()
        with patch.object(privacy.subprocess, "Popen", side_effect=capture):
            with self.assertRaisesRegex(AssertionError, "timed out|deadline"):
                privacy.run_lifecycle(self.binary("timeout"), timeout=0.5)
        self.assertLess(time.monotonic() - start, 3)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode)

    def test_proxy_captures_without_forwarding_connect_and_unknown_post(self):
        with privacy.loopback_server() as server:
            import http.client
            connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=2)
            connection.request("CONNECT", "vendor.invalid:443")
            response = connection.getresponse()
            self.assertEqual(response.status, 502)
            response.read()
            connection.close()
            with urlopen(Request(server.url + "/collector", data=b"{}"), timeout=2) as response:
                self.assertEqual(response.status, 200)
            self.assertEqual(server.collector_requests, [
                ("CONNECT", "vendor.invalid:443"), ("POST", "/collector"),
            ])
        self.assertEqual(server.fileno(), -1)

    def test_invalid_binary_and_timeout_fail_closed(self):
        with self.assertRaisesRegex(AssertionError, "not an executable"):
            privacy.run_lifecycle(self.root / "absent")
        binary = self.binary()
        for timeout in (0, -1, math.inf, math.nan):
            with self.subTest(timeout=timeout), self.assertRaises(ValueError):
                privacy.run_lifecycle(binary, timeout)


class RequestAllowlist(unittest.TestCase):
    """A reporter that phones home is caught; a boot-time catalog fetch is named.

    The allowlist exists because the product fetches three public catalogs while
    its caches are cold. It must never grow into "any request is fine": a direct
    request, a non-CONNECT method, or an unknown host still fails.
    """

    class Fake:
        def __init__(self, requests):
            self.collector_requests = requests

    def test_allowlisted_catalog_connects_are_tolerated(self):
        server = self.Fake([
            ("CONNECT", "raw.githubusercontent.com:443"),
            ("CONNECT", "openrouter.ai:443"),
            ("CONNECT", "api.pinference.ai:443"),
        ])
        self.assertEqual(privacy.unexpected_requests(server), [])

    def test_any_other_host_method_or_path_fails(self):
        server = self.Fake([
            ("CONNECT", "raw.githubusercontent.com:443"),
            ("CONNECT", "collector.invalid:443"),
            ("POST", "/api/v1/traces"),
            ("GET", "https://raw.githubusercontent.com/catalog.json"),
        ])
        self.assertEqual(privacy.unexpected_requests(server), [
            ("CONNECT", "collector.invalid:443"),
            ("POST", "/api/v1/traces"),
            ("GET", "https://raw.githubusercontent.com/catalog.json"),
        ])


if __name__ == "__main__":
    unittest.main()
