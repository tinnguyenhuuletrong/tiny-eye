# Contributing to tiny-eye

Thanks for your interest. tiny-eye is a blob-first observability engine in one Rust binary;
this page covers the development setup, the daily loop and the rules that keep the codebase small.

## Development setup

You need:

- **Rust stable** (pinned by `rust-toolchain.toml`, with `rustfmt` and `clippy`); install via [rustup](https://rustup.rs).
- **Node.js 20+** for the web UI (`ui/`) and the documentation site (`website/`).
- **[Bun](https://bun.sh) 1.x** for the examples (`examples/demo/app` and `examples/machine`, TypeScript). `make examples-lint` runs Biome (lint and format) and `tsc` on both; `bun run format` inside an example fixes style.
- **Docker** (optional): MinIO for `make bench-s3`, `make fleet-poc` and `make bench-all` (started by `make s3-up`), and k6 for the demo (`make demo-load`).

Then, once:

```bash
make tools   # pinned Prometheus 3.16 + DuckDB extensions into .tools/
make hooks   # optional: pre-commit hook running fmt + clippy
```

`make help` lists every target.

## The loop

```bash
make fmt lint test   # format, clippy with -D warnings, unit + integration tests
make run             # single node on 127.0.0.1:7070 with config/dev.yaml
```

Before opening a pull request, run everything CI runs:

```bash
make check   # lint + test (incl. e2e vs Prometheus and the schema sync check) + conformance
```

## Principles

1. **Keep it simple.** The smallest design that meets the need. No layer, trait, crate or config
   knob without a second user. Three crates: `tqs`, `engine`, `server`; one binary.
2. **One source of truth, generated everywhere else.** Never hand-write what a definition already says:

   | Definition | Generates |
   | --- | --- |
   | `api/openapi.yaml` | axum router (operationId → `handlers::<id>`), response types, auth scopes, MCP tools, UI client types |
   | `schema/tables.yaml` | Arrow schemas, typed row builders, DuckLake and hot-tail DDL |
   | `config/default.yaml` | every default; user YAML and `TINY_EYE__*` env vars override |
   | `dashboards/*.yaml` | dashboards and panels (TQS documents in git) |

   Change the definition, rebuild; the compiler points at whatever must follow. Do not edit
   generated code (`target/**/out/*.rs`, `schema/generated/`, `docs/reference/mcp-tools.md`).
3. **Configuration-driven.** Behaviour (sizes, intervals, durability, retention, storage, auth)
   comes from config, not constants in code. A new knob gets its default in `config/default.yaml`.
4. **Proven, not claimed.** Every semantic rule has a test that can fail: TQS against Prometheus
   point by point (`make conformance`, with a mutation check), storage assumptions (`make poc`),
   the whole binary (`make e2e`), numbers (`make bench-all`, also run weekly on GitHub Actions). Results
   land in `docs/deep-dive/evidence/`.

## Conventions

- Rust 2024, `cargo fmt` (max width 140), clippy with `-D warnings`, `unsafe` is forbidden.
- Every query string is a bound parameter; only validated identifiers and numbers are formatted into SQL.
- Errors: `anyhow` with context at boundaries; HTTP errors map to problem codes.

## Adding an API endpoint

1. Add the operation to `api/openapi.yaml`: path, `operationId`, `x-scope`
   (`public` | `ingest` | `query` | `admin`), request and response schemas, and `x-mcp-tool`
   if agents should be able to call it.
2. Build (`cargo build`). The generated router now references `handlers::<operationId>`,
   so the build fails until the handler exists.
3. Implement the handler in `crates/server/src/handlers.rs`, using the generated types, and add a test.
4. Run `make schemas` to regenerate `schema/generated/` and `docs/reference/mcp-tools.md`
   (a test fails if they are stale). Run `npm run gen` in `ui/` if the UI uses the endpoint.

## Commit messages

- Subject in the imperative mood, about 50 characters, no trailing period:
  `Add retention dry-run endpoint`, not `Added ...` or `Adds ...`.
- A blank line, then a body that explains **why** the change is needed and any trade-offs;
  the diff already shows what changed.
- One logical change per commit; keep `make check` green on each.

## Documentation

The docs live in [`docs/`](docs) as Markdown and are published with Docusaurus from
[`website/`](website) to GitHub Pages. Update them in the same pull request as the behaviour they
describe. Preview locally:

```bash
cd website
npm install
BASE_URL=/ npm start   # http://localhost:3000/, reloads on edits in docs/
```

`BASE_URL=/ npm run build` fails on broken links; run it before pushing doc changes.

## Reporting bugs and security issues

Open an issue with the bug report template. Report security vulnerabilities privately as
described in [SECURITY.md](SECURITY.md), not in a public issue.

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE).
