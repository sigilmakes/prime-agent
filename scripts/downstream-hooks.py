#!/usr/bin/env python3
"""Read-only downstream Git hooks. Install: git config core.hooksPath .husky.

Only origin at an exact Mnemosyne repository URL may receive pushes. The legacy
PRIME_AGENT_ALLOW_MIRROR_PUSH=1 escape hatch permits large/destructive ref sets,
not other destinations or malformed input. Local Git hooks can be bypassed;
server-side permissions remain the final authority.
"""

import os
import re
import subprocess
import sys

ORIGIN_URL = "ssh://git@mnemosyne.sigilzero.dev/sigilzero/prime-agent.git"
# Exact supported spellings, not substring/hostname guesses. Never rewrite config.
ALLOWED_URLS = frozenset(
    prefix + suffix
    for prefix in (
        "ssh://git@mnemosyne.sigilzero.dev/sigilzero/prime-agent",
        "git@mnemosyne.sigilzero.dev:sigilzero/prime-agent",
        "https://mnemosyne.sigilzero.dev/sigilzero/prime-agent",
    )
    for suffix in ("", ".git")
)
MAX_PAYLOAD = 1024 * 1024
OID = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", re.ASCII)


class Refused(ValueError):
    """The hook cannot safely accept this request."""


def git(*args):
    # No refresh locks, external diff drivers, or filesystem-monitor helpers.
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0")
    return subprocess.run(
        ["git", "-c", "core.fsmonitor=false", *args],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )


def valid_ref(ref):
    return (
        ref.startswith("refs/")
        and all(32 < ord(char) < 127 for char in ref)
        and git("check-ref-format", ref).returncode == 0
    )


def validate_push(remote, url, payload, allow_destructive=False):
    """Validate the entire Git pre-push payload before applying any override."""
    if remote != "origin" or url not in ALLOWED_URLS:
        raise Refused("push only to origin at " + ORIGIN_URL)
    if len(payload) > MAX_PAYLOAD:
        raise Refused("ref payload is too large")
    try:
        text = payload.decode("ascii")
    except UnicodeDecodeError as error:
        raise Refused("ref payload is not ASCII") from error
    if text and not text.endswith("\n"):
        raise Refused("truncated ref payload")
    rows = text.split("\n")[:-1]
    destinations = set()
    destructive = False
    oid_length = None
    for row in rows:
        fields = row.split(" ")
        if len(fields) != 4 or any(not field for field in fields):
            raise Refused("malformed ref line")
        local_ref, local_oid, remote_ref, remote_oid = fields
        if not OID.fullmatch(local_oid) or not OID.fullmatch(remote_oid):
            raise Refused("malformed object ID")
        if len(local_oid) != len(remote_oid) or oid_length not in (None, len(local_oid)):
            raise Refused("mixed object ID formats")
        oid_length = len(local_oid)
        if not valid_ref(remote_ref) or remote_ref in destinations:
            raise Refused("invalid or duplicate destination ref")
        destinations.add(remote_ref)
        local_zero = set(local_oid) == {"0"}
        remote_zero = set(remote_oid) == {"0"}
        if local_ref == "(delete)":
            if not local_zero or remote_zero:
                raise Refused("invalid deletion object IDs")
            destructive = True
        else:
            if local_zero:
                raise Refused("zero source object ID without deletion")
            if not (local_ref == "HEAD" or OID.fullmatch(local_ref) or valid_ref(local_ref)):
                raise Refused("invalid source ref")
        if not remote_ref.startswith(("refs/heads/", "refs/tags/")):
            destructive = True
    if (destructive or len(rows) > 10) and not allow_destructive:
        raise Refused(
            "large, deleting, or non-branch/tag push requires explicit authorization; "
            "then set PRIME_AGENT_ALLOW_MIRROR_PUSH=1 for that command only"
        )


def main(args):
    if args == ["pre-commit"]:
        result = git("diff", "--cached", "--check", "--no-ext-diff", "--no-textconv")
        sys.stdout.buffer.write(result.stdout)
        sys.stderr.buffer.write(result.stderr)
        return result.returncode
    if len(args) == 3 and args[0] == "pre-push":
        validate_push(
            args[1], args[2], sys.stdin.buffer.read(MAX_PAYLOAD + 1),
            allow_destructive=os.environ.get("PRIME_AGENT_ALLOW_MIRROR_PUSH") == "1",
        )
        return 0
    raise Refused("invalid hook arguments")


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (Refused, OSError) as error:
        print("downstream hook: refusing: " + str(error), file=sys.stderr)
        sys.exit(1)
