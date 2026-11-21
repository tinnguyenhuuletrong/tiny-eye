# End-to-end: TQS vs Prometheus on remote-written data

The same 114 series / 25920 samples were remote-written to Prometheus 3.16 and to tiny-eye's HTTP API, then each panel was compared at every step (1795270560000..1795273560000 ms, step 30 s). Equality: within 1e-9 relative; a point on one side only is a failure.

Storage states during the run:

- after chunk 6: flushed, committed, compacted 2 file group(s)
- after chunk 9: flushed, envelope not yet applied (rows still hot)
- last chunks: only in the hot tail (RAM)

| ID | Panel | PromQL | Result | Points | tiny-eye ms |
| :--- | :--- | :--- | :--- | ---: | ---: |
| W01 | rate, sum by | `sum by (service) (rate(app_requests_total{namespace="ns-a"}[5m]))` | PASS | 202 | 47.9 |
| W02 | increase per series (reset, late start) | `increase(app_requests_total{route="/search"}[10m])` | PASS | 808 | 22.5 |
| W03 | irate | `sum(irate(app_requests_total[1m]))` | PASS | 101 | 24.0 |
| W04 | histogram_quantile p95 | `histogram_quantile(0.95, sum by (service, le) (rate(app_latency_seconds_bucket[5m])))` | PASS | 202 | 31.9 |
| W05 | error ratio formula | `sum(rate(app_requests_total{code=~"5.."}[5m])) / sum(rate(app_requests_total[5m]))` | PASS | 101 | 41.8 |
| W06 | gauge last, max by (stale series ends) | `max by (queue) (queue_depth)` | PASS | 263 | 21.1 |
| W07 | last per series incl. staleness marker | `queue_depth{queue="stopping",namespace="ns-a"}` | PASS | 183 | 18.2 |
| W08 | avg_over_time | `avg_over_time(queue_depth{namespace="ns-b"}[10m])` | PASS | 847 | 19.3 |
| W09 | two-level aggregation (then) | `sum by (queue) (max by (queue, workspace) (queue_depth))` | PASS | 263 | 22.2 |
| W10 | topk per step | `topk(3, sum by (route) (rate(app_requests_total[5m])))` | PASS | 303 | 25.7 |
| W11 | offset | `sum(rate(app_requests_total[5m] offset 10m))` | PASS | 100 | 23.0 |
| W12 | value filter (having) | `sum by (route) (rate(app_requests_total[5m])) > 4` | PASS | 404 | 24.8 |
| W13 | or vector(0) on no data | `sum(rate(app_requests_total{code="418"}[5m])) or vector(0)` | PASS | 101 | 17.4 |
| W14 | absent | `absent(app_requests_total{service="nope"})` | PASS | 101 | 16.2 |
| W15 | label_replace (derive) | `sum by (role) (label_replace(rate(pod_cpu_seconds_total[5m]), "role", "$1", "pod", "(api\|worker)-.*"))` | PASS | 303 | 22.5 |
| W16 | count of series | `count(queue_depth)` | PASS | 101 | 19.3 |
| W17 | delta on gauges | `delta(queue_depth{queue="emails"}[10m])` | PASS | 606 | 19.6 |
| W18 | resets | `resets(app_requests_total{namespace="ns-a",service="api"}[30m])` | PASS | 808 | 20.6 |
| W19 | table: increase over $__range | `sum by (service) (increase(app_requests_total[50m]))` | PASS | 2 | 23.4 |
| W20 | negative regex + max_over_time | `max by (namespace) (max_over_time(queue_depth{queue!~"stop.*"}[15m]))` | PASS | 202 | 18.8 |

**20/20 panels equal to Prometheus.**
