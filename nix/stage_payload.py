"""Stage only immutable runtime inputs; never copy a developer venv or cache."""

import json
from pathlib import Path
import shutil
import sys
import tomllib


def stage(source: Path, target: Path) -> None:
    target.mkdir(parents=True, exist_ok=True)
    runtime = source / "prime-agent-runtime"
    destination = target / "prime-agent-runtime"
    destination.mkdir()
    for name in ("pyproject.toml", "uv.lock"):
        shutil.copyfile(runtime / name, destination / name)
    excluded = shutil.ignore_patterns(
        ".venv", "__pycache__", "*.pyc", "*.egg-info", ".git", "test", "tests"
    )
    shutil.copytree(runtime / "src", destination / "src", ignore=excluded)
    shutil.copytree(source / "skills", target / "skills", ignore=excluded)
    snapcompact = source / "crates" / "pa-core" / "src" / "snapcompact"
    if snapcompact.is_dir():
        # Embedded fonts retain their own licenses in the installed package.
        shutil.copytree(snapcompact / "assets" / "licenses", target / "licenses" / "snapcompact")
    for package in ("crossterm", "mlua"):
        vendor = source / "vendor" / package
        if vendor.is_dir():
            notices = target / "licenses" / package
            notices.mkdir(parents=True)
            shutil.copyfile(vendor / "LICENSE", notices / "LICENSE")
    for name in ("LICENSE", "README.md"):
        shutil.copyfile(source / name, target / name)
    if (source / "docs").is_dir():
        shutil.copytree(source / "docs", target / "docs", ignore=excluded)
    version = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    (target / "package.json").write_text(json.dumps({"version": version}) + "\n")


if __name__ == "__main__":
    stage(Path(sys.argv[1]), Path(sys.argv[2]))
