# End-to-end: TQS vs Prometheus on remote-written data

The same 114 series / 25920 samples were remote-written to Prometheus 3.16 and to tiny-eye's HTTP API, then each panel was compared at every step (1795268400000..1795271400000 ms, step 30 s). Equality: within 1e-9 relative; a point on one side only is a failure.

Storage states during the run:

- after chunk 6: flushed, committed, compacted 2 file group(s)
- after chunk 9: flushed, envelope not yet applied (rows still hot)
- last chunks: only in the hot tail (RAM)

| ID | Panel | PromQL | Result | Points | tiny-eye ms |
| :--- | :--- | :--- | :--- | ---: | ---: |
| W01 | rate, sum by | `sum by (service) (rate(app_requests_total{namespace="ns-a"}[5m]))` | PASS | 202 | 48.7 |
| W02 | increase per series (reset, late start) | `increase(app_requests_total{route="/search"}[10m])` | PASS | 808 | 24.9 |
| W03 | irate | `sum(irate(app_requests_total[1m]))` | PASS | 101 | 25.3 |
| W04 | histogram_quantile p95 | `histogram_quantile(0.95, sum by (service, le) (rate(app_latency_seconds_bucket[5m])))` | PASS | 202 | 32.3 |
| W05 | error ratio formula | `sum(rate(app_requests_total{code=~"5.."}[5m])) / sum(rate(app_requests_total[5m]))` | PASS | 101 | 37.5 |
| W06 | gauge last, max by (stale series ends) | `max by (queue) (queue_depth)` | PASS | 263 | 19.1 |
| W07 | last per series incl. staleness marker | `queue_depth{queue="stopping",namespace="ns-a"}` | PASS | 183 | 23.2 |
| W08 | avg_over_time | `avg_over_time(queue_depth{namespace="ns-b"}[10m])` | PASS | 847 | 20.9 |
| W09 | two-level aggregation (then) | `sum by (queue) (max by (queue, workspace) (queue_depth))` | PASS | 263 | 21.3 |
| W10 | topk per step | `topk(3, sum by (route) (rate(app_requests_total[5m])))` | PASS | 303 | 30.4 |
| W11 | offset | `sum(rate(app_requests_total[5m] offset 10m))` | PASS | 100 | 23.2 |
| W12 | value filter (having) | `sum by (route) (rate(app_requests_total[5m])) > 4` | PASS | 404 | 24.5 |
| W13 | or vector(0) on no data | `sum(rate(app_requests_total{code="418"}[5m])) or vector(0)` | PASS | 101 | 16.8 |
| W14 | absent | `absent(app_requests_total{service="nope"})` | PASS | 101 | 15.3 |
| W15 | label_replace (derive) | `sum by (role) (label_replace(rate(pod_cpu_seconds_total[5m]), "role", "$1", "pod", "(api\|worker)-.*"))` | PASS | 303 | 20.9 |
| W16 | count of series | `count(queue_depth)` | PASS | 101 | 19.0 |
| W17 | delta on gauges | `delta(queue_depth{queue="emails"}[10m])` | PASS | 606 | 19.4 |
| W18 | resets | `resets(app_requests_total{namespace="ns-a",service="api"}[30m])` | PASS | 808 | 19.5 |
| W19 | table: increase over $__range | `sum by (service) (increase(app_requests_total[50m]))` | PASS | 2 | 22.3 |
| W20 | negative regex + max_over_time | `max by (namespace) (max_over_time(queue_depth{queue!~"stop.*"}[15m]))` | PASS | 202 | 17.9 |

**20/20 panels equal to Prometheus.**
