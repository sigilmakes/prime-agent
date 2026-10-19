"""Check the installed public CLI without models, credentials, daemon or kernel."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def smoke(binary: Path, version: str) -> None:
    with tempfile.TemporaryDirectory(prefix="prime-installed-smoke-") as raw:
        home = Path(raw)
        home.chmod(0o700)
        agent = home / "agent"
        agent.mkdir()
        settings = agent / "settings.json"
        initial = {
            "theme": "terminal-native",
            "autoRefine": {"enabled": True},
            "customPreserved": {"nested": [1, 2]},
            "mcpServers": {"owned": {"type": "http", "url": "https://example.invalid/mcp"}},
        }
        settings.write_text(json.dumps(initial))
        env = {
            "HOME": raw, "TMPDIR": raw, "PATH": os.environ["PATH"],
            "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_CACHE_HOME": str(home / "cache"),
            "XDG_DATA_HOME": str(home / "data"),
            "PRIME_AGENT_CODING_AGENT_DIR": str(agent),
            "PRIME_AGENT_SESSION_DIR": str(home / "sessions"),
            "PRIME_AGENT_KERNEL_VENV": str(home / "kernel"),
            "RLM_GLOBAL_HARNESS_STATE_DIR": str(home / "global-harness"),
            "RLM_HARNESS_STATE_DIR": str(home / "local-harness"),
            "RLM_SESSION_DIR": str(home / "artifacts"),
            "DO_NOT_TRACK": "1", "PRIME_AGENT_TELEMETRY": "0", "PI_SKIP_VERSION_CHECK": "1",
            "PRIME_AGENT_RUST_INSTALLER_URL": "http://127.0.0.1:9/installer",
            "PRIME_AGENT_DOWNLOAD_BASE_URL": "http://127.0.0.1:9/releases",
        }

        def run(args: list[str]) -> str:
            result = subprocess.run(
                [str(binary), *args], cwd=home, env=env,
                text=True, capture_output=True, timeout=30, check=False,
            )
            if result.returncode:
                raise AssertionError(f"{args}: {result.returncode}\n{result.stdout}\n{result.stderr}")
            return result.stdout

        assert run(["--version"]).strip() == version
        assert "prime-agent" in run(["--help"])
        refusal = subprocess.run(
            [str(binary), "update", "--check"], cwd=home, env=env,
            text=True, capture_output=True, timeout=15, check=False,
        )
        assert refusal.returncode == 75 and "just update-prime" in refusal.stdout, (
            "Nix installation must refuse self-update before any upstream access", refusal
        )
        run(["mcp", "add", "offline", "--", "fixture-command"])
        expected = {**initial, "mcpServers": {
            **initial["mcpServers"], "offline": {"type": "stdio", "command": "fixture-command"},
        }}
        assert json.loads(settings.read_text()) == expected, "MCP save clobbered settings"
        assert not (home / "kernel").exists(), "CLI smoke unexpectedly bootstrapped a kernel"
        assert not list(home.rglob("*.sock")), "CLI smoke unexpectedly started a daemon"
        for name in ("telemetry.jsonl", "telemetry-install-id.json", "agent-traces-outbox.json"):
            assert not (agent / name).exists(), f"unexpected reporter state: {name}"
    print("Installed version/help/Nix-ownership/MCP/settings smoke passed")


if __name__ == "__main__":
    smoke(Path(sys.argv[1]).resolve(), sys.argv[2])
