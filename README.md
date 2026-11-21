# tiny-eye

A blob-first observability engine in one Rust binary. Metrics, logs and traces are stored as
sorted ZSTD Parquet in object storage (local disk or S3), tracked by a DuckLake catalog, and
queried with **TQS 3**, a closed JSON query schema compiled to DuckDB SQL that reproduces
Prometheus semantics point for point. Built from [the specification](doc/spec/Observability%20Engine%20Spec.md)
and its [research evidence](doc/research/).

```
remote-write / OTLP-HTTP / Loki push ─► WAL (fsync) ─► hot tail (in-memory DuckDB) ─► ack
                                                          │ every 60 s / 64 MB
                                                          ▼
                       sorted Parquet (bloom, ZSTD) ─► object storage ─► envelope (If-None-Match)
                                                                           │ every 5 s
TQS JSON ─► validate ─► compile ─► DuckDB:  lake ∪ hot rows above watermark ◄─ checkpoint → DuckLake
                                                                           hourly compaction, retention
```

## Quick start

```bash
make tools      # pinned Prometheus 3.16 + DuckDB 2.0.1 extensions into .tools/ (once)
make run        # single node on 127.0.0.1:7070, data under ./data (config/dev.yaml)
```

```bash
# write: any Prometheus agent (remote_write url http://127.0.0.1:7070/api/v1/write),
# an OTel Collector (otlphttp endpoint http://127.0.0.1:7070), or Loki clients (/loki/api/v1/push)

# read: TQS
curl -s localhost:7070/api/v1/query -d '{"tqs":"3","id":"rps","kind":"timeseries",
  "time":{"from":"-1h","step":"1m"},
  "queries":[{"name":"r","metric":"http_requests_total","fn":{"op":"rate","window":"5m"},"agg":{"op":"sum","by":["job"]}}]}'
```

Agents: `tiny-eye mcp --url http://127.0.0.1:7070` (stdio MCP). Tools: [doc/schema/mcp-tools.md](doc/schema/mcp-tools.md).
More query examples: [api/examples](api/examples). Language reference: SPEC §8.

## Principles

1. **Keep it simple**: three crates (`tqs`, `engine`, `server`), one binary, one process for single-node mode.
2. **One source of truth, generated everywhere else**:

   | Definition | Generates |
   | --- | --- |
   | [`api/openapi.yaml`](api/openapi.yaml) | axum router (operationId → handler; a missing one is a compile error), response types, auth scope per route (`x-scope`), MCP tools (`x-mcp-tool`) |
   | [`schema/tables.yaml`](schema/tables.yaml) | Arrow schemas, typed row builders, DuckLake DDL, hot-tail DDL, partition expressions |
   | [`config/default.yaml`](config/default.yaml) | every default (embedded); user YAML and `TINY_EYE__SECTION__KEY` env vars override |
   | [`dashboards/*.yaml`](dashboards) | dashboards, as TQS panels in git |
   | sources above + code | [`doc/schema/`](doc/schema): JSON Schemas for TQS, dashboards and config (VS Code / Monaco), MCP tool docs, formula grammar (`make schemas`, checked by a test) |

3. **Configuration-driven**: sizes, intervals, durability, retention, storage, auth come from config.
4. **Proven, not claimed**: each rule has a test that can fail; results are in [doc/results](doc/results).

## Evidence

| What | How | Result |
| --- | --- | --- |
| TQS = Prometheus | `make conformance`: 33 metric functions, 11 real-panel shapes over 3 namespaces, spans/logs/traces vs an independent computation, 38 validation/injection cases, 15 mutants | [104/104, 15/15 mutants caught](doc/results/tqs-coverage.md) |
| The whole binary = Prometheus | `make e2e`: same remote-written samples into Prometheus 3.16 and tiny-eye; data spread over L1, L0, a pending envelope and RAM | [20/20 panels equal](doc/results/e2e.md) |
| Storage assumptions on this machine | `make poc`: DuckLake registration + schema refusal, hot tail counted once, atomic compaction swap, retention deletes objects, bloom per row group, Arrow appender types | 6/6 |
| Durability + overload | `crates/engine/tests/node.rs`: crash without flush loses no acknowledged sample (WAL replay); arena limit → 429 | pass |
| Contracts in sync | `crates/server/tests/{openapi,schemas}.rs`: TQS JSON Schema ⇔ Rust validator on every example; generated docs current; config/dashboards valid | pass |
| Performance (8-core laptop, 2,620 series × 6 h, 3.7 M samples) | `make bench` / `make bench-s3` | remote-write **1.65 M samples/s**; flush 0.6 s; 6 h panels: `sum by (rate)` 0.58 s, histogram p99 73 ms, gauge 100 ms, one series 21 ms; trace by id 11 ms; absent trace id **22 → 2 S3 GETs** after compaction ([local](doc/results/bench-local.md), [MinIO](doc/results/bench-s3.md)) |

## Commands

`make help`. Daily: `make fmt lint test`. Before a PR: `make check`. Hooks: `make hooks`.

## Status against the spec

Built (single node, SPEC M1 + most of M2):

- Ingest: Prometheus remote-write 1.0, OTLP/HTTP metrics/traces/logs (protobuf + JSON, gzip), Loki push (JSON); sample-age bounds; secret attribute redaction; arena limit with 429
- Durability `wal` (group-committed fsync, replay on restart) and `memory`
- Flush per (table, hour), sorted, ZSTD, page stats, delta / byte-stream-split encodings, `trace_id` bloom; envelopes with create-only PUT
- Checkpointer (idempotent via `applied_envelopes`), hot tail with per-node watermark, hourly L0→L1 compaction with `positive_delta`, series dedup, retention through DuckLake
- TQS 3: every construct of SPEC §8 incl. Prometheus staleness; HTTP API, validation with every problem code; catalog discovery; YAML dashboards; MCP stdio bridge; bearer-token scopes; self-metrics

Not built yet (by milestone):

- M3 cluster: roles, lease + fencing, hot-tail RPC, `replicated` durability; OTLP gRPC; Loki protobuf push
- M4: alert evaluator + Alertmanager, materialized panels, Grafana data source or UI (Q11)
- Laptop snapshot export (E9), multi-region buckets, native histograms, `traces_index`

Decisions taken where the spec left a question open use its proposed defaults (Q1–Q20), except
**Q7 durability**: single node defaults to `wal`.

## Known deviations and findings

- DuckDB core must be a release build in every profile; its DEBUG build aborts when release-built extensions free memory (found by the PoC; see `Cargo.toml`).
- DuckLake needs the `icu` extension in addition to `ducklake`/`httpfs` (snapshot functions use TIMESTAMPTZ math).
- DuckDB 2.0.1: `epoch_ms(TIMESTAMP_NS) // n` raises a spurious cast error; `epoch_ns` is used instead.
- Result `labels` are parsed from the series key; a label value containing `,` is split ambiguously (the `key` itself is exact).
- OTLP delta temporality and exponential histograms are counted and skipped.
