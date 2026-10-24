"""Verify an installed Nix wrapper with a fresh, isolated real Python kernel.

Run outside a Nix build sandbox: uv may download the pinned runtime dependencies.
The faux provider uses no credentials. Keep state and logs for diagnosis.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--state-root", required=True, type=Path)
    parser.add_argument("--skill-root", type=Path)
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args()
    binary = args.binary.absolute()
    root = args.state_root.absolute()
    root.mkdir(mode=0o700)  # Never replace an earlier probe or user state.
    for name in ("home", "tmp", "agent", "sessions", "artifacts", "work", "run"):
        (root / name).mkdir(mode=0o700)
    (root / "agent/settings.json").write_text(json.dumps({
        "prewarmPythonKernel": False,
        "autoRefine": {"enabled": False},
        "customPreserved": {"probe": True},
    }))
    skills = ["edit"]
    if args.skill_root:
        source = args.skill_root.resolve(strict=True)
        (root / "agent/skills").mkdir()
        (root / "agent/skills/ollama-web").symlink_to(source, target_is_directory=True)
        skills.append("ollama_web")
    receipt = root / "kernel-receipt.json"
    cell = (
        "import importlib, json, sys\nfrom rlm import rlm as _r\n"
        f"modules = {skills!r}\n"
        "loaded = {name: importlib.import_module(name).__file__ for name in modules}\n"
        "payload = {'kernel_boot': True, 'spawn': callable(_r.spawn), "
        "'python': sys.executable, 'skills': loaded}\n"
        f"open({str(receipt)!r}, 'w').write(json.dumps(payload))\n"
        "print('KERNEL_BOOT_OK')"
    )
    script = {"responses": [
        {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": cell}}]},
        {"text": "kernel boot verified"},
    ]}
    env = {
        "PATH": os.environ["PATH"], "HOME": str(root / "home"), "TMPDIR": str(root / "tmp"),
        "XDG_CONFIG_HOME": str(root / "config"), "XDG_CACHE_HOME": str(root / "cache"),
        "XDG_DATA_HOME": str(root / "data"), "XDG_RUNTIME_DIR": str(root / "run"),
        "PRIME_AGENT_CODING_AGENT_DIR": str(root / "agent"),
        "PRIME_AGENT_SESSION_DIR": str(root / "sessions"),
        "PRIME_AGENT_KERNEL_VENV": str(root / "kernel"),
        "PRIME_AGENT_DAEMON_SOCKET": str(root / "run/daemon.sock"),
        "RLM_SESSION_DIR": str(root / "artifacts"),
        "RLM_HARNESS_STATE_DIR": str(root / "harness-local"),
        "RLM_GLOBAL_HARNESS_STATE_DIR": str(root / "harness-global"),
        "UV_CACHE_DIR": str(root / "uv-cache"),
        "UV_PYTHON_INSTALL_DIR": str(root / "uv-python"),
        "PI_OFFLINE": "1", "PI_SKIP_VERSION_CHECK": "1", "DO_NOT_TRACK": "1",
        "PRIME_AGENT_FAUX_SCRIPT": json.dumps(script),
    }
    # Trust configuration only, never provider credentials or ambient agent paths.
    for name in ("SSL_CERT_FILE", "NIX_SSL_CERT_FILE"):
        if name in os.environ:
            env[name] = os.environ[name]
    command = [str(binary), "--mode", "json", "-p", "use the kernel", "--no-session"]
    with (root / "probe.log").open("w") as log:
        process = subprocess.Popen(command, cwd=root / "work", env=env,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=args.timeout)
        except BaseException:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise
    assert code == 0, f"installed turn exited {code}; see {root / 'probe.log'}"
    payload = json.loads(receipt.read_text())
    assert payload["kernel_boot"] is True and payload["spawn"] is True, payload
    assert Path(payload["python"]) == root / "kernel/bin/python", payload
    assert set(payload["skills"]) == set(skills), payload
    assert all(payload["skills"].values()), payload
    settings = json.loads((root / "agent/settings.json").read_text())
    assert settings["customPreserved"] == {"probe": True}, settings
    auth = root / "agent/auth.json"
    assert not auth.exists() or json.loads(auth.read_text()) == {}, "unexpected probe credentials"
    print(json.dumps({"binary": str(binary), "state_root": str(root), "receipt": payload}))


if __name__ == "__main__":
    main()
