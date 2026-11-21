# Blob-first observability engine — specification and research

| File | Read it for |
| :--- | :--- |
| [SPECIFICATION.md](SPECIFICATION.md) | The product specification to implement from: scope, architecture, storage, ingest, the full TQS 3 query-schema reference, edge cases, the plan, and **20 questions for the product owner** (§16) |
| [RESEARCH.md](RESEARCH.md) | What was tested and how, every result, what changed the design, and what is not proven |
| [research/](research/) | Rust research code and its results; `research/scripts/run-all.sh` reproduces every number |
| [history/](history/) | The original v5 specification, its review, and the first concept (superseded) |

## Results at a glance

| Area | Result |
| :--- | :--- |
| Storage on MinIO + DuckLake (E1–E9) | 9/9 pass |
| TQS metric functions vs Prometheus 3.16 (M01–M33) | 33/33 equal |
| Real Margince dashboard panel shapes vs Prometheus (R01–R11) | 17/17 runs equal |
| Points compared with Prometheus | 30,758, all equal |
| Spans / logs / traces vs an independent computation | 14/14 equal |
| Validation and injection cases | 38/38 |
| Mutation check (can the suite fail?) | 15/15 mutants caught |
| Real Margince board expressible in TQS | 98/99 panels (1 uses a subquery) |

The research ran on small fixtures with MinIO on the same host. It proves mechanisms and query
semantics, not production performance. See RESEARCH.md §7.
