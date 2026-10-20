"""Installed Rust privacy gate: real RPC create/turn/EOF/resume/turn/EOF.

Run with an explicit installed prime-agent executable. The loopback provider uses
real OpenAI-compatible HTTP, not PRIME_AGENT_FAUX_SCRIPT (which bypasses reporting
wiring). Legacy opt-ins and dummy trace credentials deliberately remain enabled.
Only stdlib is needed. No user daemon, credentials, or kernel venv are used.
"""

import argparse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile
import threading
import time


PROVIDER = "privacy-fixture"
MODEL = "privacy-test"
REPLY = "privacy lifecycle reply"
FIRST_PROMPT = "privacy lifecycle first turn"
SECOND_PROMPT = "privacy lifecycle resumed turn"
# Grounded in ContinuousTraceUpload's one-second debounce. This is a bounded
# negative-network assertion, not a startup/readiness sleep or retry loop.
REPORTER_OBSERVATION_SECONDS = 1.25


def remaining(deadline):
    seconds = deadline - time.monotonic()
    if seconds <= 0:
        raise AssertionError("privacy lifecycle hard deadline exceeded")
    return seconds


class LoopbackServer(ThreadingHTTPServer):
    # Join handlers on close before checking requests. Otherwise a queued
    # upload could be counted just after the privacy assertion.
    daemon_threads = False
    block_on_close = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), FixtureHandler)
        self.provider_requests = []
        self.collector_requests = []
        self.collector_event = threading.Event()
        self.errors = []

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_port}"


class FixtureHandler(BaseHTTPRequestHandler):
    def setup(self):
        super().setup()
        self.connection.settimeout(2)

    def log_message(self, *_args):
        pass

    def handle_request(self):
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 <= length <= 16 * 1024 * 1024:
                raise ValueError("fixture request body too large")
            body = self.rfile.read(length)
            if self.command == "POST" and self.path == "/provider/v1/chat/completions":
                request = json.loads(body)
                self.server.provider_requests.append(request)
                if request.get("model") != MODEL or request.get("stream") is not True:
                    raise ValueError("expected streaming fixture model")
                chunks = [
                    {"id": "privacy", "object": "chat.completion.chunk", "model": MODEL,
                     "choices": [{"index": 0, "delta": {"role": "assistant", "content": REPLY},
                                  "finish_reason": None}]},
                    {"id": "privacy", "object": "chat.completion.chunk", "model": MODEL,
                     "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                     "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}},
                ]
                payload = ("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks)
                           + "data: [DONE]\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
            else:
                # Includes HTTPS CONNECT through the poison proxy: never forward
                # vendor traffic to the internet. A request itself fails the gate.
                self.server.collector_requests.append((self.command, self.path))
                self.server.collector_event.set()
                payload = b'{"sessionId":"fixture","traceId":"fixture","bytesStored":1}'
                self.send_response(502 if self.command == "CONNECT" else 200)
                self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            self.server.errors.append(str(error))
            self.send_error(400)

    do_POST = handle_request
    do_GET = handle_request
    do_PUT = handle_request
    do_CONNECT = handle_request
    do_DELETE = handle_request
    do_HEAD = handle_request
    do_OPTIONS = handle_request
    do_PATCH = handle_request


@contextmanager
def loopback_server():
    server = LoopbackServer()  # Already bound/listening; no readiness sleep.
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
        if thread.is_alive():
            raise AssertionError("loopback server did not close")


def isolated_environment(home, collector_url):
    """Allowlist, never copy ambient API keys, proxy auth, harness or package overrides."""
    env = {
        "PATH": os.environ.get("PATH", os.defpath),
        "HOME": str(home), "USER": "privacy-fixture", "LOGNAME": "privacy-fixture",
        "TMPDIR": str(home / "tmp"), "TMP": str(home / "tmp"), "TEMP": str(home / "tmp"),
        "XDG_CONFIG_HOME": str(home / "config"), "XDG_CACHE_HOME": str(home / "cache"),
        "XDG_DATA_HOME": str(home / "data"), "XDG_STATE_HOME": str(home / "state"),
        "XDG_RUNTIME_DIR": str(home / "run"),
        "PRIME_AGENT_CODING_AGENT_DIR": str(home / "agent"),
        "PRIME_AGENT_SESSION_DIR": str(home / "sessions"),
        "PRIME_AGENT_CODING_AGENT_SESSION_DIR": str(home / "sessions"),
        "PRIME_AGENT_DAEMON_SOCKET": str(home / "run" / "daemon.sock"),
        "PRIME_AGENT_KERNEL_VENV": str(home / "kernel-never-created"),
        # The real RPC engine prewarms by default. An explicit missing override
        # rejects before auto-bootstrap, without masking reporter installation.
        "PRIME_AGENT_KERNEL_PYTHON": str(home / "no-kernel-python"),
        "RLM_GLOBAL_HARNESS_STATE_DIR": str(home / "global-harness"),
        "RLM_HARNESS_STATE_DIR": str(home / "local-harness"),
        "RLM_SESSION_DIR": str(home / "artifacts"),
        "PRIME_AGENT_TRACES_API_KEY": "dummy-trace-key-not-a-credential",
        "PRIME_AGENT_TRACES_BASE_URL": collector_url,
        "PI_SKIP_VERSION_CHECK": "1", "UV_OFFLINE": "1",
        "GIT_CONFIG_GLOBAL": str(home / "gitconfig"), "GIT_CONFIG_NOSYSTEM": "1",
        "LANG": "C.UTF-8",
    }
    # Do not set DO_NOT_TRACK, PI_OFFLINE or PRIME_AGENT_TELEMETRY: those would
    # turn the unpatched reporters off and make the test vacuous.
    for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
        env[key] = collector_url
    for key in ("NO_PROXY", "no_proxy"):
        env[key] = "127.0.0.1,localhost,::1"
    for name in ("tmp", "config", "cache", "data", "state", "run", "agent", "sessions", "work"):
        (home / name).mkdir(mode=0o700)
    # Stop project/skill discovery at our empty fixture, even when TMPDIR is
    # nested below a developer's checkout or home directory.
    (home / "work" / ".git").mkdir(mode=0o700)
    return env


class RpcProcess:
    """Bounded JSONL transport; stderr goes to a file so neither pipe can deadlock."""

    def __init__(self, binary, home, env, deadline, session=None):
        self.deadline = deadline
        self.stderr = tempfile.TemporaryFile(dir=home / "tmp")
        self.selector = selectors.DefaultSelector()
        self.buffer = b""
        self.frames = []
        self.next_id = 0
        args = [str(binary), "--mode", "rpc", "--provider", PROVIDER, "--model", MODEL]
        if session is not None:
            args.extend(["--resume", str(session)])
        try:
            self.process = subprocess.Popen(
                args, cwd=home / "work", env=env, stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=self.stderr, start_new_session=True,
            )
            self.selector.register(self.process.stdout, selectors.EVENT_READ)
        except BaseException:
            self.selector.close()
            self.stderr.close()
            raise

    def diagnostics(self):
        self.stderr.seek(0)
        return self.stderr.read(32768).decode(errors="replace")

    def read_frame(self):
        while b"\n" not in self.buffer:
            if not self.selector.select(remaining(self.deadline)):
                raise AssertionError("RPC timed out waiting for a frame")
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise AssertionError("RPC stdout closed before expected frame")
            self.buffer += chunk
            if len(self.buffer) > 4 * 1024 * 1024:
                raise AssertionError("RPC frame exceeded 4 MiB")
        line, self.buffer = self.buffer.split(b"\n", 1)
        frame = json.loads(line)
        if not isinstance(frame, dict):
            raise AssertionError(f"RPC frame is not an object: {frame!r}")
        return frame

    def request(self, command, **fields):
        self.next_id += 1
        identifier = f"privacy-{self.next_id}"
        self.process.stdin.write((json.dumps({"type": command, "id": identifier, **fields}) + "\n").encode())
        self.process.stdin.flush()
        while True:
            frame = self.read_frame()
            if frame.get("type") == "response" and frame.get("id") == identifier:
                if frame.get("success") is not True:
                    raise AssertionError(f"RPC {command} failed: {frame}")
                return frame.get("data", {})
            self.frames.append(frame)

    def turn(self, message):
        self.request("prompt", message=message)
        while True:
            if self.frames:
                frame = self.frames.pop(0)
            else:
                frame = self.read_frame()
            if frame.get("type") == "agent_end":
                replies = [entry for entry in frame.get("messages", []) if entry.get("role") == "assistant"]
                texts = [part.get("text") for entry in replies for part in entry.get("content", [])
                         if part.get("type") == "text"]
                if REPLY not in texts or any(entry.get("stopReason") in ("error", "aborted") for entry in replies):
                    raise AssertionError(f"RPC turn did not finish with fixture reply: {frame}")
                return

    def close(self):
        self.process.stdin.close()  # EOF is the real RPC session-close contract.
        # Keep draining stdout during teardown, which may publish final events.
        while self.selector.get_map():
            if not self.selector.select(remaining(self.deadline)):
                raise AssertionError("RPC EOF close timed out")
            if not os.read(self.process.stdout.fileno(), 65536):
                self.selector.unregister(self.process.stdout)
        code = self.process.wait(timeout=remaining(self.deadline))
        if code != 0:
            raise AssertionError(f"RPC EOF close returned {code}")

    def dispose(self):
        # Also stop orphan descendants in this test-owned process group. Never
        # address the ambient user's daemon, kernel or process group.
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.process.wait(timeout=5)
        for stream in (self.process.stdin, self.process.stdout):
            stream.close()
        self.selector.close()
        self.stderr.close()


@contextmanager
def rpc_process(binary, home, env, deadline, session=None):
    rpc = RpcProcess(binary, home, env, deadline, session)
    try:
        yield rpc
    except Exception as error:
        raise AssertionError(f"{error}\nRPC stderr:\n{rpc.diagnostics()}") from error
    finally:
        rpc.dispose()


# The public catalog and provider-discovery hosts a session boot reads while its
# caches are cold. The port (and the TypeScript line before it) fetches them at
# boot by design; six daemon tests pin that behaviour. They are allowlisted ONLY
# as HTTPS CONNECT through the poison proxy - a direct request, any other host, or
# any non-CONNECT method still fails, so a reporter that phones home is caught
# whether or not it honours the proxy variables.
CATALOG_CONNECT_ALLOWLIST = frozenset({
    "raw.githubusercontent.com:443",
    "openrouter.ai:443",
    "api.pinference.ai:443",
})


def unexpected_requests(server):
    unexpected = []
    for method, target in server.collector_requests:
        if method == "CONNECT" and target in CATALOG_CONNECT_ALLOWLIST:
            continue
        unexpected.append((method, target))
    return unexpected


def observe_no_reporting(server, deadline):
    if server.collector_event.wait(min(REPORTER_OBSERVATION_SECONDS, remaining(deadline))):
        unexpected = unexpected_requests(server)
        if unexpected:
            raise AssertionError(f"outbound collector requests: {unexpected}")
    remaining(deadline)


def outbox_snapshot(home):
    result = {}
    for path in sorted(home.rglob("*")):
        if any("outbox" in part for part in path.relative_to(home).parts):
            stat = path.lstat()
            result[str(path.relative_to(home))] = (
                stat.st_mode, stat.st_ino, stat.st_mtime_ns,
                path.read_bytes() if path.is_file() else None,
            )
    return result


def privacy_errors(home, before, server):
    errors = []
    for path in sorted(home.rglob("*")):
        if path.name in ("telemetry.json", "telemetry.jsonl", "telemetry-install-id.json"):
            errors.append(f"reporter state created: {path.relative_to(home)}")
    if outbox_snapshot(home) != before:
        errors.append("trace outbox changed (sentinel mutation or new outbox state)")
    unexpected = unexpected_requests(server)
    if unexpected:
        errors.append(f"outbound collector requests: {unexpected}")
    if server.errors:
        errors.append(f"loopback fixture errors: {server.errors}")
    if (home / "kernel-never-created").exists():
        errors.append("unexpected kernel auto-bootstrap")
    if list(home.rglob("*.sock")):
        errors.append("unexpected daemon/kernel socket")
    return errors


def run_lifecycle(binary, timeout=60):
    # Keep the installed launcher path intact: resolving its symlink could skip
    # wrapper-owned runtime setup. Never discover a binary on the user's PATH.
    binary = Path(os.path.abspath(binary))
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise AssertionError(f"not an executable installed prime-agent path: {binary}")
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be finite and positive")
    deadline = time.monotonic() + timeout
    with tempfile.TemporaryDirectory(prefix="prime-privacy-") as raw, loopback_server() as server:
        home = Path(raw)
        home.chmod(0o700)
        env = isolated_environment(home, server.url)
        agent = home / "agent"
        (agent / "settings.json").write_text(json.dumps({
            "telemetry": {"enabled": True, "localMirror": True, "noticeShown": True},
            "agentTraces": {"enabled": True}, "autoRefine": {"enabled": False},
            "retry": {"enabled": False}, "compaction": {"enabled": False},
        }))
        (agent / "auth.json").write_text(json.dumps({
            "prime-agent-traces": {"type": "api_key", "key": "dummy-stored-trace-key"},
        }))
        (agent / "models.json").write_text(json.dumps({"providers": {PROVIDER: {
            "api": "openai-completions", "baseUrl": server.url + "/provider/v1",
            "apiKey": "dummy-provider-key", "models": [{
                "id": MODEL, "name": "Privacy fixture", "reasoning": False,
                "input": ["text"], "contextWindow": 100000, "maxTokens": 1024,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            }],
        }}}))
        # RPC warms its catalog in the background. Seed the public, validated
        # last-good cache so this test needs no catalog network and does not use
        # PI_OFFLINE (which would also disable the reporters under test).
        models_cache = agent / "models"
        models_cache.mkdir(mode=0o700)
        (models_cache / "provider-model-catalog.v1.json").write_text(json.dumps({
            "url": "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/models/catalog.v1.json",
            "scope": "public", "fetchedAt": int(time.time() * 1000),
            "payload": {"schemaVersion": 1, "models": [{
                "id": "gpt-4", "name": "Cache fixture", "provider": "openai",
                "api": "openai-responses", "baseUrl": "https://api.openai.com/v1",
                "reasoning": False, "input": ["text"], "contextWindow": 8192, "maxTokens": 8192,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            }]},
        }))
        outbox = agent / "agent-traces-outbox"
        outbox.mkdir(mode=0o700)
        (outbox / "sentinel.json").write_text(json.dumps({
            "sessionFile": str(home / "legacy-session.jsonl"), "size": 17, "mtimeMs": 1,
            "sentinel": "must remain byte-for-byte unchanged",
        }) + "\n")
        for path in agent.rglob("*"):
            if path.is_file():
                path.chmod(0o600)
        before = outbox_snapshot(home)
        with rpc_process(binary, home, env, deadline) as rpc:
            initial = rpc.request("get_state")
            session_id = initial["sessionId"]
            if initial.get("messageCount") != 0:
                raise AssertionError("create did not start a fresh session")
            rpc.turn(FIRST_PROMPT)
            stats = rpc.request("get_session_stats")
            if (stats.get("userMessages"), stats.get("assistantMessages")) != (1, 1):
                raise AssertionError(f"create turn not persisted: {stats}")
            session = Path(stats["sessionFile"])
            if not session.is_relative_to(home / "sessions") or not session.is_file():
                raise AssertionError(f"session escaped isolated persistence: {session}")
            observe_no_reporting(server, deadline)
            rpc.close()
        with rpc_process(binary, home, env, deadline, session=session) as rpc:
            resumed = rpc.request("get_state")
            if resumed["sessionId"] != session_id or resumed.get("messageCount", 0) < 2:
                raise AssertionError(f"resume did not restore the same conversation: {resumed}")
            rpc.turn(SECOND_PROMPT)
            stats = rpc.request("get_session_stats")
            if (stats.get("userMessages"), stats.get("assistantMessages")) != (2, 2):
                raise AssertionError(f"resumed turn not persisted: {stats}")
            observe_no_reporting(server, deadline)
            rpc.close()
        if len(server.provider_requests) != 2:
            raise AssertionError(f"expected two real provider calls, got {len(server.provider_requests)}")
        replay = json.dumps(server.provider_requests[1].get("messages", []))
        if not all(text in replay for text in (FIRST_PROMPT, SECOND_PROMPT, REPLY)):
            raise AssertionError("resumed provider call did not contain the persisted conversation")
        # Both agent processes have exited. Drain accepted HTTP handlers before
        # the final snapshot so late teardown uploads cannot evade the gate.
        server.shutdown()
        server.server_close()
        errors = privacy_errors(home, before, server)
        if errors:
            raise AssertionError("privacy lifecycle failed:\n" + "\n".join(errors))
        remaining(deadline)
    return "Installed privacy lifecycle passed: create, turn, close, resume, turn, close; zero reporting"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="explicit installed prime-agent executable")
    parser.add_argument("--timeout", type=float, default=60, help="whole lifecycle deadline in seconds")
    args = parser.parse_args()
    try:
        print(run_lifecycle(args.binary, args.timeout))
    except (AssertionError, OSError, ValueError) as error:
        parser.exit(1, str(error) + "\n")


if __name__ == "__main__":
    main()
