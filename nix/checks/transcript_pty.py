#!/usr/bin/env python3
"""Installed Prime Agent native-scrollback regression (Linux, stdlib only).

Usage: python3 nix/checks/transcript_pty.py /nix/store/.../bin/prime-agent
No live daemon, provider, credentials or host session files are used. A local
JSONL peer injects deterministic restore/reconnect/branch faults. Every assertion
about native history inspects bytes emitted by the installed CLI on a REAL PTY,
strictly between alternate-screen leave and reentry, not a headless render seam.
Logs remain in a private scratch directory on success and failure.
"""
import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import selectors
import signal
import socket
import struct
import sys
import tempfile
import termios
import time
import traceback

SCHEMA = "protocol-7-schema-30-8e4b17c2a9f5"
COUNT = 1205
OMITTED = 1200
CSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
OSC = re.compile(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
TOKEN = re.compile(rb"(?:restore|selected)-\d{4}-end")


def plain(data):
    return CSI.sub(b"", OSC.sub(b"", bytes(data)))


def history(prefix):
    return [{"role": "user", "content": f"{prefix}-{i:04}-end", "timestamp": i + 1}
            for i in range(COUNT)]


def check_history(data, prefix):
    # Main-screen wire bytes are the actual native scrollback, not viewport rows.
    tokens = TOKEN.findall(plain(data))
    expected = [f"{prefix}-{i:04}-end".encode() for i in range(COUNT)]
    assert tokens == expected, (
        f"{prefix}: expected {COUNT} ordered unique native messages, got {len(tokens)}; "
        f"first={tokens[:3]!r}, last={tokens[-3:]!r}")
    assert b"\x1b[3J" not in data, "native scrollback was erased"


class TerminalBytes:
    """Split the real byte stream at terminal mode changes; retain all raw bytes."""
    def __init__(self):
        self.raw = bytearray()
        self.pending = bytearray()
        self.alt = False
        self.main = bytearray()
        self.replays = []
        self.enters = 0
        self.leaves = 0
        self.segment = bytearray()

    def feed(self, chunk):
        self.raw.extend(chunk)
        self.pending.extend(chunk)
        replies = bytearray()
        while self.pending:
            if self.pending[0] != 27:
                end = self.pending.find(27)
                if end < 0:
                    end = len(self.pending)
                text = self.pending[:end]
                if not self.alt:
                    self.main.extend(text)
                    self.segment.extend(text)
                del self.pending[:end]
                continue
            if len(self.pending) < 2:
                break
            if self.pending[1] == ord('['):
                match = CSI.match(self.pending)
                if match is None:
                    break
                seq = bytes(match[0])
            elif self.pending[1] == ord(']'):
                match = OSC.match(self.pending)
                if match is None:
                    break
                seq = bytes(match[0])
            else:
                seq = bytes(self.pending[:2])
            del self.pending[:len(seq)]
            if seq == b"\x1b[?1049h":
                if self.segment:
                    self.replays.append(bytes(self.segment))
                    self.segment.clear()
                self.alt = True
                self.enters += 1
            elif seq == b"\x1b[?1049l":
                self.alt = False
                self.leaves += 1
            else:
                if not self.alt:
                    self.main.extend(seq)
                    self.segment.extend(seq)
                if seq == b"\x1b[6n":
                    replies.extend(b"\x1b[1;1R")
                elif seq == b"\x1b[?u":
                    replies.extend(b"\x1b[?0u")
                elif seq in (b"\x1b[c", b"\x1b[0c"):
                    replies.extend(b"\x1b[?1;2c")
        return replies


class Probe:
    def __init__(self, args, scratch):
        self.args, self.root = args, scratch
        self.deadline = time.monotonic() + args.timeout
        self.sel = selectors.DefaultSelector()
        self.term = TerminalBytes()
        self.clients = {}
        self.requests = []
        self.active = None
        self.selected = False
        self.reject_selected = True
        self.generation = "g1"
        self.pid = None
        self.status = None
        self.master = None
        self.sockpath = scratch / "d.sock"
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(self.sockpath))
        self.listener.listen(16)
        self.listener.setblocking(False)
        self.sel.register(self.listener, selectors.EVENT_READ, "listen")
        self.log = (scratch / "protocol.jsonl").open("w", buffering=1)
        self.rawlog = (scratch / "terminal.bin").open("wb", buffering=0)

    def send(self, client, data):
        self.log.write(json.dumps({"out": data}) + "\n")
        self.clients[client][1].extend(json.dumps(data).encode() + b"\n")
        self.sel.modify(client, selectors.EVENT_READ | selectors.EVENT_WRITE, "client")

    def close_client(self, client):
        self.sel.unregister(client)
        client.close()
        del self.clients[client]
        if self.active is client:
            self.active = None

    def state(self):
        return {"sessionId": "saved", "activeSessionId": "active",
                "leafId": "selected-leaf" if self.selected else "old-leaf",
                "compactionCount": 1, "cwd": str(self.root / "cwd"),
                "sessionFile": str(self.fixture), "isStreaming": False,
                "isCompacting": False}

    def command(self, client, envelope):
        command = envelope["command"]
        kind = command["type"]
        self.requests.append(command)
        self.log.write(json.dumps({"in": envelope}) + "\n")
        state = self.state()
        messages = history("selected" if self.selected else "restore")
        data = {}
        success = True
        if kind == "create":
            data = {"id": "active", "activeSessionId": "active", "sessionId": "saved",
                    "cwd": str(self.root / "cwd")}
        elif kind == "attach":
            self.active = client
            caps = command.get("capabilities", [])
            self.attach_caps = caps
            snapshot = {"activeSessionId": "active", "state": state,
                        "summary": {"id": "active", "cwd": state["cwd"]},
                        "messages": messages[OMITTED:], "historyBefore": OMITTED,
                        "displayTranscript": True, "lastEventSequence": 0,
                        "lastEventCursor": {"generation": self.generation, "sequence": 0}}
            data = {"activeSessionId": "active", "snapshot": snapshot,
                    "replay": {"status": "complete", "toSequence": 0}}
        elif kind == "get_connection_state":
            data = state
        elif kind == "get_messages":
            if self.selected and self.reject_selected and "before" not in command:
                self.reject_selected = False
                success = False
            else:
                data = {"messages": messages[:command["before"]] if "before" in command else messages,
                        "displayTranscript": True, "sessionId": "saved",
                        "leafId": state["leafId"], "generation": self.generation}
                if "historyLeafId" in command:
                    data["historyLeafId"] = command["historyLeafId"]
        elif kind == "get_session_context":
            data = {"context": {"messages": messages[-1:]}}
        elif kind == "get_session_tree":
            nodes = []
            for i, (entry_id, parent, content) in enumerate([
                    ("root", None, "shared root"), ("old-leaf", "root", "old branch"),
                    ("selected-leaf", "root", "selected destination")]):
                nodes.append({"entry": {"type": "message", "id": entry_id,
                    "parentId": parent, "timestamp": f"2026-01-01T00:00:0{i}Z",
                    "message": {"role": "user", "content": content, "timestamp": i + 1}}})
            data = {"leafId": state["leafId"], "flatNodes": nodes}
        elif kind == "navigate_tree":
            assert command["targetId"] == "selected-leaf", command
            self.selected = True
            data = {"editorText": ""}
        elif kind == "get_session_stats":
            data = {"sessionId": "saved", "sessionFile": str(self.fixture),
                    "contextUsage": {"tokens": 1, "contextWindow": 200000}}
        elif kind == "get_rlm_children":
            data = {"children": [], "eventSequence": 0}
        elif kind == "list_heartbeats":
            data = {"heartbeats": []}
        elif kind == "get_commands":
            data = {"commands": []}
        elif kind == "reload":
            data = {"message": "Reloaded keybindings, skills, prompts, themes"}
        response = {"type": "response", "id": envelope["id"], "command": kind,
                    "success": success, "data": data}
        if not success:
            response["error"] = "injected selected history failure"
        self.send(client, response)

    def tick(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("hard probe deadline exceeded")
        for key, mask in self.sel.select(remaining):
            obj = key.fileobj
            if key.data == "listen":
                client, _ = self.listener.accept()
                client.setblocking(False)
                self.clients[client] = [bytearray(), bytearray()]
                self.sel.register(client, selectors.EVENT_READ, "client")
                self.send(client, {"type": "daemon_hello", "socketPath": str(self.sockpath),
                    "protocol": {"name": "prime-agent.daemon", "version": 7},
                    "schemaId": self.args.schema_id, "appVersion": "0.10.0",
                    "clientId": "pty-probe", "serverCapabilities": ["display_transcript"]})
            elif key.data == "pty":
                try:
                    chunk = os.read(self.master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b""
                if not chunk:
                    self.sel.unregister(self.master)
                    _, self.status = os.waitpid(self.pid, 0)
                    continue
                self.rawlog.write(chunk)
                replies = self.term.feed(chunk)
                if replies:
                    os.write(self.master, replies)
            else:
                if mask & selectors.EVENT_WRITE:
                    output = self.clients[obj][1]
                    if output:
                        del output[:obj.send(output)]
                    if not output:
                        self.sel.modify(obj, selectors.EVENT_READ, "client")
                if mask & selectors.EVENT_READ:
                    chunk = obj.recv(65536)
                    if not chunk:
                        self.close_client(obj)
                        continue
                    incoming = self.clients[obj][0]
                    incoming.extend(chunk)
                    while b"\n" in incoming:
                        line, _, rest = incoming.partition(b"\n")
                        incoming[:] = rest
                        if line:
                            self.command(obj, json.loads(line))

    def wait(self, predicate, label):
        self.phase = label
        print(f"WAIT {label}", flush=True)
        while not predicate():
            if self.status is not None:
                raise AssertionError(f"CLI exited early ({self.status}) during {label}")
            self.tick()
        print(f"OK   {label}", flush=True)

    def text_since(self, offset, needle):
        return needle.encode() in plain(self.term.raw[offset:])

    def write(self, text):
        os.write(self.master, text)

    def replay_count(self, prefix):
        return sum(f"{prefix}-0000-end".encode() in plain(s) for s in self.term.replays)

    def launch(self):
        self.fixture = self.root / "saved.jsonl"
        self.fixture.write_text(json.dumps({"type": "session", "version": 3, "id": "saved",
            "timestamp": "2026-01-01T00:00:00Z", "cwd": str(self.root / "cwd")}) + "\n")
        self.fixture_hash = hashlib.sha256(self.fixture.read_bytes()).hexdigest()
        env = isolated_env(self.root)
        agent = Path(env["PRIME_AGENT_CODING_AGENT_DIR"])
        (agent / "settings.json").write_text(json.dumps({"onboardingShown": True,
            "telemetryEnabled": False, "agentTraces": {"enabled": False},
            "branchSummary": {"skipPrompt": True}}))
        pid, master = pty.fork()
        if pid == 0:
            os.chdir(self.root / "cwd")
            os.execve(self.args.binary, [self.args.binary, "--daemon-socket", str(self.sockpath),
                "--resume", str(self.fixture), "--provider", "openai", "--model", "gpt-4o",
                "--api-key", "fixture-not-a-credential"], env)
        self.pid, self.master = pid, master
        os.set_blocking(master, False)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        self.sel.register(master, selectors.EVENT_READ, "pty")

    def run(self):
        self.launch()
        self.wait(lambda: self.replay_count("restore") == 1, "1205-message restore native replay + alt reentry")
        assert "display_transcript" in self.attach_caps, self.attach_caps
        assert any(r.get("before") == OMITTED for r in self.requests), "backfill boundary not exercised"
        check_history(next(s for s in self.term.replays if b"restore-0000-end" in plain(s)), "restore")
        offset = len(self.term.raw)
        self.write(b"\x0f")  # Ctrl+O: repaint detail, not restore.
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", 25, 101, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)
        self.write(b"/reload\r")
        self.wait(lambda: self.text_since(offset, "Reloaded keybindings"), "redraw and ordinary reload")
        assert self.replay_count("restore") == 1, "ordinary redraw duplicated native history"
        # Force transport loss, but keep our bound listener. No free-port race.
        self.generation = "g2"
        assert self.active is not None
        self.close_client(self.active)
        self.wait(lambda: self.replay_count("restore") == 2, "reconnect authoritative native replay")
        restore = [s for s in self.term.replays if b"restore-0000-end" in plain(s)]
        check_history(restore[1], "restore")
        offset = len(self.term.raw)
        self.write(b"/tree\r")
        self.wait(lambda: self.text_since(offset, "Session Tree"), "tree picker")
        offset = len(self.term.raw)
        self.write(b"selected")
        # Wait for visible filter handling rather than sending Enter in the same paste burst.
        self.wait(lambda: self.text_since(offset, "selected"), "selected branch filter")
        offset = len(self.term.raw)
        self.write(b"\r")
        self.wait(lambda: self.selected, "branch navigate request")
        self.wait(lambda: self.text_since(offset, "Full session history unavailable"), "injected branch failure disclosed")
        assert self.text_since(offset, "selected-1204-end"), "selected context fallback not rendered"
        assert self.replay_count("selected") == 0, "incomplete branch replayed as complete history"
        offset = len(self.term.raw)
        self.write(b"/reload\r")
        self.wait(lambda: self.replay_count("selected") == 1, "selected history recovery + native replay")
        check_history(next(s for s in self.term.replays if b"selected-0000-end" in plain(s)), "selected")
        assert b"\x1b[3J" not in self.term.raw, "native scrollback erase"
        assert hashlib.sha256(self.fixture.read_bytes()).hexdigest() == self.fixture_hash, "fixture changed"
        self.write(b"\x04")
        self.wait(lambda: self.status is not None, "CLI clean exit")
        assert os.waitstatus_to_exitcode(self.status) == 0, self.status
        assert not self.term.alt, "alternate screen not restored on exit"
        (self.root / "result.json").write_text(json.dumps({"binary": self.args.binary,
            "sha256": hashlib.sha256(Path(self.args.binary).read_bytes()).hexdigest(),
            "schemaId": self.args.schema_id, "messages": COUNT,
            "restoreReplays": len(restore), "selectedReplays": self.replay_count("selected"),
            "fixtureUnchanged": True, "exitCode": 0}, indent=2) + "\n")

    def close(self):
        if self.pid is not None:
            try:
                os.killpg(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            if self.status is None:
                try:
                    os.waitpid(self.pid, 0)
                except ChildProcessError:
                    pass
        for client in list(self.clients):
            self.close_client(client)
        self.listener.close()
        if self.master is not None:
            os.close(self.master)
        self.sel.close()
        self.log.close()
        self.rawlog.close()
        self.sockpath.unlink(missing_ok=True)


def isolated_env(root):
    # Construct, never filter inherited env: no credentials, PRIME_AGENT_*, RLM_*,
    # PI_*, proxy, SSH agent, project hooks, user config, Python path or loader env.
    paths = {name: root / name for name in ["home", "config", "cache", "data", "state",
        "runtime", "tmp", "agent", "sessions", "kernel-venv", "harness", "global-harness", "cwd"]}
    for path in paths.values():
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
    return {"PATH": "/run/current-system/sw/bin:/usr/bin:/bin", "TERM": "xterm-256color",
        "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TZ": "UTC", "DO_NOT_TRACK": "1",
        "HOME": str(paths["home"]), "XDG_CONFIG_HOME": str(paths["config"]),
        "XDG_CACHE_HOME": str(paths["cache"]), "XDG_DATA_HOME": str(paths["data"]),
        "XDG_STATE_HOME": str(paths["state"]), "XDG_RUNTIME_DIR": str(paths["runtime"]),
        "TMPDIR": str(paths["tmp"]), "PRIME_AGENT_CODING_AGENT_DIR": str(paths["agent"]),
        "PRIME_AGENT_AGENT_DIR": str(paths["agent"]), "PI_CODING_AGENT_DIR": str(paths["agent"]),
        "PRIME_AGENT_SESSION_DIR": str(paths["sessions"]),
        "PRIME_AGENT_CODING_AGENT_SESSION_DIR": str(paths["sessions"]),
        "PRIME_AGENT_KERNEL_VENV": str(paths["kernel-venv"]),
        "RLM_SESSION_DIR": str(paths["sessions"]), "RLM_HARNESS_STATE_DIR": str(paths["harness"]),
        "RLM_GLOBAL_HARNESS_STATE_DIR": str(paths["global-harness"]),
        "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_REGISTRY_DIR": str(paths["runtime"]),
        "PRIME_AGENT_DAEMON_SOCKET": str(root / "d.sock")}


def self_test():
    data = b"\x1b[?1049hviewport\x1b[?1049l" + b"\n".join(
        f"restore-{i:04}-end".encode() for i in range(COUNT)) + b"\x1b[?1049h"
    terminal = TerminalBytes()
    for byte in data:
        terminal.feed(bytes([byte]))
    assert terminal.alt and terminal.enters == 2 and terminal.leaves == 1
    assert len(terminal.replays) == 1
    check_history(terminal.replays[0], "restore")
    for mutant in [terminal.replays[0] + b"restore-0000-end", terminal.replays[0][30:],
                   terminal.replays[0] + b"\x1b[3J"]:
        try:
            check_history(mutant, "restore")
        except AssertionError:
            continue
        raise AssertionError("oracle accepted missing/duplicate/erased history")
    env = isolated_env(Path(tempfile.mkdtemp(prefix="env-", dir=SCRATCH)))
    assert "OPENAI_API_KEY" not in env and "PYTHONPATH" not in env
    print("PASS fixture parser, chunk-boundary handling, negative mutants, isolated environment")


def alarm(_signum, _frame):
    raise TimeoutError("hard wall-clock deadline exceeded")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", help="absolute path to installed Prime Agent executable")
    parser.add_argument("--timeout", type=int, default=90, help="whole-run hard deadline in seconds")
    parser.add_argument("--schema-id", default=SCHEMA, help="installed daemon schema ID (hello compatibility)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("--timeout must be positive")
    os.umask(0o077)
    global SCRATCH
    base = Path.home() / ".cache" / "agent-scratch" / "prime-rust-migration"
    base.mkdir(mode=0o700, parents=True, exist_ok=True)
    SCRATCH = Path(tempfile.mkdtemp(prefix="pty-", dir=base))
    print(f"Diagnostics: {SCRATCH}", flush=True)
    signal.signal(signal.SIGALRM, alarm)
    signal.alarm(args.timeout)
    if args.self_test:
        self_test()
        return 0
    if not args.binary or not Path(args.binary).is_absolute() or not os.access(args.binary, os.X_OK):
        parser.error("an absolute installed executable path is required")
    probe = Probe(args, SCRATCH)
    try:
        probe.run()
        print("PASS installed CLI native-scrollback PTY regression", flush=True)
        return 0
    except BaseException:
        diagnostic = traceback.format_exc()
        (SCRATCH / "failure.txt").write_text(f"phase: {getattr(probe, 'phase', 'launch')}\n{diagnostic}")
        print(diagnostic, file=sys.stderr, flush=True)
        return 1
    finally:
        signal.alarm(0)
        probe.close()


if __name__ == "__main__":
    sys.exit(main())
