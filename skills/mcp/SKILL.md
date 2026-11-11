---
name: mcp
description: Use external MCP servers configured in `mcpServers` generically from Python - inspect the configured connections, discover live tool schemas, and call tools without per-service packages.
---

# MCP Services

External services reachable over MCP use the same pre-imported `mcp` module.
Servers come from the user's `mcpServers` setting and the built-in Linear and
Notion integrations; there are no per-service skills to read.

## Connections

`connectionId` is the dispatch id everywhere below. The session prompt lists
the enabled generic servers; the built-in integrations are `linear` and
`notion`. Ask the user to run `wv mcp add` and `/mcp login <server>` to
configure another one.

## Recommend, never connect

If a task would benefit from an unconfigured server, name it (for example
"Connecting Notion would let me search your workspace directly") and tell the
user to run `wv mcp add` and `/mcp login <server>`. Never install anything,
open a browser, use credentials, or request extra scopes from this module.

## Discover, then call

```python
tools = await mcp.search_tools("search workspace documents", connection_id="notion")
tool = await mcp.describe_tool("notion", "notion-search")
tools = await mcp.list_tools("notion")

result = await mcp.call_tool("notion", "notion-search", {"query": "roadmap"})
```

- Discover before calling: don't assume tool names or arguments; read the
  `inputSchema` with `describe_tool` and construct matching arguments.
- `list_tools` returns the complete inventory or raises; never a silent
  partial list. Returned schemas are isolated copies; mutate them freely.
- `search_tools` searches one connection. It reports `searched` and
  `truncated` (narrow the query or raise the limit when it is set); its errors
  propagate. Search each connection separately when you need several.
- Results are already-parsed Python (a `dict` for structured output, otherwise
  a string). No `json.loads` needed.

## Errors and permissions

- `PermissionError`: the tool is excluded by the user's `enabledTools` /
  `disabledTools` policy. Don't retry; tell the user.
- `KeyError`: the tool or connection doesn't exist. Re-check the server name
  and `list_tools`; tools can change across sessions.
- `McpDiscoveryError`: the server's tool listing couldn't complete honestly
  (broken pagination). No partial inventory is published; re-run, and ask the
  user to reconnect if it persists.
- `RuntimeError`: credentials are not available (`McpCredentialsUnavailable`:
  ask the user to connect via `/mcp login <server>`, never via environment
  variables), a refresh failed, or the host bridge is unavailable.
- `McpToolError`: the server flagged the call as an error. Surface the
  message; don't blindly retry a write that may have already happened.

Credentials never appear in tool schemas or errors.
