# End-to-end: TQS vs Prometheus on remote-written data

The same 114 series / 25920 samples were remote-written to Prometheus 3.16 and to tiny-eye's HTTP API, then each panel was compared at every step (1795423440000..1795426440000 ms, step 30 s). Equality: within 1e-9 relative; a point on one side only is a failure.

Storage states during the run:

- after chunk 6: flushed, committed, compacted 2 file group(s)
- after chunk 9: flushed, envelope not yet applied (rows still hot)
- last chunks: only in the hot tail (RAM)

| ID | Panel | PromQL | Result | Points | tiny-eye ms |
| :--- | :--- | :--- | :--- | ---: | ---: |
| W01 | rate, sum by | `sum by (service) (rate(app_requests_total{namespace="ns-a"}[5m]))` | PASS | 202 | 58.8 |
| W02 | increase per series (reset, late start) | `increase(app_requests_total{route="/search"}[10m])` | PASS | 808 | 27.3 |
| W03 | irate | `sum(irate(app_requests_total[1m]))` | PASS | 101 | 28.6 |
| W04 | histogram_quantile p95 | `histogram_quantile(0.95, sum by (service, le) (rate(app_latency_seconds_bucket[5m])))` | PASS | 202 | 36.6 |
| W05 | error ratio formula | `sum(rate(app_requests_total{code=~"5.."}[5m])) / sum(rate(app_requests_total[5m]))` | PASS | 101 | 47.0 |
| W06 | gauge last, max by (stale series ends) | `max by (queue) (queue_depth)` | PASS | 263 | 24.3 |
| W07 | last per series incl. staleness marker | `queue_depth{queue="stopping",namespace="ns-a"}` | PASS | 183 | 21.4 |
| W08 | avg_over_time | `avg_over_time(queue_depth{namespace="ns-b"}[10m])` | PASS | 847 | 21.7 |
| W09 | two-level aggregation (then) | `sum by (queue) (max by (queue, workspace) (queue_depth))` | PASS | 263 | 28.5 |
| W10 | topk per step | `topk(3, sum by (route) (rate(app_requests_total[5m])))` | PASS | 303 | 31.1 |
| W11 | offset | `sum(rate(app_requests_total[5m] offset 10m))` | PASS | 100 | 27.8 |
| W12 | value filter (having) | `sum by (route) (rate(app_requests_total[5m])) > 4` | PASS | 404 | 32.9 |
| W13 | or vector(0) on no data | `sum(rate(app_requests_total{code="418"}[5m])) or vector(0)` | PASS | 101 | 20.5 |
| W14 | absent | `absent(app_requests_total{service="nope"})` | PASS | 101 | 18.8 |
| W15 | label_replace (derive) | `sum by (role) (label_replace(rate(pod_cpu_seconds_total[5m]), "role", "$1", "pod", "(api\|worker)-.*"))` | PASS | 303 | 27.4 |
| W16 | count of series | `count(queue_depth)` | PASS | 101 | 26.5 |
| W17 | delta on gauges | `delta(queue_depth{queue="emails"}[10m])` | PASS | 606 | 27.3 |
| W18 | resets | `resets(app_requests_total{namespace="ns-a",service="api"}[30m])` | PASS | 808 | 25.6 |
| W19 | table: increase over $__range | `sum by (service) (increase(app_requests_total[50m]))` | PASS | 2 | 25.9 |
| W20 | negative regex + max_over_time | `max by (namespace) (max_over_time(queue_depth{queue!~"stop.*"}[15m]))` | PASS | 202 | 21.1 |

**20/20 panels equal to Prometheus.**
