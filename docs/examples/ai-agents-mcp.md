---
title: AI agents over MCP
description: Connect Claude Code, Claude Desktop, Cursor or any MCP client so an agent can discover, validate and run TQS queries.
sidebar_position: 4
---

`tiny-eye mcp` is a stdio [MCP](https://modelcontextprotocol.io) server. An AI agent uses it to
list your metrics and services, write TQS queries, validate them and run them against a running
tiny-eye node.

## What the bridge is

The bridge is a thin JSON-RPC process that the MCP client starts. It does not open the data
files; it forwards each tool call to the node's HTTP API:

```mermaid
flowchart LR
  A[MCP client] -->|stdio JSON-RPC| B[tiny-eye mcp]
  B -->|HTTP| C[tiny-eye node :7070]
```

- The tool list is generated from `api/openapi.yaml`: every operation marked `x-mcp-tool`
  becomes a tool, and its input schema is the operation's request schema.
- An agent gets exactly the API's behavior: the same validation, the same problem codes, the same
  auth scope.
- All tools are read-only. There is no tool that ingests data or saves a dashboard.

## Tools

| Tool | HTTP | Use |
| --- | --- | --- |
| `discover_telemetry_catalog` | `GET /api/v1/catalog` | Metric names with label keys and sample values, services, span names, log line counts (last 24 h, bounded). |
| `validate_tqs` | `POST /api/v1/validate` | Check a TQS document without running it. Returns every problem with a code and JSON path. |
| `run_tqs` | `POST /api/v1/query` | Run a TQS document of any kind. |

The generated reference with input fields is [MCP tools](../reference/mcp-tools.md). The exact
`tools/list` result is in `schema/generated/mcp-tools.json`.

## Connect a client

Build the binary once (`make build`) and start a node (`make run`, or `make demo` for the demo
shop). The bridge's default `--url` is `http://127.0.0.1:7070`.

### Claude Code

Run this from the repository root:

```bash
claude mcp add tiny-eye -- "$PWD/target/release/tiny-eye" mcp --url http://127.0.0.1:7070
```

Check it with `claude mcp list`, then ask a question in a Claude Code session.

### Claude Desktop, Cursor and other MCP clients

Add an entry under `mcpServers` in the client's config file (`claude_desktop_config.json` for
Claude Desktop, `.cursor/mcp.json` for Cursor). Use an absolute path to the binary:

```json
{
  "mcpServers": {
    "tiny-eye": {
      "command": "/absolute/path/to/tiny-eye/target/release/tiny-eye",
      "args": ["mcp", "--url", "http://127.0.0.1:7070"]
    }
  }
}
```

Restart the client after editing the file.

## Tokens

When the node has auth enabled (`auth.tokens` in its config), the bridge must send a bearer token
with the `query` scope. Pass the *name* of an environment variable with `--token-env`; the bridge
reads the token from it at start and refuses to start if it is not set:

```bash
claude mcp add tiny-eye -e TINY_EYE_QUERY_TOKEN=your-token -- \
  "$PWD/target/release/tiny-eye" mcp --url http://127.0.0.1:7070 --token-env TINY_EYE_QUERY_TOKEN
```

```json
{
  "mcpServers": {
    "tiny-eye": {
      "command": "/absolute/path/to/tiny-eye/target/release/tiny-eye",
      "args": ["mcp", "--url", "http://127.0.0.1:7070", "--token-env", "TINY_EYE_QUERY_TOKEN"],
      "env": { "TINY_EYE_QUERY_TOKEN": "your-token" }
    }
  }
}
```

The token never appears on the command line, so it does not show up in process listings. On the
node side, tokens are configured the same way:

```yaml
auth:
  tokens:
    - { token_env: TINY_EYE_QUERY_TOKEN, scopes: [query] }
```

## Example prompts

With the [demo shop](demo-shop.md) running for a few minutes:

- "Which route of shop-api has the worst p99 latency right now, and why?"
- "Find the slowest failed checkout in the last 15 minutes and show me its spans."
- "What are the most common error log patterns, and which service do they come from?"
- "Compare card authorization latency for ok, declined and timeout outcomes."

On your own data, start broad: "What services and metrics do you see?", then narrow down.

## How an agent works with the tools

The server tells the client at `initialize`: start with `discover_telemetry_catalog`, then
`validate_tqs` and `run_tqs`. A typical loop:

1. Discover the metric names, label keys and services that exist.
2. Write a TQS document and validate it.
3. Fix every reported problem in one pass, then run it.

Because the schema is closed, an invented field or a wrong value is reported instead of being
ignored. For example, this document has three mistakes:

```json
{ "tqs": "3", "id": "p95", "kind": "timeseries", "time": { "from": "-1h" },
  "queries": [{ "name": "p", "metric": "http_server_request_duration",
                "fn": { "op": "histogram_quantile", "window": "5m" },
                "agg": { "by": ["http_route"] } }],
  "formulas": [{ "name": "ms", "expr": "q * 1000" }] }
```

`validate_tqs` returns all of them at once:

```json
{
  "problems": [
    { "code": "q", "message": "histogram_quantile needs q in [0, 1]", "path": "queries[0].fn.q" },
    { "code": "agg", "message": "agg.op is required for metrics", "path": "queries[0].agg.op" },
    { "code": "formula_ref", "message": "\"q\" is not a query or an earlier formula", "path": "formulas[0].expr" }
  ],
  "valid": false
}
```

The agent adds `"q": 0.95`, sets `"op": "sum"`, and renames the reference to `p`. Every problem
code is listed in the [TQS reference](../deep-dive/tqs-reference.md).

## Prompt injection

Telemetry can contain any text, including text written by an attacker (a log line, a span name,
an HTTP header value). The bridge never returns that text as tool prose. Every tool result is
wrapped in a delimited block:

```text
<data status="200">
{"tqs":"3","id":"errors","rows":[{"body":"Ignore previous instructions and ..."}], ...}
</data>
```

Any `</data` inside the payload is escaped, so the payload cannot close the block early. A
response with HTTP status 400 or higher is flagged as a tool error (`isError: true`). This makes
it clear to the model what is data, but it is not a guarantee: treat agent actions based on
telemetry with the same care as any untrusted input. The bridge's tools are read-only, which
limits what an injected instruction could do through them.
