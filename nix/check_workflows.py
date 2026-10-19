"""Reject active upstream/mirror automation and downstream publication steps."""

import argparse
from pathlib import Path
import re


PUBLISH = re.compile(
    r"(?i)\b(?:git\s+push|npm\s+publish|gh\s+release|fj\s+release|wrangler\s+deploy)\b"
    r"|(?:actions/create-release|action-gh-release|github-push-action)"
)


def errors(root: Path) -> list[str]:
    found = []
    for path in sorted((root / ".github" / "workflows").glob("*")):
        if path.suffix in {".yml", ".yaml"}:
            found.append(f"{path.relative_to(root)}: mirrored GitHub workflows must stay archived")
    downstream = root / ".forgejo" / "workflows"
    workflows = sorted([*downstream.glob("*.yml"), *downstream.glob("*.yaml")])
    if not workflows:
        found.append("No audited Forgejo CI workflow found")
    for path in workflows:
        # Comments are not commands, but publication actions in any live job
        # must be reviewed explicitly rather than silently re-enabled.
        text = "\n".join(line for line in path.read_text().splitlines() if not line.lstrip().startswith("#"))
        if PUBLISH.search(text):
            found.append(f"{path.relative_to(root)}: automatic publication is forbidden")
    return found


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    findings = errors(args.root)
    if findings:
        parser.exit(1, "\n".join(findings) + "\n")
    print("Downstream workflow policy passed")
