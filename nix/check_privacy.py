"""Fail closed if a source tree or installed payload retains vendor reporters."""

import argparse
from pathlib import Path
import re


FORBIDDEN = (
    b"agent-analytics/events",
    b"agent-traces/sessions",
    b"install_session_telemetry",
    b"ContinuousTraceUpload",
    b"AnalyticsSink",
    b"prime-agent-traces",
    b"pa_telemetry",
    b"pa-telemetry",
    b"PRIME_AGENT_TRACES_API_KEY",
    b"PRIME_AGENT_TRACES_BASE_URL",
    b"installationId",
    b"telemetry.jsonl",
    b"agent-traces-outbox",
)
SOURCE_CALLBACKS = re.compile(
    r"\b(?:pa_telemetry|install_session_telemetry|ContinuousTraceUpload|AnalyticsSink)\b"
)


def artifact_errors(root: Path) -> list[str]:
    errors = []
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        content = path.read_bytes()
        for marker in FORBIDDEN:
            if marker in content:
                errors.append(f"{path.relative_to(root)}: vendor reporter marker {marker.decode()}")
    return errors


def source_errors(root: Path) -> list[str]:
    errors = []
    manifest = root / "Cargo.toml"
    if '"crates/pa-telemetry"' in manifest.read_text():
        errors.append("Cargo.toml: vendor reporter crate remains in workspace")
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        if "tests" in path.parts or path.name.endswith(("tests.rs", "test_env.rs")):
            continue
        # Inline test fixtures can retain legacy inputs. Installed artifacts are
        # checked independently, including their actual executable bytes.
        production = path.read_text().split("#[cfg(test)]", 1)[0]
        code = "\n".join(line.split("//", 1)[0] for line in production.splitlines())
        if SOURCE_CALLBACKS.search(code) or any(marker.decode() in code for marker in FORBIDDEN):
            errors.append(f"{path.relative_to(root)}: active vendor reporter import/callback")
    return errors


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--artifact", type=Path)
    group.add_argument("--source", type=Path)
    args = parser.parse_args()
    if args.artifact and not (args.artifact / "prime-agent").is_file():
        parser.exit(1, "Installed prime-agent executable is missing\n")
    errors = artifact_errors(args.artifact) if args.artifact else source_errors(args.source)
    if errors:
        parser.exit(1, "\n".join(errors) + "\n")
    print("Vendor reporting source/artifact gate passed")


if __name__ == "__main__":
    main()
