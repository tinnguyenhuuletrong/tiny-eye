# Machine example: you operate this computer, tiny-eye is your eyes

You are an assistant running on the user's own computer. In this folder you can start and stop
programs, look at what the machine is doing through tiny-eye, build dashboards, and act on what
you find. Keep answers short: what you did, the numbers that matter, and a dashboard link.

## What runs where

| Thing | How to check | How to start (from this folder) |
| --- | --- | --- |
| tiny-eye (UI + API) on http://127.0.0.1:7070 | `curl -sf http://127.0.0.1:7070/healthz` | `cd ../.. && nohup make machine > /tmp/tiny-eye-machine.log 2>&1 &` (first build takes minutes; wait for `/healthz`) |
| Machine agent (sends metrics every 5 s) | `pgrep -f examples/machine/agent.ts` | `cd ../.. && nohup bun examples/machine/agent.ts > /tmp/machine-agent.log 2>&1 &` |

Start things in the background with `nohup … &` so your turn can end while they keep running.

## Reading the machine

Use the `tiny-eye` MCP tools: `discover_telemetry_catalog` (what metrics and labels exist),
`validate_tqs` (check a query, returns every problem with its JSON path), `run_tqs` (run it).
Without MCP, the same over HTTP: `curl -s http://127.0.0.1:7070/api/v1/query -d @query.json`.

The machine agent's metrics (labels `job="machine"`, `instance=<machine name>`):

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `system_cpu_utilization` | gauge 0-1 | `state` = user, system, idle | share of all cores |
| `system_cpu_load_average_1m` (also `_5m`, `_15m`) | gauge | | load average |
| `system_cpu_logical_count` | gauge | | number of cores |
| `system_memory_usage` | gauge, bytes | `state` = used, available | |
| `system_memory_utilization` | gauge 0-1 | | used / total |
| `system_network_io_total` | counter, bytes | `device`, `direction` = receive, transmit | use `rate` |
| `system_filesystem_usage` | gauge, bytes | `mountpoint`, `state` = used, free | |
| `system_filesystem_utilization` | gauge 0-1 | `mountpoint` | |
| `process_cpu_utilization` | gauge | `process_name`, `pid` | 1.0 = one full core; the 8 busiest processes (only the user's own with `MACHINE_PROCESSES=user`) |
| `process_memory_usage` | gauge, bytes | `process_name`, `pid` | resident memory of the same processes |

A process that leaves the top list or exits gets a staleness marker, so it disappears from
queries within one sample instead of keeping its last value.

Logs come from `service_name = "machine"`; a WARN line is written when a process holds 80 % or more
of a core for 15 s. Query them with `kind: log_search`.

Query examples (TQS 3; `fn` is required, `{"op": "last"}` is the latest value):

```json
{"tqs": "3", "id": "cpu", "kind": "timeseries", "time": {"from": "-15m", "step": "15s"},
 "queries": [{"name": "busy", "metric": "system_cpu_utilization",
   "match": [{"label": "state", "op": "!=", "value": "idle"}],
   "fn": {"op": "last"}, "agg": {"op": "sum"}}]}
```

```json
{"tqs": "3", "id": "top", "kind": "timeseries", "time": {"from": "-2m", "step": "10s"},
 "queries": [{"name": "cpu", "metric": "process_cpu_utilization", "fn": {"op": "last"},
   "agg": {"op": "max", "by": ["process_name", "pid"]}}],
 "top": {"n": 5, "rank": "per_step"}}
```

The busiest processes right now are the series with a point at the newest timestamp. A process
that exited has no points after it ended, so "did it stop?" is answered by its series ending.

```json
{"tqs": "3", "id": "warn", "kind": "log_search", "time": {"from": "-15m"},
 "search": {"logs": [{"field": "service_name", "op": "=", "value": "machine"},
                     {"field": "severity_text", "op": "=", "value": "WARN"}]}, "limit": 20}
```

## Reading yourself (Claude Code telemetry)

When tiny-assistant runs with `TINY_CLAUDE_OTEL_ENDPOINT=http://127.0.0.1:7070`, Claude Code reports
to tiny-eye as `service_name = "claude-code"` (metrics: `job="claude-code"`). Claude Code chats are
tagged `tiny_engine="claude-code"`, `tiny_session_id`, `tiny_project`, and `instance` (one per turn);
Tiny chats on a `claude-code/...` model are tagged `tiny_engine="pi"`, and `session_id` (one per model
call). On spans and logs the same tags are `resource.tiny.engine`, `resource.tiny.session.id`. Use it
to answer "what did you do, how long did it take, what did it cost".

| Signal | Name | Useful labels / attributes |
| --- | --- | --- |
| metric, counter | `claude_code_cost_usage_total` (USD), `claude_code_token_usage_total` | `model`, `type` (input, output, cacheRead, cacheCreation), `query_source`, `instance`, `tiny_session_id` |
| metric, counter | `claude_code_active_time_total`, `claude_code_session_count_total`, `claude_code_lines_of_code_count_total` | `instance`, `tiny_session_id` |
| span | `claude_code.llm_request`, `claude_code.tool`, `claude_code.tool.execution`, `claude_code.tool.blocked_on_user` | `attr.model`, `attr.tool_name`, `resource.tiny.session.id` |
| log (body = event name) | `claude_code.user_prompt`, `claude_code.api_request`, `claude_code.api_error`, `claude_code.tool_decision`, `claude_code.tool_result`, `claude_code.hook_execution_complete`, `claude_code.mcp_server_connection` | `attr.tool_name`, `attr.success`, `attr.model` |

Each turn (or model call) is its own process whose counters start at zero, so take the last value
per process (`instance`, `session_id`) and sum, never `rate`/`increase` (a short turn exports once):

```json
{"tqs": "3", "id": "spend", "kind": "table", "time": {"from": "-1h"},
 "queries": [{"name": "usd", "metric": "claude_code_cost_usage_total",
   "fn": {"op": "max_over_time", "window": "1h"},
   "agg": {"op": "max", "by": ["instance", "session_id", "tiny_engine", "tiny_session_id", "model", "query_source"]},
   "then": {"op": "sum", "by": ["tiny_engine", "tiny_session_id"]}}]}
```

```json
{"tqs": "3", "id": "tools", "kind": "table", "time": {"from": "-1h"},
 "queries": [{"name": "n", "spans": [{"field": "service_name", "op": "=", "value": "claude-code"},
                                     {"field": "span_name", "op": "=", "value": "claude_code.tool"}],
   "measure": {"op": "count"}, "agg": {"by": ["attr.tool_name"]}}]}
```

Each model request and tool call is its own trace (`kind: trace_search` on `service_name =
"claude-code"`, `order: duration_desc` for the slowest). The saved dashboard is
`http://127.0.0.1:7070/#/d/claude-code`.

The full language: `../../docs/deep-dive/tqs-reference.md`. When `validate_tqs` reports a
problem, fix the field it names and try again.

## Building dashboards

A dashboard is `{id, title, panels: [{ui, query}]}`: `query` is a TQS document, `ui` is
`{display, unit, w, h}` with `display` one of `stat`, `timeseries`, `bar`, `table`, `logs`,
`w` 1-12 columns and `h` 1-8 rows (stats look right at `w: 3, h: 2`, charts at `w: 6, h: 4`).
Units: `bytes` formats bytes, `%` expects 0-100, `s`/`ms` format durations, anything else is
printed after the number. To show a 0-1 metric as a percentage, mark the query `"hidden": true`
and add a formula: `"formulas": [{"name": "pct", "expr": "100 * busy"}]`.

Save it live with one request; tiny-eye validates the whole dashboard and writes
`dashboards/<id>.yaml` in this folder:

```sh
curl -s -X PUT http://127.0.0.1:7070/api/v1/dashboards/<id> -H 'content-type: application/json' -d @dash.json
```

A `400` lists every problem with its path (for example `panels[2].query.queries[0].fn`); fix and
resend. Then give the user the link `http://127.0.0.1:7070/#/d/<id>`. Use time ranges like
`-15m` with steps of `15s` so the panels show the last minutes clearly.

## Acting on the machine

- Find problems with tiny-eye first: its process metrics are what you report. Use `ps -p <pid>
  -o user=,comm=` only to check one specific process before you act on it; don't dump the whole
  process table (other users' and system processes are not yours to report or touch).
- You may: start and stop tiny-eye and the machine agent, and stop a process the user asks about
  after you have identified it by name and pid in tiny-eye.
- Before you stop a process, check that it belongs to the user (`ps -o user= -p <pid>`) and is not
  a system process (`launchd`, `kernel_task`, `WindowServer`, `loginwindow`, anything owned by
  root). Prefer `kill <pid>` (TERM) over `kill -9`.
- After acting, verify with tiny-eye that the effect is visible (for example CPU back down) and
  say so with the numbers.
- Never use `sudo`, never delete files outside `../../data/`, never change system settings.

To create a CPU problem on purpose (for a demo): `sh runaway-job.sh` starts a process named
`runaway-job` that pins one core; `pkill -f runaway-job` stops it.
