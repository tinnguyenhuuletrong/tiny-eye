---
title: Security
description: Bearer tokens and scopes, injection safety, request and query limits, ingest redaction, MCP prompt-injection handling, and what is not built yet.
sidebar_position: 11
---

This page describes the security controls tiny-eye implements today and the gaps an operator must cover in front of it.

## Bearer tokens and scopes

When `auth.tokens` is empty (the default), auth is **disabled** and every route is open. This is meant for local development only.

When at least one token is configured, every request is checked by the `auth` middleware in `crates/server/src/lib.rs`:

1. The required scope is looked up in the generated `OPERATIONS` table, which comes from `x-scope` on each operation in `api/openapi.yaml`.
2. `public` operations pass without a token.
3. Otherwise the request needs `Authorization: Bearer <token>`, and the token must grant the required scope or `admin`.
4. Anything else gets `401` with the plain-text body `missing or insufficient bearer token`.

| Scope | Operations |
| :--- | :--- |
| `public` | `GET /healthz`, `GET /metrics`, `GET /openapi.yaml`, and the web UI's static files |
| `ingest` | `POST /api/v1/write`, `POST /v1/metrics`, `POST /v1/traces`, `POST /v1/logs`, `POST /loki/api/v1/push` |
| `query` | `POST /api/v1/query`, `POST /api/v1/validate`, `GET /api/v1/catalog`, `GET /api/v1/dashboards`, `GET /api/v1/dashboards/{id}` |
| `admin` | `PUT /api/v1/dashboards/{id}`, and any request that matches no operation |

Rules:

- **`admin` grants every scope.**
- **HEAD needs GET's scope**, since GET handlers serve HEAD.
- **Unknown API routes fail closed.** A method or path that is not in the spec needs `admin`, so a non-admin token gets `401` rather than a hint about which routes exist. Paths under `/api/`, `/v1/` and `/loki/` are never treated as UI paths.
- **UI static files are public.** A GET for any other path (the UI shell, `/assets/*`, client-side routes) needs no token: those files hold no data. Every data call the UI makes goes to the API and needs a token. A non-GET request to a UI path needs `admin`.
- `/metrics` is public. It exposes counters (rows, flushes, queries) but no telemetry content; restrict it at the proxy if that matters to you.

### Configuring tokens

Tokens never live in the configuration file. Each entry names an environment variable:

```yaml
auth:
  tokens:
    - { token_env: TINY_EYE_INGEST_TOKEN, scopes: [ingest] }
    - { token_env: TINY_EYE_QUERY_TOKEN,  scopes: [query] }
    - { token_env: TINY_EYE_ADMIN_TOKEN,  scopes: [admin] }
```

```bash
export TINY_EYE_INGEST_TOKEN=$(openssl rand -hex 32)
export TINY_EYE_QUERY_TOKEN=$(openssl rand -hex 32)
export TINY_EYE_ADMIN_TOKEN=$(openssl rand -hex 32)
tiny-eye --config prod.yaml
```

If a named variable is not set, the node refuses to start (`auth token env ... is not set`). Tokens are loaded once at start; rotating a token means a restart. The MCP bridge reads its token the same way: `tiny-eye mcp --url ... --token-env TINY_EYE_QUERY_TOKEN`.

Give each client the narrowest scope: agents and dashboards `query`, collectors `ingest`, and only people who edit dashboards `admin`.

## Injection safety

TQS is designed so that a document cannot change the SQL it compiles to.

- **Bound parameters.** Every string taken from a query (matcher values, condition values, attribute keys, regexes, `derive` replacements, variable values, trace ids) is a bound DuckDB parameter (`$n`). The SQL text contains only fixed SQL, validated identifiers and numbers the validator produced (epoch milliseconds, durations).
- **Validated identifiers.** Label names must match `^[a-zA-Z_][a-zA-Z0-9_]*$`, metric names `^[a-zA-Z_:][a-zA-Z0-9_:]*$`, query and formula names `^[a-z][a-z0-9_]{0,31}$`. Span and log fields are checked against fixed lists; `attr.*` and `resource.*` keys against a restricted grammar and then bound.
- **Formulas are an AST.** Formula expressions are parsed with a closed grammar (numbers, names, `+ - * /`, parentheses, six functions). SQL is generated from the AST; the expression text never reaches SQL.
- **Closed model.** Unknown fields are refused, so an option cannot be smuggled in.
- **Regexes are RE2** (validated with the Rust `regex` crate, run by DuckDB's RE2), with no backtracking blow-up, and limited to 256 characters.

The conformance suite includes cases A1 and A2: SQL-looking matcher values and search text are bound as data and match nothing; the logs table is unchanged. See [TQS coverage](evidence/tqs-coverage.md).

Dashboard ids double as file names; they must match `^[a-z0-9][a-z0-9_-]{0,63}$`, so a save can never write outside the dashboards directory.

## Limits

| Limit | Value | Where |
| :--- | :--- | :--- |
| Request body | 64 MB (larger bodies get `413`) | HTTP layer |
| Decompressed body | 256 MB (gzip and snappy; snappy's declared length is checked before allocating) | ingest receivers |
| Regex length | 256 characters | TQS validator |
| Points per series | 11,000 | TQS validator |
| Queries per document | 10 | TQS validator |
| `limit` on list kinds | 5,000 (default 100) | TQS validator |
| `in` values | 100 | TQS validator |
| Formula | 512 characters, 128 nodes | formula parser |
| Variable value | 256 characters | TQS validator |
| Query time | `query.timeout` (default 30 s), then the query is interrupted | node |
| Parallel queries | `query.workers` (default 4) connections | node |
| Ingest buffer | `ingest.arena_limit` (default 1 GB), then `429` | node |
| DuckDB memory | `duckdb.memory_limit` (default 4 GB) | DuckDB |

The full list with problem codes is in the [TQS reference](tqs-reference.md#limits). A per-query bytes-scanned limit is not built.

## Ingest redaction of secret attributes

At ingest, attribute values whose key looks like a secret are replaced by `<redacted>` before anything is stored. The check is case-insensitive and matches when the key equals, or ends with `.<word>` or `_<word>` for, one of:

`authorization`, `cookie`, `set-cookie`, `password`, `passwd`, `token`, `api_key`, `apikey`, `secret`

So `http.authorization`, `db_password` and `Cookie` are redacted. It applies to OTLP resource, span and log attributes (and therefore to metric labels derived from OTLP attributes), and to Loki stream labels and structured metadata. Prometheus remote-write labels and log **bodies** are not redacted. Keep secrets out of log lines at the source.

## MCP and prompt injection

`tiny-eye mcp` is a stdio bridge: each tool call is forwarded to the operation's HTTP endpoint with the bridge's token, so an agent gets exactly the API's validation and the token's scope. The three generated tools (`discover_telemetry_catalog`, `run_tqs`, `validate_tqs`) need only `query`; give the bridge a `query` token.

Telemetry may contain text written by anyone (log lines, span names, attribute values). To limit prompt injection, the bridge never returns telemetry as tool prose. Every result is wrapped in a delimited data block:

```text
<data status="200">
{ ...the HTTP response body... }
</data>
```

Any `</data` inside the body is escaped as `<\/data`, so telemetry cannot close the block early. Results with an HTTP status of 400 or more are flagged `isError: true`. Agents should still treat everything inside the block as data, not instructions. See [AI agents and MCP](../examples/ai-agents-mcp.md).

## Error bodies

| Situation | Status | Body |
| :--- | :--- | :--- |
| Invalid TQS document or dashboard | 400 | JSON `{"valid": false, "problems": [{code, path, message}]}` |
| Undecodable ingest payload | 400 | plain text with the decoder's error |
| Missing or insufficient token | 401 | `missing or insufficient bearer token` |
| Unknown dashboard | 404 | `unknown dashboard` |
| Unknown API route (with sufficient token) | 404 | `no such API route; see GET /openapi.yaml` |
| Body over 64 MB | 413 | from the HTTP layer |
| Ingest arena full | 429 | `ingest arena full; retry later` |
| Query failure or timeout, internal error | 500 | plain text with the engine's error message |

:::warning
`500` responses carry the engine's error text, which can include file paths and SQL details. That helps locally; put a reverse proxy in front before exposing the API.
:::

## What is not built

- **TLS.** The server speaks plain HTTP. Terminate TLS at a reverse proxy or load balancer.
- **mTLS between nodes.** Only single-node mode exists (cluster is milestone M3).
- **Multi-tenant isolation.** One tenant per node (`node.tenant`); tokens are not tied to a tenant (spec question Q8).
- **Token features.** No expiry, no rotation without restart, no per-token rate limits, no audit log.
- **CORS.** No CORS headers are sent; the UI is served from the same origin.
- **Read-only laptop mode** with a tenant-scoped object-storage key, and **erasure** (rewriting files to delete data).
- **Dependency licence checks** in CI.

See [Limitations and roadmap](limitations-and-roadmap.md).
