# Real dashboard coverage (construct level)

Source: the Margince Grafana board as recorded by margince-qc (99 panels with PromQL). Aggregate counts only.

**98/99 panels (99%) use only constructs TQS 3 can express.** Construct level means each construct has a TQS equivalent; it does not by itself prove the panel's numbers. The numbers are proven for the representative panels R01–R11 in `results/tqs-coverage.md`.

| Not expressible | Panels |
| :--- | ---: |
| subquery `expr[range:step]` | 1 |

| PromQL construct | Panels using it | TQS 3 | Verified by |
| :--- | ---: | :--- | :--- |
| `sum` | 71 | agg.op sum / then | M02 R05 |
| `by` | 61 | agg.by | M02 |
| `rate` | 41 | fn.op rate | M01 M02 R04 R07 R10 |
| `or` | 32 | formula join outer + coalesce | R05 |
| `max` | 31 | agg.op max | M11 R05 |
| `vector` | 28 | fill: zero (only `or vector(0)`) | R01 R02 R09 |
| `absent` | 16 | agg.op absent | R04z |
| `increase` | 10 | fn.op increase | M03 R01 R02 R09 |
| `histogram_quantile` | 8 | fn.op histogram_quantile | M06 M07 M08 R03 R07 |
| `clamp_min` | 6 | formula greatest() | R02 |
| `clamp_max` | 3 | formula least() | R02 |
| `count` | 3 | agg.op count / then count | M23 R09 R11 |
| `max_over_time` | 3 | fn.op max_over_time | M12 R05 |
| `label_replace` | 3 | derive (regex) / output name (constant label) | R10 |
| `and` | 1 | top.query (only with topk) | R07 |
| `topk` | 1 | top (per_step) / top.query | M19 R07 |
| `on` | 1 | top.query (only `and on(...) topk`) | R07 |
| `delta` | 1 | fn.op delta | M18 |
| `min` | 1 | agg.op min | R06 |
| `time` | 1 | formula `time` | R06 |
