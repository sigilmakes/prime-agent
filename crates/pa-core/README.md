# pa-core

The session engine.

## Scope
MCP host side (`mcp/`): auth gating for built-in integrations (the
disabled-skill overrides and `/mcp list` status), the `mcpServers` settings
seam, and the `mcp.*` host-request handlers the kernel's generic MCP
client reaches through (`mcp.config`, `mcp.refresh`, optional
`mcp.begin_login`). The OAuth flow lives behind it (`oauth*` submodules:
RFC 9728 discovery, dynamic registration, PKCE, the local callback
server, token refresh; `McpOAuthConfig::callback_port` selects an exact registered
desktop callback port, while dynamic clients retain the candidate range.
Slack's endpoint selects its documented public desktop client; `login.rs`: login execution that persists the
endpoint-bound credential into the shared auth store and the
`wire_begin_login`/`McpOAuth` refresh integration hosts wire). The MCP
protocol itself runs kernel-side (Python
stdio/HTTP); the engine never spawns MCP servers. Tools (bash, edit, python_repl + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, settings/config, package manager (npm/git/local source install/remove/list/update against settings), session manager (persist/resume). The RLM recursion host seam
(`session_engine::rlm_host`): the trait the kernel's `rlm.spawn`/
`rlm.create_session`/`rlm.list_subagents`/`rlm.collect`/
`rlm.delete_subagent` host requests call into, with the roster/collect/
selector-error vocabulary the daemon implements over the supervisor link.
Platform wall (`platform`): process control (signals/process groups, kernel exit waits), file locking, file permissions, and shell selection - every OS-specific behavior in the engine lives there behind cfg-gated implementations (the durable-write rename primitive is `platform::rename_onto`). Scheduled jobs (`cron`, the `AgentCronJobStore` port of `core/cron-jobs.ts`): file-backed job state under session artifacts (`scheduled-jobs.json` partitions) with cross-process locking, plus the public read-only scan (`cron::store::read_scheduled_jobs_artifact`) the update flow's roster projection and boot re-arm read. Tools (bash, edit, python_repl + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume). origin/main

Provider resilience policies in `session_engine`: the shared quick-retry
policy (TS `provider-retry.ts`: permanent-kind classification,
Retry-After-aware capped delays), the interactive auto-retry loop, and the
provider-failover driver (when a provider exhausts its quick retries and
another configured provider serves the same model, the failed turn
re-routes to the next provider in catalog order; the TS
`auto_retry_start`/`reason: "backup"` event vocabulary surfaces the
progression). The drivers are pure decision logic: the host (pa-daemon)
owns the attempt, the wait, and the model switch.

Session HTML export (`export_html`): the standalone viewer file for
`/export` and `session export` - the embedded product template
(assets/export-html, vendored marked/highlight.js attributed in its
NOTICE.md) plus the theme-to-CSS resolution over the bundled theme data
(pa-types) and custom themes under `<agent-dir>/themes/`, the tools
section mapping (`tools_section`: the session's tool registry to the
template's name/description/parameters list), and the custom-tool
pre-render pipeline (`ToolHtmlRenderer`/`pre_render_custom_tools` +
`ansi_to_html`: the exporter walks the entries, the session layer's
renderer produces line-oriented output, ANSI converts to inline-styled
HTML at the export step), and the two entry points
(`export_session_to_html` for the daemon worker's `export_html`
command, `export_from_file` for the CLI). Terminal theme rendering stays
pa-tui's; tool renderers live with the session's tool surface;
the share-viewer flow (gist creation) is the caller's.

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No update coordination (the pa-cli coordinator owns the FSM; the update-flow seam here is the daemon-free support layer: `update::version` (semver/channel policy), `update::install` (the managed install-root layout), `update::release` (the channel manifest fetch), `update::download` (sha256-verified archive download + staging)). The package manager installs sources and resolves resource paths only.

## Public API

`session_engine::PluginJobSink` and `AgentSession::set_plugin_job_sink` let hosts
observe future terminal deferred-job outcomes. Callbacks run outside the job
registry lock; retired generations cannot publish. Observation never consumes
results returned by the independent drain API. Successful job results may include
`message` using the existing custom-message payload (`customType`, `content`,
`display`, optional object `details`) for a host to present; the generic job
registry does not interpret that payload.

`SessionEngine` (message in -> events out), `export_html` (the HTML exporter plus its renderer seam: `tools_section`, `pre_render_custom_tools` + `ToolHtmlRenderer`/`RenderedToolHtml`/`RenderedToolResult`, and `ansi_to_html::ansi_lines_to_html`/`tool_render::trim_rendered_result_lines` for renderer implementers), `ToolRegistry`, kernel manager, settings (`SettingsManager`), packages (`packages::PackageManager` + source types), `platform` (process control, file locking, permissions, shell selection - usable by higher crates, e.g. pa-cli's detached spawn), `session_engine::rlm_host::RlmSubagentHost` (implemented by pa-daemon for supervisor-backed children and by pa-cli's print mode for in-process children; the default is no children; the trait crate also owns the shared spawn/depth facts — `DEFAULT_RLM_MAX_DEPTH`, the depth-limit and name errors, the daemonless `create_session` refusal, `rlm_child_session_dir`, and the `persisted_rlm_max_depth`/`resolve_rlm_max_depth` precedence chain), `session_engine::semantic_edges` (the TS `semantic-edges.ts` WRITER: `SemanticEdgeRecorder`/`SemanticEdgeIdentity`, `semantic_edge_ledger_path` + `last_committed_request_id`, and the `X-ACP-Model-Request-ID`/`Idempotency-Key` constants — one durable per-session ledger of model-request events, byte-compatible with the TS lines; the fold and any publishing stay downstream), `mcp::McpManager` (the session's host-side MCP manager, exposed as `SessionEngine.mcp_manager`: auth gating for built-in integrations, the `mcpServers` setting seam, and the `mcp.*` host-request handlers - `mcp.config`/`mcp.refresh`/optional `mcp.begin_login` - the kernel's generic MCP client resolves through), `mcp::McpManager::connection_roster` + `mcp::McpConnectionEntry` (the `/mcp` view's roster: every resolved integration with its connected state, display kind, transport, and generic-listable flag), `mcp::McpManager::service_catalog_views` + `service_catalog_diagnostics` (the `/mcp` view's resolved service-catalog cards and their diagnostics, answered from local state - the TS `buildServiceCatalogViews` parity; the picker never round-trips the kernel), `mcp::McpManager::api_key_credential_views` + `mcp::API_KEY_CREDENTIALS`/`McpCredentialView` (the `/mcp` view's api-key credential rows - the stored keys the surface manages alongside the connections, e.g. the web-search key - configured from the shared auth store), `models` (the registry and resolver, plus `models::order_for_picker`: the picker-catalog order the composition root applies to its snapshot), `session_engine::provider_adapter` (`switchable_stream_fn`/`ProviderTarget`: the live provider-target slot a host swaps on `set_model`; the adapter bridges the loop's abort into pa-ai's `StreamOptions::signal` cancellation token, so a turn abort cancels the in-flight HTTP fetch at the transport — closed by `ModelStream::close` and the stream's drop), `session_engine::ipython_state` (the post-compaction `ipython_state` kernel-persistence notice: the `CompactionKernelProbe` view of the kernel provisioner the engine wiring binds via `AgentSession::set_kernel_state_probe`, and the notice row that rides `CompactRun` for the surfaces to broadcast). `session_engine::session_commands` (the `/compact`/`/goal` executor: the durable echo/result rows with their persistence order - the echo ahead of the command so its own work sees it - the compaction and goal arms, and the post-execution live-context rebuild that keeps the rows in the agent state; hosts own the wire surface). `session_engine::goal_boundary` (the goal arms of the natural turn end on `SessionEngine`: the `--goal` construction seed with its next-turn context row - TS `_pendingNextTurnMessages` on `AgentSession` - the usage accounting, the budget-limit steer mint, the natural continuation mint with its slot rollback, the RLM deferral behind unsettled descendant work (mark/drop/take owed, the wake-up ownership probe, and the latest settled turn the post-settle deliveries decide from), and the terminal-error finish; the transports - the print driver's in-loop hook, the ACP settle loop, the daemon worker's queue - drive the one goal driver through these methods). `SessionEngine::expand_skill_submission` + `AgentSession`'s prompt-path expansion (the `/skill:<name>` submission seam, TS `_expandSkillCommand`: `skills::expand_skill_command` builds the `<skill ...>` block over the shared `pa_types::skill_blocks` parse, the accepted-turn row and the admitted user message agree on the expanded text, and the `skill_use_count` session counter counts at the admission). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only).

## Depends on
pa-types, pa-ai, pa-agent (one-way).

The Unix platform process API exposes `current_user_id()` for the daemon
endpoint namespace to match the TypeScript `process.getuid()` contract on
macOS as well as Linux. This reads the OS identity without filesystem or
subprocess work.

## Plugin host foundation

`plugins::PluginHost` is a session-local deep module with three lifecycle operations:
`reconcile(Vec<PluginSource>)`, `snapshot()`, and
`invoke(&Contribution, JSON, CancellationToken)`. This is a new public core interface
and embedded-VM dependency. It is not engine, kernel, prompt, settings, or TUI integration,
and it does not by itself activate any optional first-party plugin. A native test
consumer is provisional until those integration slices replace their current policies.

Each enabled source returns a declaration table with optional dense `exports`,
`commands`, and `hooks` arrays. Each entry has `name`, optional `description`, optional
`mode` (`unary` by default, or `streaming`), and a `run(input, ctx)` Lua function.
Exports map to `plugin.<plugin>.<name>` when a kernel adapter is added; commands map
to `/<name>`. Hooks are plugin-scoped named callbacks. The future engine adapter
must define stage inputs, outcomes, and ordering; this host does not invent stage
policy or claim context/compaction/provider contributions are implemented.
Unknown declaration fields and unsupported modes are errors, not ignored stubs.
Names are lowercase ASCII identifiers, at most 64 bytes. Duplicate plugins,
per-kind callbacks, and global command names are rejected. The composition root
supplies the reserved command inventory. No optional plugin command is
permanently reserved by the host.

Reconcile loads changed sources outside the registry lock, validates the whole
inventory, then commits once. Any failure preserves the old snapshot and live Lua
state. Concurrent reconciles cannot overwrite a newer commit. Unchanged sources
retain their generation. Successful replacements and omitted (disabled) plugins
retire previous generations, remove every callback registration, and cancel work.
The host has no persistence or data-deletion capability: disabling never deletes
saved plugin data. Each generation owns one VM; hosts do not share mutable Lua
state. Snapshot descriptors contain opaque generation IDs and cannot retarget a
replacement or cross to another host/session. No global provider registry changes.

`invoke` returns a native asynchronous `Invocation`. Its `next()` produces bounded,
backpressured JSON updates and one typed terminal `Unary`/`Streamed` outcome or error.
Unary callbacks cannot emit updates. Cancellation, consumer drop, host drop, disable,
and reload drop pending capability futures. Delivery checks retirement/cancellation
again, so buffered predecessor results cannot regain authority. Pure Lua callbacks
yield cooperatively at instruction intervals, allowing native cancellation to run.
Cancelled/retired work is never treated as a fallback candidate.

A callback receives only invocation-scoped `ctx.call(operation, JSON)` and
`ctx.emit(JSON)`. `HostCapabilities` is a dynamically supplied async adapter for
explicit host operations, including future auxiliary inference. Requests carry the
session, plugin, and generation identity. Adapters must authorize operations, honor
cancellation, and use explicit private runtime paths. They must not block while
constructing or polling their future. Registry locks are not held during callbacks;
mlua VM locks release on each pending coroutine poll. Retained Lua contexts lose
capability authority when their originating invocation ends. There are no default
filesystem, network, credential, storage, or inference-provider adapters.

The reviewed dependency is `mlua` 0.11.6 (MIT), with `lua54`, `vendored`, `async`,
`send`, and `serialize`: an actual embedded Lua VM plus coroutine/JSON support that
existing workspace crates cannot supply. Vendored Lua avoids ambient system-Lua ABI
selection. Lua values and functions stay private to generation/runtime files.
Only table/string/math/UTF-8 standard libraries are loaded; ambient OS, IO, package
loading, and console output are absent. Local plugins are trusted code, not a
hostile-code sandbox. A 16 MiB VM allocation limit, 1 MiB source limit, callback
inventory limits, and initialization instruction budget bound common mistakes;
these are not a complete security/resource isolation promise.

Future contribution forms should add an explicit declaration validator, typed
inventory descriptor, and generation-bound invocation adapter together. They must
use this same retirement and capability lifecycle, not add a parallel registry or
leak Lua values to the engine. Context strategy selection and ordered compaction
policy remain separate from this generic callback host.

Session persistence embeddings use `session::manager::SessionEntryWriter` through
`SessionManager::set_entry_writer`. Each complete entry envelope goes to the
embedding's durable log before the manager publishes it. Native engine event
persistence runs before agent event delivery; metadata uses the same append path.
The engine has no session ownership, adoption, epoch, acknowledgement, or revision
protocol. `session_engine::runtime::SessionRuntime` remains the kernel host runtime.
