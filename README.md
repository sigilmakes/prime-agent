<p align="center">
  <a href="https://primeintellect.ai">
    <picture>
      <source media="(prefers-color-scheme: light)" srcset="https://github.com/user-attachments/assets/40c36e38-c5bd-4c5a-9cb3-f7b902cd155d">
      <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/6414bc9b-126b-41ca-9307-9e982430cde8">
      <img alt="Prime Intellect" src="https://github.com/user-attachments/assets/6414bc9b-126b-41ca-9307-9e982430cde8" width="312" style="max-width: 100%;">
    </picture>
  </a>
</p>

<h3 align="center">
Prime Agent: A Self-Improving RLM Harness
</h3>

<p align="center">
  <a href="#install">Documentation</a> &bull;
  <a href="https://github.com/PrimeIntellect-ai/verifiers">Verifiers</a> &bull;
  <a href="https://github.com/PrimeIntellect-ai/prime-rl">PRIME-RL</a>
</p>

<p align="center">
  <a href="https://mnemosyne.sigilzero.dev/sigilzero/prime-agent/actions">Downstream CI</a>
  <a href="https://arxiv.org/abs/2608.23552">
    <img src="https://img.shields.io/badge/arXiv-2608.23552-b31b1b.svg" alt="arXiv" />
  </a>
</p>

<p align="center">
  <a href="https://trendshift.io/repositories/104249?utm_source=repository-badge&amp;utm_medium=badge&amp;utm_campaign=badge-repository-104249" target="_blank" rel="noopener noreferrer">
    <img src="https://trendshift.io/api/badge/repositories/104249" alt="PrimeIntellect-ai%2Fprime-agent | Trendshift" width="250" height="55" />
  </a>
</p>

Prime Agent is an open-source coding and research agent for general and long-running work. It is designed around two core abstractions:

- The **[Recursive Language Model (RLM)](https://www.primeintellect.ai/blog/rlm)** treats context as variables (*prompt-as-a-variable*) and tools like recursive subagents as function calls (*programmatic tool /sub-agent calling*) inside a persistent REPL.
- The **[Continual Harness](https://arxiv.org/abs/2605.09998)** stores supplemental prompts, memories, skill descriptions, and reusable subagent specifications as durable state that Prime Agent can refine through small, evidence-backed updates, local to the session by default.

## Install

This is Willow's downstream Rust fork. Mnemosyne is the source of truth;
GitHub is its mirror. It preserves native Ollama Cloud support, provider catalog
refresh, local transcript history, and the fork's no-vendor-reporting policy.

Build the complete application with its Python runtime, skills, and catalogs:

```bash
nix build
./result/bin/prime-agent
```

Use `nix develop` for the pinned development tools. On Willow's NixOS fleet,
`/config` owns installation and pins this repository through `prime-agent-src`.
Update that pin with the guarded fleet updater and deploy through the fleet pipeline;
do not use upstream installers, global npm packages, or self-update for a Nix-owned
installation.

Downstream releases are manual, source-only Forgejo Releases tagged
`fork-YYYY.MM.DD.N` at an exact reviewed commit. The package version remains
`0.10.0`; the source tag does not restamp the application. CI validates but never
publishes. No downstream binaries, npm packages, catalogs, or installers are
published. Portable packaging tools and archived upstream workflows are reference
material, not the downstream installation path. See the
[source release procedure](scripts/release/README.md).

Before integration, `make check` runs the full workspace formatting, clippy, test,
and hook gates. `nix flake check --no-update-lock-file` adds the Nix package,
privacy, and audited runtime checks; it does not replace the full Rust gate.
For credential-isolated validation with a prepared real kernel, enter
`nix develop --no-update-lock-file`, then run:

```bash
python3 -B scripts/check_workspace.py
```

This can download locked build/Python dependencies. It never needs live provider
or MCP credentials. It uses private test state and one reusable Cargo target;
see the release procedure for limits and explicit cache/kernel reuse options.

## Why Prime Agent

Prime Agent combines a persistent Python control environment with durable harness state, so useful working context and reusable operating patterns can outlive a single chat window.

- **Everything is programmatic:** a persistent Python REPL is the built-in model tool; file operations, shell commands, tool use, subagents, and context management happen through code.
- **Subagents are built in:** `rlm.spawn(...)` spawns real child agents for parallel or background work and returns their results programmatically.
- **The harness can improve:** `/refine` reviews the current trajectory and can apply small, evidence-backed updates to supplemental harness state. It never rewrites the immutable base system prompt, and recorded snapshots support rollback.
- **Skills are executable:** skills are importable Python packages, and the built-in skill creator can turn recurring workflows into project or personal skills.
- **Sessions run in the background:** daemon-backed agents keep running when the terminal disconnects and can be reattached later.
- **Agents communicate directly:** running agents can exchange messages and orchestrate one another without routing everything through the user.
- **Long tasks keep moving:** automatic compaction, persistent goals, heartbeats, schedules, autonomous mode, and retained subagents preserve progress across turns and terminal sessions.

## Context compaction

The builtin rolling context strategy tries server-side compaction, then Snapcompact,
then standard text summarization. No compaction route settings are required.
The server attempt uses the **resolved** `openai-responses` provider, endpoint and
model; other transports skip it. In particular, Codex native archive replay is
not supported: Codex vision models continue to Snapcompact without a server probe. Only an explicit unsupported-capability response
(or a local input-budget decline) advances the chain. Authentication, route errors,
malformed responses, cancellation and uncertain network outcomes stop the operation.
Provider-native archives stay bound to that exact route and model. The server request
inherits resolved model/provider authentication headers; its explicit API key remains
the bearer credential.

Snapcompact requires image input. Native Anthropic and Google transports select their
image profiles. OpenAI-compatible transports use recognized Claude, Gemini or OpenAI
model lineages, not the gateway's provider name. Unknown aliases skip Snapcompact
unless `plugins.snapcompact.config.profile` explicitly selects `claude`, `google`,
`openai` or `explicit` (with `frame_size` and `frame_tokens`). Canonical history is
retained independently of these context archives.

To replace the chain, set `plugins.default_context.config.compactors` to an ordered
array of `plugin/compactor` routes (an empty array disables compaction attempts).
For example, `["standard_compaction/standard"]` selects text summarization only.
Disabled compactor plugins are skipped. Set `plugins.server_compaction.config.route`
to `null` to disable the server probe, or supply an exact `{provider, endpoint, model}`
route override; a mismatched override is an error. Custom context strategies keep
control of their own chains.

## Slack

Connect Slack from `/mcp` or run `/mcp login slack` inside Prime Agent to search
your workspace and use Slack tools. See the [Slack connector guide](docs/slack-mcp.md)
for OAuth setup and troubleshooting.

## Clipboard over SSH and tmux

Prime Agent uses the local clipboard tool (such as `pbcopy`) when available. In tmux it asks the attached tmux client to forward a clipboard buffer if that client advertises the `Ms` capability. This works with tmux's default `set-clipboard external`, unlike application-origin OSC 52. Without tmux, remote copies request delivery using OSC 52. Terminal requests cannot confirm that your local clipboard changed, so Prime Agent reports them as unconfirmed.

Nested tmux servers and terminals may still block delivery. An outer tmux with `set-clipboard external` can reject the inner server's OSC 52. In that case, run `tmux set -s set-clipboard on` on the outer server, or use native terminal selection (Shift-drag in most terminals; Option-drag in iTerm2) followed by Cmd-C. macOS Terminal.app does not support OSC 52; iTerm2 requires its “Applications in terminal may access clipboard” setting.

## Custom certificate authorities

HTTPS requests trust your operating system's trust store together with the bundled Mozilla roots. For servers behind a custom CA, point `NODE_EXTRA_CA_CERTS` at a PEM bundle of your CA certificates — the same variable the Node/TypeScript build honors:

```bash
export NODE_EXTRA_CA_CERTS=/path/to/your-ca.pem
```

Each transport reads the variable once when its first client is initialized. The background daemon keeps the environment it started with, so run `prime-agent shutdown` after changing it.

## Getting Started

Start Prime Agent from the repository or directory you want it to work in:

```bash
cd /path/to/project
prime-agent
```

On first launch, run `/login` to choose a subscription or API-key provider. Prime Agent works in the current directory and can run commands and modify files there. Use a disposable clone, clean worktree, or another checkpoint you can inspect and restore.

> [!WARNING]
> Prime Agent executes model-generated Python and project commands with your user permissions. Its worker and kernel processes improve lifecycle isolation and recovery; they are **not** a security sandbox. Review changes and use trusted repositories, instructions, and skills only. Run untrusted code or instructions in an external sandbox or restricted environment.

Useful commands:

```bash
prime-agent agents                   # Browse running, idle, and saved sessions
prime-agent attach <agent>           # Reattach to a running session
prime-agent --resume [path|id]       # Browse sessions or resume one directly
prime-agent status                   # Inspect background service state
prime-agent doctor [--fix]           # Inspect or repair background services
prime-agent update [--force]         # Update Prime Agent
prime-agent shutdown [--force]       # Stop every agent, worker, and background service
```

## Built for Long-Running Work
Prime Agent is built for long-running work, especially for evaluations in research. These features are available in the TUI, and when run autonomously.

- **Continual Harness:** `/refine` can persist focused, reviewable lessons as supplemental prompts, memories, reusable skill descriptions, or subagent specifications, with recorded refinement history. It does not replace packaging and reviewing new executable skills.
- **Direct agent-to-agent communication:** running agents and retained subagents can discover one another, exchange messages, and steer active work.
- **Daemon-backed continuity:** active sessions, Python REPL state, schedules, and subagents keep running when the terminal detaches and can be reattached later.
- **Heartbeats and schedules:** `/heartbeat`, `rlm_heartbeat`, and `prime-agent schedule` can re-enter a session periodically or at a specific time.
- **Persistent goals:** `/goal` keeps an objective and its progress active across turns until it is completed, paused, or cleared.
- **Bounded autonomous mode:** `/autonomous` continues within configured turn, token, and time budgets and can run user-defined quality gates. A passed gate checks only what that gate verifies; reaching a limit does not imply task success.

## Contributing

Read [AGENTS.md](AGENTS.md) before contributing. Use the downstream
[Forgejo issues](https://mnemosyne.sigilzero.dev/sigilzero/prime-agent/issues) and
[pull requests](https://mnemosyne.sigilzero.dev/sigilzero/prime-agent/pulls).
GitHub and archived upstream automation are not downstream publication authority.

## License

Prime Agent is fully open source and released under the [MIT License](LICENSE).

## Citation

If you use this codebase in your research, please cite Prime Agent:

```bibtex
@article{karten2026prime,
  title={Prime Agent: A Self-Improving RLM Harness},
  author={Karten, Seth and Zhang, Alex L. and Thomas, Kevin and Müller, Sebastian and Bakouch, Elie and Auras, Daniel and Senghaas, Mika and Obeid, Fares and Dunas, Konstantin and Hagemann, Johannes and Jaghouar, Sami},
  journal={arXiv preprint arXiv:2608.23552},
  year={2026}
}
```

Available at [https://arxiv.org/abs/2608.23552](https://arxiv.org/abs/2608.23552).

### Automatic compaction settings

`/settings` exposes the global automatic compaction percentage in five-point steps,
with a 95% default. A saved off-grid percentage stays selectable. The effective
trigger also reserves room for retained history and summaries; the model's input
and requested-output ceiling always takes priority. Manual compaction budgets are
unchanged. The underlying `compaction.thresholdPercent` setting accepts 1–100,
including fractional values; project settings can override the global setting.

For per-model early compaction, use the existing provider-scoped
`modelOverrides.<id>.contextWindow` option in `models.json`, for example:

```json
{
  "providers": {
    "prime-inference": {
      "modelOverrides": {
        "your-model-id": { "contextWindow": 524288 }
      }
    }
  }
}
```

Use the actual model ID for your provider. This sets that model's effective context
window; it does not change every model's global compaction percentage.
