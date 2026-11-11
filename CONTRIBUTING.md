# Contributing to Weave

Weave runs on user machines and can execute code with the user's permissions. Read
[AGENTS.md](AGENTS.md) before contributing; it holds the engineering, testing, and
merge rules for this repository.

## Start with an issue

Use the [Forgejo issue tracker](https://mnemosyne.sigilzero.dev/sigilzero/weave/issues)
and search it before opening a new issue. For a bug, include the `wv --version`
output, your platform, the command you ran, and the expected and actual results.
Keep the report reproducible. Never share API keys, tokens, private prompts,
transcripts, or other sensitive information.

Report security vulnerabilities privately to the maintainers instead of posting
them publicly.

An existing issue does not mean an external pull request is wanted. Ask in the
issue before starting substantial work; maintainers may decline changes that do
not fit the project.

## Pull requests

If a maintainer invites a pull request, open it against `main` on Mnemosyne:
<https://mnemosyne.sigilzero.dev/sigilzero/weave/pulls>. GitHub is a mirror; push
only to `origin`.

- Keep one logical change per pull request. Do not mix unrelated refactors or
  dependency changes into it.
- Follow AGENTS.md and the existing conventions.
- Explain the scope and how you validated the change.
- Add or update tests for behavioral changes. Bug fixes need a regression test
  that fails without the fix.
- User-visible changes need the parity-diff evidence described in
  [AGENTS.md](AGENTS.md).
- State ownership compliance for crate changes as [AGENTS.md](AGENTS.md) requires.

Run the gates before requesting review:

```bash
make check
nix flake check --no-update-lock-file
```

Use `nix develop --no-update-lock-file` for the pinned tools. Keep test state
isolated and never use live user credentials.

## Commits

Never set git identity for commits to this repository and never add
`Co-authored-by` trailers; [AGENTS.md](AGENTS.md) explains both rules.
