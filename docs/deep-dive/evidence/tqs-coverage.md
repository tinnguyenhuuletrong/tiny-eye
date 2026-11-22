# TQS 3 coverage results

Reference: Prometheus 3.16.0 on the same fixtures. Engine: DuckDB v2.0.1. Fixtures: 349 series / 161154 samples (Margince-shaped), 14999 spans, 4288 logs.

**105/105 cases pass.** Range 1762741800000..1762746900000 ms, step 60000 ms unless stated; equality tolerance 1e-9 relative.


## metrics vs Prometheus

| ID | Function | Reference | Result | Detail | ms |
| :--- | :--- | :--- | :--- | :--- | ---: |
| M01 | rate per series | `rate(margince_ai_calls_total[5m])` | PASS | 2351 points equal (max rel. diff 2.3e-16); missing 0, extra 0, mismatched 0 | 27.9 |
| M02 | sum by of rate | `sum by (provider) (rate(margince_ai_calls_total[5m]))` | PASS | 172 points equal (max rel. diff 2.3e-16); missing 0, extra 0, mismatched 0 | 16.4 |
| M03 | increase over 1h | `sum by (task) (increase(margince_ai_calls_total[1h]))` | PASS | 344 points equal (max rel. diff 2.1e-16); missing 0, extra 0, mismatched 0 | 15.3 |
| M04 | irate | `irate(margince_ai_calls_total[1m])` | PASS | 2346 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 14.9 |
| M05 | error ratio (formula) | `sum by (provider) (rate(margince_ai_calls_total{outcome="error"}[5m])) / sum by (provider) (rate(margince_ai_calls_total[5m]))` | PASS | 172 points equal (max rel. diff 4.9e-16); missing 0, extra 0, mismatched 0 | 23.4 |
| M06 | histogram p95 by label | `histogram_quantile(0.95, sum by (task, le) (rate(margince_ai_call_duration_seconds_bucket[5m])))` | PASS | 258 points equal (max rel. diff 3.4e-15); missing 0, extra 0, mismatched 0 | 22.7 |
| M07 | histogram p99 overall | `histogram_quantile(0.99, sum by (le) (rate(margince_ai_call_duration_seconds_bucket[5m])))` | PASS | 86 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 22.1 |
| M08 | histogram p99 per series (+Inf bucket edge) | `histogram_quantile(0.99, rate(margince_ai_call_duration_seconds_bucket[5m]))` | PASS | 516 points equal (max rel. diff 2.8e-14); missing 0, extra 0, mismatched 0 | 23.9 |
| M09 | average latency from _sum/_count | `sum by (task) (rate(margince_ai_call_duration_seconds_sum[5m])) / sum by (task) (rate(margince_ai_call_duration_seconds_count[5m]))` | PASS | 258 points equal (max rel. diff 4.2e-16); missing 0, extra 0, mismatched 0 | 20.8 |
| M10 | latest gauge value (5m lookback, stale series) | `margince_job_queue_depth` | PASS | 2061 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 12.2 |
| M11 | max across series | `max(margince_job_queue_depth)` | PASS | 86 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 10.7 |
| M12 | max_over_time | `max_over_time(margince_job_queue_depth[10m])` | PASS | 2096 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 11.8 |
| M13 | avg_over_time | `avg_over_time(margince_job_queue_depth[10m])` | PASS | 2096 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 11.2 |
| M14 | min_over_time | `min_over_time(margince_job_queue_depth[10m])` | PASS | 2096 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 11.0 |
| M15 | sum_over_time | `sum_over_time(margince_pgxpool_conns[10m])` | PASS | 172 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 7.3 |
| M16 | count_over_time | `count_over_time(margince_job_queue_depth[10m])` | PASS | 2096 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 11.9 |
| M17 | last_over_time | `last_over_time(margince_job_queue_depth[10m])` | PASS | 2096 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 12.2 |
| M18 | delta of a gauge | `delta(margince_job_queue_depth[10m])` | PASS | 2096 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 12.9 |
| M19 | top 3 per step | `topk(3, sum by (provider, task) (rate(margince_ai_calls_total[5m])))` | PASS | 258 points equal (max rel. diff 1.9e-16); missing 0, extra 0, mismatched 0 | 15.5 |
| M20 | time shift (offset) | `sum(rate(margince_ai_calls_total[5m] offset 30m))` | PASS | 85 points equal (max rel. diff 2.9e-16); missing 0, extra 0, mismatched 0 | 13.5 |
| M21 | regex and negative regex matchers | `sum by (task) (rate(margince_ai_calls_total{provider=~"anth.*",task!~"summ.*"}[5m]))` | PASS | 258 points equal (max rel. diff 2.2e-16); missing 0, extra 0, mismatched 0 | 12.1 |
| M22 | aggregate without labels | `sum without (outcome, tier, model) (rate(margince_ai_calls_total[5m]))` | PASS | 1097 points equal (max rel. diff 2.4e-16); missing 0, extra 0, mismatched 0 | 15.3 |
| M23 | count series by label | `count by (provider) (margince_ai_calls_total)` | PASS | 172 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 11.4 |
| M24 | counter resets | `resets(margince_ai_calls_total[30m])` | PASS | 2373 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 13.9 |
| M25 | gauge changes | `changes(margince_job_queue_depth[10m])` | PASS | 2096 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 13.1 |
| M26 | scalar arithmetic (per minute) | `sum(rate(margince_ai_calls_total[5m])) * 60` | PASS | 86 points equal (max rel. diff 1.7e-16); missing 0, extra 0, mismatched 0 | 13.7 |
| M27 | value filter (having) | `sum by (task) (rate(margince_ai_calls_total[5m])) > 1` | PASS | 344 points equal (max rel. diff 3.6e-16); missing 0, extra 0, mismatched 0 | 13.7 |
| M28 | label value containing a quote | `sum(rate(margince_ai_calls_total{task="quote\"d"}[5m]))` | PASS | 86 points equal (max rel. diff 3.8e-16); missing 0, extra 0, mismatched 0 | 10.8 |
| M29 | avg across series | `avg by (provider) (rate(margince_ai_calls_total[5m]))` | PASS | 172 points equal (max rel. diff 3.4e-16); missing 0, extra 0, mismatched 0 | 13.5 |
| M30 | instant table | `sum by (provider) (increase(margince_ai_calls_total[1h]))` | PASS | 2 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 12.7 |
| M31 | window shorter than scrape interval | `rate(margince_ai_calls_total[10s])` | PASS | 0 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 13.0 |
| M32 | clamp | `clamp(sum(rate(margince_ai_calls_total[5m])), 0, 20)` | PASS | 86 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 13.6 |
| M33 | formula of a formula | `sum by (provider) (rate(margince_ai_calls_total{outcome="error"}[5m])) / sum by (provider) (rate(margince_ai_calls_total[5m])) * 100` | PASS | 172 points equal (max rel. diff 4.7e-16); missing 0, extra 0, mismatched 0 | 23.1 |
| M34 | outer join of scalars: steps missing in the first operand | `sum(rate(margince_ai_calls_total{provider="openai",task="draft",outcome="error"}[5m])) or sum(rate(margince_ai_calls_total{provider="openai",task="draft"}[5m])) * 0` | PASS | 86 points equal (max rel. diff 2.3e-16); missing 0, extra 0, mismatched 0 | 19.7 |

## deliberate divergence

| ID | Function | Reference | Result | Detail | ms |
| :--- | :--- | :--- | :--- | :--- | ---: |
| D1 | fill: zero on a missing operand | `sum by (provider) (rate(margince_ai_calls_total{task="classify",outcome="error"}[5m])) / sum by (provider) (rate(margince_ai_calls_total{task="classify"}[5m]))` | PASS | Prometheus: no openai points (no error series). TQS: 65 openai points, all 0. Other 86/86 points equal. | 20.7 |
| D2 | division by zero | `sum by (provider) (rate(margince_ai_calls_total{task="classify",outcome="error",provider="anthropic"}[5m])) / sum by (provider) (rate(margince_ai_calls_total{task="classify",outcome="error",provider="anthropic"}[5m]))` | PASS | Prometheus: 86 NaN points. TQS: 0 points (x/0 is no value, rendered as a gap). | 19.0 |

## real dashboard panels vs Prometheus

| ID | Function | Reference | Result | Detail | ms |
| :--- | :--- | :--- | :--- | :--- | ---: |
| R01/ns-a | stat: AI calls in range, 0 when none | `sum(increase(margince_ai_calls_total{kubernetes_namespace="ns-a"}[5100s])) or vector(0)` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 11.8 |
| R01/ns-empty | stat: AI calls in range, 0 when none | `sum(increase(margince_ai_calls_total{kubernetes_namespace="ns-empty"}[5100s])) or vector(0)` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 8.6 |
| R02/ns-a | stat: AI error % with clamps, 0 when none | `100 * clamp_max(sum(increase(margince_ai_call_errors_total{kubernetes_namespace="ns-a"}[5100s])) / clamp_min(sum(increase(margince_ai_call_attempts_total{kubernetes_namespace="ns-a"}[5100s])), 1), 1) or vector(0)` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 19.3 |
| R02/ns-b | stat: AI error % with clamps, 0 when none | `100 * clamp_max(sum(increase(margince_ai_call_errors_total{kubernetes_namespace="ns-b"}[5100s])) / clamp_min(sum(increase(margince_ai_call_attempts_total{kubernetes_namespace="ns-b"}[5100s])), 1), 1) or vector(0)` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 18.0 |
| R02/ns-empty | stat: AI error % with clamps, 0 when none | `100 * clamp_max(sum(increase(margince_ai_call_errors_total{kubernetes_namespace="ns-empty"}[5100s])) / clamp_min(sum(increase(margince_ai_call_attempts_total{kubernetes_namespace="ns-empty"}[5100s])), 1), 1) or vector(0)` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 17.0 |
| R03/ns-a | stat: p95 HTTP latency over the range | `histogram_quantile(0.95, sum by (le) (rate(margince_http_request_duration_seconds_bucket{kubernetes_namespace="ns-a"}[5100s])))` | PASS | 1 points equal (max rel. diff 4.8e-16); missing 0, extra 0, mismatched 0 | 22.5 |
| R04/ns-a | graph: calls per hour by provider | `sum by (provider) (rate(margince_ai_calls_total{kubernetes_namespace="ns-a"}[30m])) * 3600` | PASS | 172 points equal (max rel. diff 2.5e-16); missing 0, extra 0, mismatched 0 | 16.9 |
| R04z/ns-a | graph: the panel's 0 * absent(...) line | `0 * absent(margince_ai_calls_total{kubernetes_namespace="ns-a"})` | PASS | 0 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0; both empty, as expected | 17.0 |
| R04/ns-empty | graph: calls per hour by provider | `sum by (provider) (rate(margince_ai_calls_total{kubernetes_namespace="ns-empty"}[30m])) * 3600` | PASS | 0 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0; both empty, as expected | 11.4 |
| R04z/ns-empty | graph: the panel's 0 * absent(...) line | `0 * absent(margince_ai_calls_total{kubernetes_namespace="ns-empty"})` | PASS | 86 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 11.0 |
| R05/ns-a | graph: queue depth with zero baselines (or ... * 0) | `sum by (queue, kind) (max by (queue,kind,workspace_id) (margince_job_queue_depth{kubernetes_namespace="ns-a"})) or sum by (queue, kind) (margince_job_declared_info{kubernetes_namespace="ns-a",queue!=""} * 0) or sum by (queue, kind) (max_over_time(margince_job_queue_depth{kubernetes_namespace="ns-a"}[7d]) * 0)` | PASS | 430 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 34.4 |
| R06/ns-a | graph: process uptime by app (time() - start) | `max by (app) (time() - process_start_time_seconds{kubernetes_namespace="ns-a"})` | PASS | 172 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 10.3 |
| R07/ns-a | graph: p95 latency of the 10 busiest routes | `histogram_quantile(0.95, sum by (route, le) (rate(margince_http_request_duration_seconds_bucket{kubernetes_namespace="ns-a"}[30m]) and on (route) topk(10, sum by (route) (rate(margince_http_request_duration_seconds_count{kubernetes_namespace="ns-a"}[30m])))))` | PASS | 860 points equal (max rel. diff 5.7e-15); missing 0, extra 0, mismatched 0 | 41.5 |
| R09/ns-a | stat: number of active models | `count(count by (provider,model) (increase(margince_ai_calls_total{kubernetes_namespace="ns-a"}[5100s]) > 0)) or vector(0)` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 13.1 |
| R09/ns-empty | stat: number of active models | `count(count by (provider,model) (increase(margince_ai_calls_total{kubernetes_namespace="ns-empty"}[5100s]) > 0)) or vector(0)` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 9.7 |
| R10/ns-a | graph: CPU by pod with role derived from the pod name | `label_replace(label_replace(sum by (pod) (rate(container_cpu_usage_seconds_total{namespace="ns-a",container!="",container!="POD"}[30m])), "role", "$1", "pod", "([a-z]+)-application-.*"), "role", "$1", "pod", "([a-z]+)(?:-master)?-[0-9]+")` | PASS | 344 points equal (max rel. diff 2.7e-16); missing 0, extra 0, mismatched 0 | 14.3 |
| R11/ns-a | stat: declared job kinds | `count(margince_job_declared_info{kubernetes_namespace="ns-a"})` | PASS | 1 points equal (max rel. diff 0.0e0); missing 0, extra 0, mismatched 0 | 9.0 |

## spans vs Rust

| ID | Function | Reference | Result | Detail | ms |
| :--- | :--- | :--- | :--- | :--- | ---: |
| S1 | span rate by service | count / step, by start time | PASS | 336 points; missing 0, extra 0, max rel. diff 0.0e0 | 4.9 |
| S2 | error count by service | count where status=error | PASS | 98 points; missing 0, extra 0, max rel. diff 0.0e0 | 2.5 |
| S3 | p95 duration (exact, ad hoc) | R-7 quantile | PASS | 9 points; missing 0, extra 0, max rel. diff 1.3e-16 | 2.7 |
| S4 | error % by service (fill zero) | errors/all*100, 0 when no errors | PASS | 36 points; missing 0, extra 0, max rel. diff 0.0e0 | 6.2 |
| S5 | attribute + duration filter, table | count over (from, to] | PASS | 3 points; missing 0, extra 0, max rel. diff 0.0e0 | 2.8 |
| S6 | regex on span name, avg duration | mean | PASS | 9 points; missing 0, extra 0, max rel. diff 0.0e0 | 2.6 |

## logs vs Rust

| ID | Function | Reference | Result | Detail | ms |
| :--- | :--- | :--- | :--- | :--- | ---: |
| L1 | log count by severity | count | PASS | 27 points; missing 0, extra 0, max rel. diff 0.0e0 | 2.8 |
| L2 | substring search count, table | count | PASS | 1 points; missing 0, extra 0, max rel. diff 0.0e0 | 2.3 |
| L3 | log search (regex, newest first, limit) | filter + sort + take 20 | PASS | 20 rows, order identical: true | 2.4 |
| L4 | log patterns (deterministic masking) | group by masked-body hash | PASS | 5 patterns (5 templates planted); counts identical: true; templates: user <*> placed order <*> total <*> \| ledger posted entry <*> \| health check ok in <*>ms — région eu \| connection to <*> timed out after <*>ms \| payment declined for card ****<*> code <*> | 1.5 |
| L5 | logs of one trace | filter on trace id | PASS | 2 of 2 log lines | 1.2 |

## traces vs Rust

| ID | Function | Reference | Result | Detail | ms |
| :--- | :--- | :--- | :--- | :--- | ---: |
| T1 | trace search: failed payment, > 1 s, slowest first | group spans by trace | PASS | 10 traces, order identical: true | 4.5 |
| T2 | waterfall across regions | planted parent/child tree | PASS | 0:us-east:gateway → 1:us-east:checkout → 2:us-east:checkout → 2:eu-west:payment → 3:eu-west:ledger | 4.5 |
| T3 | waterfall with a lost parent span | checkout span removed | PASS | 4 spans shown, 2 flagged orphan (expected 4 and 2) | 4.3 |

## validation

| ID | Function | Reference | Result | Detail | ms |
| :--- | :--- | :--- | :--- | :--- | ---: |
| V01 | refuse with `schema` | — | PASS | problem codes: ["schema"] | 0.0 |
| V02 | refuse with `version` | — | PASS | problem codes: ["version"] | 0.0 |
| V03 | refuse with `time` | — | PASS | problem codes: ["time"] | 0.0 |
| V04 | refuse with `time_order` | — | PASS | problem codes: ["time_order"] | 0.0 |
| V05 | refuse with `too_many_points` | — | PASS | problem codes: ["too_many_points"] | 0.0 |
| V06 | refuse with `step` | — | PASS | problem codes: ["step"] | 0.0 |
| V07 | refuse with `queries` | — | PASS | problem codes: ["queries"] | 0.0 |
| V08 | refuse with `queries` | — | PASS | problem codes: ["queries"] | 0.0 |
| V09 | refuse with `name` | — | PASS | problem codes: ["name"] | 0.0 |
| V10 | refuse with `source` | — | PASS | problem codes: ["source"] | 0.0 |
| V11 | refuse with `fn` | — | PASS | problem codes: ["fn"] | 0.0 |
| V12 | refuse with `window` | — | PASS | problem codes: ["window"] | 0.0 |
| V13 | refuse with `window` | — | PASS | problem codes: ["window"] | 0.0 |
| V14 | refuse with `q` | — | PASS | problem codes: ["q"] | 0.0 |
| V15 | refuse with `q` | — | PASS | problem codes: ["q"] | 0.0 |
| V16 | refuse with `label` | — | PASS | problem codes: ["label"] | 0.0 |
| V17 | refuse with `op` | — | PASS | problem codes: ["op"] | 0.0 |
| V18 | refuse with `regex` | — | PASS | problem codes: ["regex"] | 0.0 |
| V19 | refuse with `regex` | — | PASS | problem codes: ["regex"] | 0.0 |
| V20 | refuse with `formula_ref` | — | PASS | problem codes: ["formula_ref"] | 0.0 |
| V21 | refuse with `formula` | — | PASS | problem codes: ["formula"] | 0.0 |
| V22 | refuse with `formula` | — | PASS | problem codes: ["formula"] | 0.0 |
| V23 | refuse with `formula` | — | PASS | problem codes: ["formula"] | 0.0 |
| V24 | refuse with `formula_join` | — | PASS | problem codes: ["formula_join"] | 0.0 |
| V25 | refuse with `agg` | — | PASS | problem codes: ["agg"] | 0.0 |
| V26 | refuse with `agg` | — | PASS | problem codes: ["agg"] | 0.0 |
| V27 | refuse with `field` | — | PASS | problem codes: ["field"] | 0.0 |
| V28 | refuse with `cond` | — | PASS | problem codes: ["cond"] | 0.0 |
| V29 | refuse with `q` | — | PASS | problem codes: ["q"] | 0.0 |
| V30 | refuse with `measure` | — | PASS | problem codes: ["measure"] | 0.0 |
| V31 | refuse with `trace_id` | — | PASS | problem codes: ["trace_id"] | 0.0 |
| V32 | refuse with `offset` | — | PASS | problem codes: ["offset"] | 0.0 |
| V33 | refuse with `having` | — | PASS | problem codes: ["having"] | 0.0 |
| V34 | refuse with `top` | — | PASS | problem codes: ["top"] | 0.0 |
| V35 | refuse with `schema` | — | PASS | problem codes: ["schema"] | 0.0 |
| V36 | refuse with `metric` | — | PASS | problem codes: ["metric"] | 0.0 |
| A1 | SQL-looking matcher value is bound, not executed | — | PASS | 0 rows (expected 0) | 8.7 |
| A2 | SQL-looking search text is bound, not executed | — | PASS | 0 rows; logs table still has 4288 rows | 0.0 |

## Mutation check — can the suite fail?

Each mutant breaks one rule in the compiler on purpose. A mutant no case catches is a hole in the suite.

| Mutant | Rule broken | Cases that turned red |
| :--- | :--- | :--- |
| `no_reset_correction` | counter resets are not added back in rate/increase | 16 (M01, M02, M03, M05, M06, M07, M08, M09, M19, M20, M22, M26, M27, M29, M32, M33) |
| `closed_window` | range windows include their left edge [t-w, t] | 31 (M01, M02, M03, M04, M05, M06, M08, M09, M10, M12, M13, M14, M15, M16, M17, M18, M19, M20, M21, M22, M25, M26, M27, M28, M29, M33, M34, R04/ns-a, R05/ns-a, R07/ns-a, R10/ns-a) |
| `no_extrapolation` | rate/increase/delta use the raw first-to-last difference | 23 (M01, M02, M03, M05, M06, M09, M18, M19, M20, M21, M22, M26, M27, M28, M29, M30, M33, M34, R01/ns-a, R02/ns-a, R03/ns-a, R04/ns-a, R10/ns-a) |
| `lookback_4m` | latest-value lookback is 4 m instead of 5 m | 3 (M10, M23, R05/ns-a) |
| `hist_upper_bound` | histogram_quantile returns the bucket upper bound, no interpolation | 4 (M06, M08, R03/ns-a, R07/ns-a) |
| `without_ignored` | agg.without keeps every label | 1 (M22) |
| `fill_ignored` | fill: zero has no effect | 6 (D1, R01/ns-empty, R02/ns-b, R02/ns-empty, R09/ns-empty, S4) |
| `division_ieee` | x/0 yields +Inf/NaN instead of no value | 1 (D2) |
| `bucket_closed_left` | spans/logs buckets include their left edge | 1 (L1) |
| `derive_ignored` | derive (label_replace) rules are not applied | 1 (R10/ns-a) |
| `then_ignored` | the second aggregation (then) is skipped | 2 (R05/ns-a, R09/ns-a) |
| `outer_as_inner` | formula join outer behaves like inner | 2 (M34, R05/ns-a) |
| `outer_scalar_base_first` | formula join outer of scalars keeps only the first operand's steps | 1 (M34) |
| `top_query_ignored` | top.query does not restrict the output | 1 (R07/ns-a) |
| `absent_unlabelled` | absent() returns no labels | 1 (R04z/ns-empty) |
| `time_in_ms` | formula `time` is in milliseconds, not seconds | 1 (R06/ns-a) |
