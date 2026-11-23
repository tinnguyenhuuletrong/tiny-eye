# Machine example: an AI assistant that watches and controls this computer

This folder turns tiny-eye into the eyes of an AI assistant running on your machine. A small agent
sends the computer's CPU, memory, load, network, disk and busiest processes to tiny-eye; an
assistant opened in this folder (Claude Code, or a Claude Code chat in
[tiny-assistant](https://github.com/tinnguyenhuuletrong/tiny-assistant)) reads `CLAUDE.md`, gets
tiny-eye's MCP tools from `.mcp.json`, and can then start things, build dashboards, find problems
and fix them.

![tiny-assistant starts the machine agent, builds a dashboard in tiny-eye, then finds and stops runaway processes](../../docs/assets/assistant-machine.gif)

| File | What it is |
| --- | --- |
| `agent.ts` | The machine agent: OTLP/HTTP JSON every 5 s, no runtime dependencies (Bun 1.x, macOS or Linux) |
| `tiny-eye.yaml` | tiny-eye config: data under `./data/machine`, dashboards in `dashboards/` |
| `CLAUDE.md` | What the assistant may do here, the metric names, query examples, how to save a dashboard |
| `.mcp.json`, `tiny-eye-mcp.sh` | tiny-eye's MCP bridge for an assistant started in this folder (`.claude/settings.json` enables it) |
| `runaway-job.sh` | Starts a process that pins one CPU core, for the assistant to find |
| `dashboards/` | Dashboards the assistant saves (tiny-eye writes them here). `this-mac.yaml` is the one it built in the video |

## Run it

From the repository root:

```bash
make machine          # terminal 1: tiny-eye on http://127.0.0.1:7070 with this config
```

Then open an assistant in `examples/machine`:

- **tiny-assistant:** add `examples/machine` as a project, start a chat with the **Claude Code** engine.
- **Claude Code:** `cd examples/machine && claude`.

Ask, for example:

1. "Start watching this Mac with tiny-eye, then build me a dashboard for it: CPU, memory, load,
   network, disk and the busiest processes."
2. Run `make machine-hog` (three times for a bigger spike), then: "My fan just got loud. Use
   tiny-eye to find what is eating the CPU, and stop it."

The assistant starts the agent itself if it is not running (`make machine-agent` does the same by
hand), saves the dashboard with `PUT /api/v1/dashboards/<id>` (tiny-eye validates it and writes
`dashboards/<id>.yaml`), and checks with tiny-eye that CPU went back down after stopping the process.

## Metrics

All carry `job="machine"` and `instance=<machine name>`. Names follow the OpenTelemetry system
conventions after tiny-eye's translation (dots become underscores, counters get `_total`):
`system_cpu_utilization{state}`, `system_cpu_load_average_1m|5m|15m`, `system_cpu_logical_count`,
`system_memory_usage{state}`, `system_memory_utilization`, `system_network_io_total{device,direction}`,
`system_filesystem_usage{mountpoint,state}`, `system_filesystem_utilization{mountpoint}`,
`process_cpu_utilization{process_name,pid}`, `process_memory_usage{process_name,pid}`. A WARN log
(`service_name="machine"`) is written when a process holds 80 % of a core for 15 s.

| Env | Default | |
| --- | --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://127.0.0.1:7070` | where to send |
| `MACHINE_NAME` | the hostname | the `instance` label |
| `MACHINE_INTERVAL` | `5` | seconds between samples |
| `MACHINE_TOP` | `8` | how many of the busiest processes to report |
| `MACHINE_PROCESSES` | `all` | `user`: only processes of the user running the agent |

## What the assistant may do

`CLAUDE.md` is the contract: start and stop tiny-eye and the agent, inspect processes, and stop a
process only after identifying it and checking it belongs to the user and is not a system process;
never `sudo`, never delete files outside `data/`. Claude Code chats in tiny-assistant run with
permissions bypassed by default, so these instructions are what keeps the assistant in bounds:
read them before you point it at a machine you care about.

## Re-record the video

With tiny-eye (`make machine`) and a tiny-assistant daemon running, `examples/machine` added as a
project, and no machine agent running yet:

```bash
TINY_ASSISTANT_URL=http://127.0.0.1:7777 node docs/assets/media/record.mjs assistant-machine
```

The recording used `MACHINE_NAME=my-mac MACHINE_PROCESSES=user` in the daemon's environment and an
empty tiny-assistant data directory, so no host name, other users' processes or past chats show.
