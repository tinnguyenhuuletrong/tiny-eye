# TQS on DuckLake + MinIO

The same compiled queries, run on DuckLake tables stored in MinIO, against the in-memory result.

| ID | Function | Result | Detail |
| :--- | :--- | :--- | :--- |
| M02-lake | sum by of rate | PASS | 172 points identical: true; memory 21.2 ms, lake 126.7 ms |
| M05-lake | error ratio (formula) | PASS | 172 points identical: true; memory 34.7 ms, lake 54.0 ms |
| M06-lake | histogram p95 by label | PASS | 258 points identical: true; memory 39.1 ms, lake 47.8 ms |
| M10-lake | latest gauge value (5m lookback, stale series) | PASS | 303 points identical: true; memory 14.9 ms, lake 28.2 ms |
| M19-lake | top 3 per step | PASS | 258 points identical: true; memory 24.4 ms, lake 36.6 ms |
| M22-lake | aggregate without labels | PASS | 667 points identical: true; memory 21.8 ms, lake 34.7 ms |
