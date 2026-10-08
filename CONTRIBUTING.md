# Working on this fork

[Mnemosyne](https://mnemosyne.sigilzero.dev/sigilzero/prime-agent) is the source of truth.
Use its issues for bugs, ideas, and work tracking. GitHub is an automatic mirror,
not a contribution queue. No upstream Discussion or vouch is required.
Never include credentials or private session data in issues or logs.

## Development

- Read [AGENTS.md](AGENTS.md) for validation and safety rules.
- Solo work may use `main`. Use a short-lived branch when isolation or review helps.
- Keep changes scoped to an issue. Add behavioral tests and describe validation.
- Use `nix develop` for pinned tools. Before committing, run
  `nix flake check --print-build-logs` and, through `nix develop`,
  `TEST_POLICY_BASE=<actual-base-commit> npm run check:test-policy`.
- `npm run check:ci` is nonmutating. `npm run check` intentionally formats files.
  The pre-commit hook never formats or restages files, so partial staging is preserved.
  Checks read the working tree; review the staged diff separately.
- Use isolated home/config/session directories for tests. Never use personal
  credentials or connect test clients to the user's daemon.
- Add user-visible package changes as bullets in `packages/<pkg>/.changes/<slug>.md`.
  Do not edit released changelogs. Internal workflow changes need no fragment.

## Remotes and hooks

Push validated work to `origin` (Mnemosyne), including `main`. Forgejo mirrors
branches and tags to GitHub. Never push directly to `github` or `upstream`.
Releases, installation changes, and destructive history changes need separate approval.
Inherited release commands and archived GitHub workflows are not this fork's release path.

Activate the tracked hooks without installing npm dependencies:

```sh
git config core.hooksPath .husky
```

The push guard rejects GitHub hosts and the `github`/`upstream` remote names.
On `origin` and Mnemosyne hosts it blocks deletions, more than ten ref updates,
and destinations outside branches/tags. Other scratch remotes retain normal Git behavior.
For an explicitly authorized destructive primary push only, use
`PRIME_AGENT_ALLOW_MIRROR_PUSH=1 git push origin ...`. This does not permit
GitHub/upstream pushes or malformed input. Hooks are accident protection, not a
security boundary: SSH aliases can conceal hosts, and Git can bypass local hooks.
