---
name: factory
description: >-
    Run and monitor state-machine workflows of child agents. Use when the user wants fan-out, joins, bounded review loops, or progress control through rlm.factory.
---

# Factory

Use the pre-imported `rlm.factory` namespace to run machines from the machine
library. A machine defines states, child-agent prompts, and transitions.

## Start a workflow

1. Inspect a library `MACHINE.md` to choose a machine. To read the embedded
   reference, call `print(rlm.factory.help())` synchronously; it works before
   Factory is enabled. That reference also covers custom persistence adapters.
   Use the library workflow and exact public methods below for this session.
2. Factory is disabled by default. Ask the user to enable `factory.enabled`
   before running or monitoring a workflow. The six async methods below refuse
   with `the factory is disabled; run /factory on to enable it` while it is off.
3. Choose an existing library machine. Start it with
   `run = await rlm.factory.run("review-sweep", name="review")`.
   `run` validates the machine and returns a run descriptor. A background kernel
   task continues the workflow; finish the turn instead of polling in a loop.
4. Save `run["run_id"]`. On a later turn, inspect its status or wait for a bounded
   change. A paused run needs an explicit resume decision. Stop the run when the
   user asks to cancel its children.

## Public methods

All methods except `help` are async. Await each call.

- `rlm.factory.run(spec_id: str, *, name: str | None = None)`: start a library
  machine by name. A host-injected persistence adapter can also resolve a stored
  spec. The standard runtime does not require such an adapter.
- `rlm.factory.status(run_id: str)`: read state, recent events, elapsed time, and
  usage for a known run.
- `rlm.factory.stop(run_id: str)`: cancel its running children and stop the run.
- `rlm.factory.resume(run_id: str)`: resume a paused run; other states are errors.
- `rlm.factory.graph(ref: str | None = None)`: read a live run's graph by run ID,
  or omit `ref` for the run collection. A library machine name alone does not
  resolve a static graph.
- `rlm.factory.watch(run_id: str, timeout: float = 0.0)`: return the graph with
  `changed` after a shape change or bounded timeout in seconds. The default
  timeout returns immediately.
- `rlm.factory.help() -> str`: return the embedded machine authoring reference.

Run state lives in kernel memory. Keep the run ID for later calls in that kernel;
these methods are not a durable run-recovery API.
