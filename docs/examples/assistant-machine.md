---
title: An assistant that watches and controls your machine
description: "tiny-eye as the eyes of an AI assistant on your computer, with tiny-assistant or Claude Code: start collecting, build a dashboard, find and stop a runaway process."
sidebar_position: 6
---

`examples/machine` makes tiny-eye the eyes of an AI assistant running on your own computer. A
small agent sends the machine's CPU, memory, load, network, disk and busiest processes to tiny-eye.
An assistant opened in that folder reads its `CLAUDE.md` (what it may do, the metric names, query
examples, how to save a dashboard) and gets tiny-eye's MCP tools from `.mcp.json`.

Below, [tiny-assistant](https://github.com/tinnguyenhuuletrong/tiny-assistant), a self-hosted
personal assistant, runs a Claude Code chat in that folder (left) while tiny-eye shows the machine
(right). Waiting for the model is fast-forwarded.

![tiny-assistant starts the machine agent, builds a dashboard in tiny-eye, then finds and stops runaway processes](../assets/assistant-machine.gif)

## Run it

```bash
make machine          # tiny-eye on http://127.0.0.1:7070 with examples/machine/tiny-eye.yaml
```

Open an assistant in `examples/machine`:

- **tiny-assistant:** add `examples/machine` as a project and start a chat with the **Claude Code**
  engine. Claude Code chats load the folder's `CLAUDE.md`, `.mcp.json` and `.claude/settings.json`.
- **Claude Code:** `cd examples/machine && claude`.

Then ask:

1. "Start watching this Mac with tiny-eye, then build me a dashboard for it: CPU, memory, load,
   network, disk and the busiest processes."
2. Run `make machine-hog` a few times, then ask: "My fan just got loud. Use tiny-eye to find what
   is eating the CPU, and stop it."

What happens:

| Step | The assistant | tiny-eye |
| --- | --- | --- |
| Start watching | checks `/healthz`, starts `examples/machine/agent.ts` in the background | receives OTLP metrics every 5 s and an INFO log line |
| Build a dashboard | lists metrics with `discover_telemetry_catalog`, checks each panel with `validate_tqs`, saves the dashboard with `PUT /api/v1/dashboards/<id>` | validates the whole dashboard, writes `examples/machine/dashboards/<id>.yaml`, serves it at `/#/d/<id>` |
| Find the problem | ranks `process_cpu_utilization` with `top` through `run_tqs`, reads the WARN logs | answers from the hot tail, so the last seconds are already there |
| Fix it | checks the process belongs to the user, stops it with `kill` | shows CPU back down a few samples later; the assistant confirms with another query |

## Watch the assistant too

The machine agent shows what the computer does; Claude Code's own OpenTelemetry shows what the
assistant does on it. In tiny-assistant's `.env`:

```bash
TINY_CLAUDE_OTEL_ENDPOINT=http://127.0.0.1:7070
# TINY_CLAUDE_OTEL_CONTENT=1   # also prompts, responses, Bash commands and tool output
```

After a daemon restart, Claude Code chats and Tiny chats on a `claude-code/...` model send their
metrics, events and traces to tiny-eye, tagged with the engine (`tiny_engine`), tiny session
(`tiny_session_id`) and project (`tiny_project`). The `claude-code` dashboard
(`/#/d/claude-code`) shows:

| Panel | From |
| --- | --- |
| Cost, tokens (1 h), by type, by engine / session / model | `claude_code_cost_usage_total`, `claude_code_token_usage_total` |
| Tool calls by tool, model request p95, slowest steps | spans `claude_code.tool` (`attr.tool_name`), `claude_code.llm_request` |
| What Claude Code did | events `claude_code.user_prompt`, `tool_result`, `api_request`, `api_error` |

The assistant can read it too (`CLAUDE.md`, "Reading yourself"). Every turn is its own `claude`
process, so its counters are summed as the last value per process (`max_over_time` per
`instance`), not with `rate`; tiny-assistant forces cumulative temporality, since tiny-eye does not
store OTLP delta sums. With Claude Code 2.1.330, a Claude Code chat's model requests and tool
calls arrive as separate traces (no `claude_code.interaction` root span).

## The pieces

| File | Role |
| --- | --- |
| `agent.ts` | The machine agent (Bun, TypeScript). OTLP/HTTP JSON, no runtime dependencies, macOS and Linux. `MACHINE_NAME`, `MACHINE_INTERVAL`, `MACHINE_TOP`, `MACHINE_PROCESSES=user` |
| `CLAUDE.md` | The assistant's contract: what runs where, the metric table, query examples, dashboard format, and rules for acting on the machine (no `sudo`, only the user's own processes) |
| `.mcp.json`, `tiny-eye-mcp.sh` | Starts `tiny-eye mcp` against the local node; `.claude/settings.json` enables it without a prompt |
| `tiny-eye.yaml` | Data under `./data/machine`; dashboards from `examples/machine/dashboards` |
| `runaway-job.sh` | A process named `runaway-job` that pins one core (`make machine-hog`) |

The metrics follow the OpenTelemetry system conventions after tiny-eye's name translation:
`system_cpu_utilization{state}`, `system_cpu_load_average_1m`, `system_memory_usage{state}`,
`system_network_io_total{device,direction}`, `system_filesystem_usage{mountpoint,state}`,
`process_cpu_utilization{process_name,pid}` and more; the full table is in
`examples/machine/CLAUDE.md`.

:::caution[It really controls the machine]
tiny-assistant runs Claude Code chats with permissions bypassed by default, so `CLAUDE.md` is what
keeps the assistant in bounds. Read it, and tighten it, before you point an assistant at a machine
you care about.
:::

## Why this works well with tiny-eye

- **The assistant writes the dashboard as data.** A dashboard is a TQS document plus layout, so an
  agent can create it, and tiny-eye refuses anything invalid with every problem and its JSON path,
  which the agent fixes on the next try. The result is a YAML file in git, not clicks in a UI.
- **Questions are answered from fresh data.** Writes are queryable as soon as they are
  acknowledged, so "did CPU go down after I stopped it?" has an answer seconds later.
- **The tools are closed and read-only.** MCP gives the agent discovery, validation and queries;
  saving a dashboard goes through the same validated API as the web UI.

More on the MCP side: [AI agents over MCP](ai-agents-mcp.md). Re-recording the video:
`examples/machine/README.md`.
