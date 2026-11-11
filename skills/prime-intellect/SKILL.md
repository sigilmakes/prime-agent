---
name: prime-intellect
description: Operate Prime Intellect services with the prime CLI and Python SDKs - verifiers environments, evaluations, Hosted Training and prime-rl, sandboxes, Prime Inference, GPU pods and clusters, storage, and tunnels. Use when a task involves the prime CLI, environments, evals, training runs, sandboxes, GPU rental, or Prime Inference models.
---

# Prime Intellect CLI

Prime Intellect provides environments and evaluations, post-training, an
OpenAI-compatible inference API, GPU pods and clusters, storage, and tunnels.
Use the `prime` CLI for these operations.

Current documentation is at https://docs.primeintellect.ai. Append `.md` to a
docs page URL to fetch it as Markdown; the index is at
https://docs.primeintellect.ai/llms.txt.

## Products

| Product | Purpose | Reference |
|---|---|---|
| verifiers | Python library for LLM environments and evaluations | [environments.md](references/environments.md) |
| Environments Hub | Shared environment registry (`prime env`) | [environments.md](references/environments.md) |
| Hosted Evaluations | Evaluations on managed infrastructure (`prime eval run --hosted`) | [environments.md](references/environments.md) |
| Hosted Training | Post-training against environments (`prime train`, Lab) | [environments.md](references/environments.md) |
| prime-rl | Async RL framework for self-managed training | [environments.md](references/environments.md) |
| Sandboxes | Disposable VM sandboxes for generated or untrusted code | [sandboxes.md](references/sandboxes.md) |
| Tunnels | Public HTTPS URLs for local or sandboxed services | [sandboxes.md](references/sandboxes.md) |
| Inference | OpenAI-compatible inference API | [inference.md](references/inference.md) |
| Compute | Single-GPU pods and multi-node clusters | [compute.md](references/compute.md) |
| Storage | Persistent disks attached to instances | [compute.md](references/compute.md) |

## Install and sign in

```bash
uv tool install prime    # or: pip install prime
prime login              # browser auth; or: prime config set-api-key
prime config view        # verify configuration
```

The same package provides the Python SDKs, such as `prime_sandboxes` and
`prime_tunnel`. Source: https://github.com/PrimeIntellect-ai/prime

## Fetching current details

Exact flags, API schemas, and availability change. Read the live docs instead
of guessing:

```bash
curl -s https://docs.primeintellect.ai/llms.txt                      # page index
curl -s https://docs.primeintellect.ai/sandboxes/overview.md         # page as markdown
```

The REST API is documented under `api-reference/` pages. Base URL:
https://api.primeintellect.ai. OpenAPI spec:
https://api.primeintellect.ai/openapi.json.

## Command reference

```bash
# Environments Hub
prime env list --search "math"      # discover environments
prime env info owner/name           # inspect one
prime env install owner/name        # install locally
prime env init my-env --v1          # scaffold a new environment
prime env push                      # publish to the Hub

# Evaluations
prime eval run my-env -m openai/gpt-4.1-mini -n 5    # local smoke eval
prime eval run owner/env --hosted --follow           # hosted eval with logs

# Training
prime lab setup                     # set up a Lab workspace (Hosted Training)
prime train models                  # models, capacity, pricing
prime train init && prime train rl.toml              # configure and launch a run

# Sandboxes
prime sandbox create python:3.11-slim --timeout-minutes 120
prime sandbox run <sandbox-id> "python --version"
prime sandbox delete <sandbox-id>

# Inference
prime inference models              # list available models

# Compute
prime availability list             # GPU availability and pricing
prime pods create                   # provision a pod
prime pods ssh <pod-id>             # SSH in (needs: prime config set-ssh-key-path)
```

## Operating notes

- Prefer the ecosystem commands (`prime env init`, `prime eval run`,
  `prime lab setup`) over custom scaffolding.
- Run a small eval (`-n 5`) before scaling; keep default result uploads unless
  the user explicitly opts out.
- For non-trivial eval or training work, ask whether the user wants instruct
  models (`gpt-4.1` series, `qwen3` instruct) or reasoning models (`gpt-5`
  series, `qwen3` thinking, `glm` series).
- Hosted Training launches from a CPU machine. Self-managed `prime-rl` needs
  local GPU access and is a power-user path.
- The web console at https://app.primeintellect.ai manages API keys, billing,
  and team settings that the CLI does not expose.
