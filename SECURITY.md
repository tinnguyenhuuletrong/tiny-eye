# Security policy

## Supported versions

tiny-eye has no releases yet. Security fixes go to the `main` branch only.

| Version | Supported |
| --- | --- |
| `main` | yes |
| anything else | no |

## Reporting a vulnerability

Please **do not** open a public issue, pull request or discussion for a vulnerability.

Report it privately through GitHub: on the repository page open **Security → Advisories →
Report a vulnerability** (GitHub private vulnerability reporting). Include:

- what is affected (component, endpoint, config) and the commit you tested;
- steps or a proof of concept to reproduce it;
- the impact you expect (data exposure, auth bypass, denial of service, ...).

The maintainers will acknowledge the report, work on a fix in a private security advisory, and
credit you in the advisory unless you prefer otherwise. Please give us reasonable time to release
a fix before disclosing the issue publicly.

## Scope

In scope: the code in this repository, in particular

- the `tiny-eye` binary: HTTP API, ingest receivers (Prometheus remote-write, OTLP/HTTP, Loki push),
  bearer-token authentication and scopes, the MCP bridge;
- TQS validation and compilation to SQL (for example, SQL injection through a TQS document);
- storage handling: WAL, Parquet files, DuckLake catalog, object storage credentials and
  secret attribute redaction;
- the web UI in `ui/`.

Out of scope:

- vulnerabilities in dependencies (DuckDB, Prometheus, npm or crates.io packages) that are not
  caused by how tiny-eye uses them; report those upstream;
- deployments that expose the API without authentication or without a reverse proxy, against the
  documentation (500 responses carry the engine's error text by design);
- the demo app in `examples/`, the prototype in `ui/prototype/`, and `research/`.
