//! Wired end-to-end test (SPEC §15.3, without the Margince stack): the same samples are sent
//! with Prometheus remote-write to a real Prometheus 3.16 and to the tiny-eye binary's HTTP
//! API, then every panel is compared point by point — Prometheus with the panel's PromQL,
//! tiny-eye with the TQS translation (SPEC §8.13).
//!
//! The data deliberately ends up in every storage state at once: compacted L1 files, L0 files,
//! a flushed-but-uncommitted envelope (rows still in the hot tail) and rows only in RAM.
//! Planted edge cases: counter reset, a series starting late, a series that stops with a
//! staleness marker, 3 % missing scrapes, 0–2 s jitter, a histogram, label values for
//! `label_replace`, and selections that match nothing (`or vector(0)`, `absent`).
//!
//! Run with `make e2e` (needs `make tools`). Writes docs/deep-dive/evidence/e2e.md.

#![allow(clippy::unwrap_used)]

use engine::node::now_ms;
use engine::{Config, Node};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use serde_json::{Value as J, json};
use server::receivers::{Label, Sample, TimeSeries, WriteRequest, encode_remote_write};
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SCRAPE: i64 = 15_000;
const SPAN: i64 = 60 * 60_000;
const STEP: i64 = 30_000;
const REL_TOL: f64 = 1e-9;
const PROM_PORT: u16 = 19092;

struct Series {
    labels: BTreeMap<String, String>, // includes __name__
    samples: Vec<(i64, f64)>,
}

fn ls(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn scrapes(rng: &mut StdRng, from: i64, to: i64) -> Vec<i64> {
    let mut out = vec![];
    let mut t = from;
    while t <= to {
        if !rng.random_bool(0.03) {
            out.push(t + rng.random_range(0..3) * 1000);
        }
        t += SCRAPE;
    }
    out
}

fn fixtures(start: i64, end: i64) -> Vec<Series> {
    let mut rng = StdRng::seed_from_u64(20261121);
    let mut out = vec![];
    // Request counters.
    for ns in ["ns-a", "ns-b"] {
        for svc in ["api", "web"] {
            for route in ["/login", "/home", "/search", "/checkout"] {
                for code in ["200", "500"] {
                    let per_s: f64 = if code == "500" { rng.random_range(0.0..0.3) } else { rng.random_range(0.5..5.0) };
                    let from = if route == "/checkout" && svc == "web" { start + SPAN / 3 } else { start }; // starts late
                    let mut v = 0.0;
                    let mut samples = vec![];
                    let reset_at = start + SPAN / 2;
                    let mut reset_done = false;
                    for t in scrapes(&mut rng, from, end) {
                        v += per_s * 15.0 * rng.random_range(0.5..1.5);
                        if ns == "ns-a" && svc == "api" && route == "/search" && code == "200" && t >= reset_at && !reset_done {
                            v = rng.random_range(0.0..5.0); // counter reset
                            reset_done = true;
                        }
                        samples.push((t, v.floor()));
                    }
                    out.push(Series {
                        labels: ls(&[
                            ("__name__", "app_requests_total"),
                            ("namespace", ns),
                            ("service", svc),
                            ("route", route),
                            ("code", code),
                        ]),
                        samples,
                    });
                }
            }
            // Latency histogram (classic buckets, cumulative counters).
            let bounds = ["0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10", "+Inf"];
            let mut counts = vec![0.0f64; bounds.len()];
            let (mut sum, mut count) = (0.0, 0.0);
            let mut per_bucket: Vec<Vec<(i64, f64)>> = vec![vec![]; bounds.len()];
            let (mut sums, mut cnts) = (vec![], vec![]);
            for t in scrapes(&mut rng, start, end) {
                for _ in 0..rng.random_range(5..40) {
                    let x: f64 = (rng.random_range(-6.0f64..2.5)).exp();
                    for (i, b) in bounds.iter().enumerate() {
                        if *b == "+Inf" || x <= b.parse::<f64>().unwrap() {
                            counts[i] += 1.0;
                        }
                    }
                    sum += x;
                    count += 1.0;
                }
                for i in 0..bounds.len() {
                    per_bucket[i].push((t, counts[i]));
                }
                sums.push((t, sum));
                cnts.push((t, count));
            }
            for (i, b) in bounds.iter().enumerate() {
                out.push(Series {
                    labels: ls(&[("__name__", "app_latency_seconds_bucket"), ("namespace", ns), ("service", svc), ("le", b)]),
                    samples: per_bucket[i].clone(),
                });
            }
            out.push(Series { labels: ls(&[("__name__", "app_latency_seconds_sum"), ("namespace", ns), ("service", svc)]), samples: sums });
            out.push(Series {
                labels: ls(&[("__name__", "app_latency_seconds_count"), ("namespace", ns), ("service", svc)]),
                samples: cnts,
            });
        }
        // Queue gauges per workspace; "stopping" ends at 2/3 of the range with a staleness marker.
        for queue in ["emails", "imports", "stopping"] {
            for ws in ["w1", "w2", "w3"] {
                let to = if queue == "stopping" { start + 2 * SPAN / 3 } else { end };
                let mut v: f64 = rng.random_range(0.0..50.0);
                let mut samples: Vec<(i64, f64)> = scrapes(&mut rng, start, to)
                    .into_iter()
                    .map(|t| {
                        v = (v + rng.random_range(-5.0..5.0)).max(0.0).round();
                        (t, v)
                    })
                    .collect();
                if queue == "stopping" {
                    let last = samples.last().map(|s| s.0).unwrap_or(to);
                    samples.push((last + SCRAPE, f64::from_bits(0x7ff0_0000_0000_0002)));
                }
                out.push(Series {
                    labels: ls(&[("__name__", "queue_depth"), ("namespace", ns), ("queue", queue), ("workspace", ws)]),
                    samples,
                });
            }
        }
        // CPU counters with pod names for label_replace.
        for pod in ["api-7d9f-a", "api-7d9f-b", "worker-55c-x", "db-0"] {
            let per_s: f64 = rng.random_range(0.05..0.9);
            let mut v = 0.0;
            let samples = scrapes(&mut rng, start, end)
                .into_iter()
                .map(|t| {
                    v += per_s * 15.0;
                    (t, v)
                })
                .collect();
            out.push(Series { labels: ls(&[("__name__", "pod_cpu_seconds_total"), ("namespace", ns), ("pod", pod)]), samples });
        }
    }
    out
}

fn write_request(series: &[Series], from: i64, to: i64) -> WriteRequest {
    WriteRequest {
        timeseries: series
            .iter()
            .filter_map(|s| {
                let samples: Vec<Sample> =
                    s.samples.iter().filter(|(t, _)| *t >= from && *t < to).map(|(t, v)| Sample { value: *v, timestamp: *t }).collect();
                (!samples.is_empty()).then(|| TimeSeries {
                    labels: s.labels.iter().map(|(k, v)| Label { name: k.clone(), value: v.clone() }).collect(),
                    samples,
                })
            })
            .collect(),
    }
}

struct Prom {
    child: Child,
}

impl Drop for Prom {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_prometheus(tools: &std::path::Path, dir: &std::path::Path) -> Prom {
    std::fs::write(dir.join("prometheus.yml"), "global:\n  evaluation_interval: 1m\n").unwrap();
    let child = Command::new(tools.join("prometheus"))
        .arg(format!("--config.file={}", dir.join("prometheus.yml").display()))
        .arg(format!("--storage.tsdb.path={}", dir.join("data").display()))
        .arg(format!("--web.listen-address=127.0.0.1:{PROM_PORT}"))
        .args(["--web.enable-remote-write-receiver", "--query.lookback-delta=5m"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join("prometheus.log")).unwrap())
        .spawn()
        .expect("start .tools/prometheus (run `make tools`)");
    let deadline = Instant::now() + Duration::from_secs(60);
    while ureq::get(&format!("http://127.0.0.1:{PROM_PORT}/-/ready")).call().is_err() {
        assert!(Instant::now() < deadline, "prometheus not ready");
        std::thread::sleep(Duration::from_millis(200));
    }
    Prom { child }
}

fn post_rw(url: &str, body: &[u8]) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut r = agent
        .post(url)
        .header("Content-Type", "application/x-protobuf")
        .header("Content-Encoding", "snappy")
        .header("X-Prometheus-Remote-Write-Version", "0.1.0")
        .send(body)
        .unwrap();
    let status = r.status().as_u16();
    assert!(status < 300, "{url}: HTTP {status}: {}", r.body_mut().read_to_string().unwrap_or_default());
}

type Points = BTreeMap<(String, i64), f64>;

fn canonical(m: &serde_json::Map<String, J>) -> String {
    let mut kv: Vec<String> =
        m.iter().filter(|(k, _)| k.as_str() != "__name__").map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect();
    kv.sort();
    kv.join(",")
}

fn prom_query(promql: &str, start: i64, end: i64, instant: bool) -> Points {
    let base = format!("http://127.0.0.1:{PROM_PORT}");
    let req = if instant {
        ureq::get(&format!("{base}/api/v1/query")).query("query", promql).query("time", format!("{:.3}", end as f64 / 1000.0))
    } else {
        ureq::get(&format!("{base}/api/v1/query_range"))
            .query("query", promql)
            .query("start", format!("{:.3}", start as f64 / 1000.0))
            .query("end", format!("{:.3}", end as f64 / 1000.0))
            .query("step", format!("{}", STEP / 1000))
    };
    let v: J = serde_json::from_str(&req.call().unwrap().body_mut().read_to_string().unwrap()).unwrap();
    assert_eq!(v["status"], "success", "{promql}: {v}");
    let mut out = Points::new();
    for s in v["data"]["result"].as_array().into_iter().flatten() {
        let key = canonical(s["metric"].as_object().unwrap());
        if instant {
            out.insert((key, end), s["value"][1].as_str().unwrap().parse().unwrap());
        } else {
            for p in s["values"].as_array().unwrap() {
                out.insert((key.clone(), (p[0].as_f64().unwrap() * 1000.0).round() as i64), p[1].as_str().unwrap().parse().unwrap());
            }
        }
    }
    out
}

fn tqs_query(base: &str, doc: &J, output: &str) -> (Points, f64) {
    let started = Instant::now();
    let mut r = ureq::post(&format!("{base}/api/v1/query")).header("Content-Type", "application/json").send(doc.to_string()).unwrap();
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let v: J = serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap();
    let mut out = Points::new();
    for s in v["series"].as_array().unwrap_or_else(|| panic!("{doc}: {v}")) {
        if s["output"] == output {
            for p in s["points"].as_array().unwrap() {
                out.insert((s["key"].as_str().unwrap().to_string(), p[0].as_i64().unwrap()), p[1].as_f64().unwrap());
            }
        }
    }
    (out, ms)
}

struct Case {
    id: &'static str,
    what: &'static str,
    promql: &'static str,
    instant: bool,
    output: &'static str,
    queries: J,
    formulas: J,
    top: Option<J>,
}

fn q(name: &str, metric: &str, matchers: J, f: J, agg: J) -> J {
    let mut o = json!({"name": name, "metric": metric, "match": matchers, "fn": f});
    if !agg.is_null() {
        o["agg"] = agg;
    }
    o
}

fn m(label: &str, op: &str, value: &str) -> J {
    json!({"label": label, "op": op, "value": value})
}

fn cases() -> Vec<Case> {
    let c =
        |id, what, promql, output, queries: J| Case { id, what, promql, instant: false, output, queries, formulas: json!([]), top: None };
    let rate5 = json!({"op": "rate", "window": "5m"});
    vec![
        c(
            "W01",
            "rate, sum by",
            r#"sum by (service) (rate(app_requests_total{namespace="ns-a"}[5m]))"#,
            "r",
            json!([q(
                "r",
                "app_requests_total",
                json!([m("namespace", "=", "ns-a")]),
                rate5.clone(),
                json!({"op": "sum", "by": ["service"]})
            )]),
        ),
        c(
            "W02",
            "increase per series (reset, late start)",
            r#"increase(app_requests_total{route="/search"}[10m])"#,
            "r",
            json!([q("r", "app_requests_total", json!([m("route", "=", "/search")]), json!({"op": "increase", "window": "10m"}), J::Null)]),
        ),
        c(
            "W03",
            "irate",
            "sum(irate(app_requests_total[1m]))",
            "r",
            json!([q("r", "app_requests_total", json!([]), json!({"op": "irate", "window": "1m"}), json!({"op": "sum"}))]),
        ),
        c(
            "W04",
            "histogram_quantile p95",
            "histogram_quantile(0.95, sum by (service, le) (rate(app_latency_seconds_bucket[5m])))",
            "p",
            json!([q(
                "p",
                "app_latency_seconds",
                json!([]),
                json!({"op": "histogram_quantile", "q": 0.95, "window": "5m"}),
                json!({"op": "sum", "by": ["service"]})
            )]),
        ),
        Case {
            formulas: json!([{"name": "ratio", "expr": "e / a"}]),
            ..c(
                "W05",
                "error ratio formula",
                r#"sum(rate(app_requests_total{code=~"5.."}[5m])) / sum(rate(app_requests_total[5m]))"#,
                "ratio",
                json!([
                    q("e", "app_requests_total", json!([m("code", "=~", "5..")]), rate5.clone(), json!({"op": "sum"})),
                    q("a", "app_requests_total", json!([]), rate5.clone(), json!({"op": "sum"}))
                ]),
            )
        },
        c(
            "W06",
            "gauge last, max by (stale series ends)",
            "max by (queue) (queue_depth)",
            "g",
            json!([q("g", "queue_depth", json!([]), json!({"op": "last"}), json!({"op": "max", "by": ["queue"]}))]),
        ),
        c(
            "W07",
            "last per series incl. staleness marker",
            r#"queue_depth{queue="stopping",namespace="ns-a"}"#,
            "g",
            json!([q(
                "g",
                "queue_depth",
                json!([m("queue", "=", "stopping"), m("namespace", "=", "ns-a")]),
                json!({"op": "last"}),
                J::Null
            )]),
        ),
        c(
            "W08",
            "avg_over_time",
            r#"avg_over_time(queue_depth{namespace="ns-b"}[10m])"#,
            "g",
            json!([q("g", "queue_depth", json!([m("namespace", "=", "ns-b")]), json!({"op": "avg_over_time", "window": "10m"}), J::Null)]),
        ),
        Case {
            queries: json!([{"name": "g", "metric": "queue_depth", "fn": {"op": "last"}, "agg": {"op": "max", "by": ["queue", "workspace"]}, "then": {"op": "sum", "by": ["queue"]}}]),
            ..c("W09", "two-level aggregation (then)", "sum by (queue) (max by (queue, workspace) (queue_depth))", "g", json!([]))
        },
        Case {
            top: Some(json!({"n": 3})),
            ..c(
                "W10",
                "topk per step",
                "topk(3, sum by (route) (rate(app_requests_total[5m])))",
                "r",
                json!([q("r", "app_requests_total", json!([]), rate5.clone(), json!({"op": "sum", "by": ["route"]}))]),
            )
        },
        Case {
            queries: json!([{"name": "r", "metric": "app_requests_total", "fn": rate5.clone(), "agg": {"op": "sum"}, "offset": "10m"}]),
            ..c("W11", "offset", "sum(rate(app_requests_total[5m] offset 10m))", "r", json!([]))
        },
        Case {
            queries: json!([{"name": "r", "metric": "app_requests_total", "fn": rate5.clone(), "agg": {"op": "sum", "by": ["route"]}, "having": {"op": ">", "value": 4}}]),
            ..c("W12", "value filter (having)", "sum by (route) (rate(app_requests_total[5m])) > 4", "r", json!([]))
        },
        Case {
            queries: json!([{"name": "r", "metric": "app_requests_total", "match": [m("code", "=", "418")], "fn": rate5.clone(), "agg": {"op": "sum"}, "fill": "zero"}]),
            ..c("W13", "or vector(0) on no data", r#"sum(rate(app_requests_total{code="418"}[5m])) or vector(0)"#, "r", json!([]))
        },
        c(
            "W14",
            "absent",
            r#"absent(app_requests_total{service="nope"})"#,
            "a",
            json!([q("a", "app_requests_total", json!([m("service", "=", "nope")]), json!({"op": "last"}), json!({"op": "absent"}))]),
        ),
        Case {
            queries: json!([{"name": "c", "metric": "pod_cpu_seconds_total", "fn": rate5.clone(), "agg": {"op": "sum", "by": ["role"]},
                                "derive": [{"label": "role", "from": "pod", "regex": "(api|worker)-.*", "replacement": "$1"}]}]),
            ..c(
                "W15",
                "label_replace (derive)",
                r#"sum by (role) (label_replace(rate(pod_cpu_seconds_total[5m]), "role", "$1", "pod", "(api|worker)-.*"))"#,
                "c",
                json!([]),
            )
        },
        c(
            "W16",
            "count of series",
            "count(queue_depth)",
            "g",
            json!([q("g", "queue_depth", json!([]), json!({"op": "last"}), json!({"op": "count"}))]),
        ),
        c(
            "W17",
            "delta on gauges",
            r#"delta(queue_depth{queue="emails"}[10m])"#,
            "g",
            json!([q("g", "queue_depth", json!([m("queue", "=", "emails")]), json!({"op": "delta", "window": "10m"}), J::Null)]),
        ),
        c(
            "W18",
            "resets",
            r#"resets(app_requests_total{namespace="ns-a",service="api"}[30m])"#,
            "r",
            json!([q(
                "r",
                "app_requests_total",
                json!([m("namespace", "=", "ns-a"), m("service", "=", "api")]),
                json!({"op": "resets", "window": "30m"}),
                J::Null
            )]),
        ),
        Case {
            instant: true,
            ..c(
                "W19",
                "table: increase over $__range",
                "sum by (service) (increase(app_requests_total[50m]))",
                "r",
                json!([q(
                    "r",
                    "app_requests_total",
                    json!([]),
                    json!({"op": "increase", "window": "$__range"}),
                    json!({"op": "sum", "by": ["service"]})
                )]),
            )
        },
        c(
            "W20",
            "negative regex + max_over_time",
            r#"max by (namespace) (max_over_time(queue_depth{queue!~"stop.*"}[15m]))"#,
            "g",
            json!([q(
                "g",
                "queue_depth",
                json!([m("queue", "!~", "stop.*")]),
                json!({"op": "max_over_time", "window": "15m"}),
                json!({"op": "max", "by": ["namespace"]})
            )]),
        ),
    ]
}

fn equal(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || (a - b).abs() <= REL_TOL * a.abs().max(b.abs()).max(1.0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tqs_equals_prometheus_on_remote_written_data() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tools = root.join(".tools");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("prom")).unwrap();
    let _prom = start_prometheus(&tools, &dir.path().join("prom"));

    let d = dir.path().display();
    let yaml = format!(
        "duckdb: {{ extension_dir: {}, threads: 4 }}\nstorage: {{ url: {d}/objects }}\ncatalog: {{ url: {d}/catalog.ducklake }}\n\
         ingest: {{ wal_dir: {d}/wal, max_sample_age: 3h }}\nmaterialize: {{ path: {d}/mat.duckdb }}\ncoordinator: {{ hot_grace: 1ms, compaction_delay: 1ms }}\n",
        tools.join("duckdb_ext").display()
    );
    let node = Node::open(Config::from_layers(Some(&yaml), &[]).unwrap()).await.unwrap();
    let state = server::AppState::new(node.clone(), vec![]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server::app(state)).await.unwrap() });

    // Range ends on a whole minute in the recent past, so both engines see the same grid.
    let end = now_ms() / 60_000 * 60_000 - 60_000;
    let start = end - SPAN;
    let series = fixtures(start, end + SCRAPE);
    let samples: usize = series.iter().map(|s| s.samples.len()).sum();
    let chunk = 5 * 60_000;
    let chunks: Vec<i64> = (0..).map(|i| start + i * chunk).take_while(|t| *t <= end + SCRAPE).collect();
    let mut states = vec![];
    for (i, from) in chunks.iter().enumerate() {
        let body = encode_remote_write(&write_request(&series, *from, from + chunk));
        let (b1, b2, u1, u2) = (body.clone(), body, format!("http://127.0.0.1:{PROM_PORT}/api/v1/write"), format!("{base}/api/v1/write"));
        tokio::task::spawn_blocking(move || {
            post_rw(&u1, &b1);
            post_rw(&u2, &b2);
        })
        .await
        .unwrap();
        // Put the data in every storage state.
        if i == chunks.len() / 2 {
            node.flush_and_checkpoint().await.unwrap();
            let n = node.compact(now_ms()).await.unwrap();
            states.push(format!("after chunk {i}: flushed, committed, compacted {n} file group(s)"));
        } else if i == chunks.len() * 3 / 4 {
            node.flush_now().await.unwrap();
            states.push(format!("after chunk {i}: flushed, envelope not yet applied (rows still hot)"));
        }
    }
    states.push("last chunks: only in the hot tail (RAM)".into());

    let mut md = format!(
        "# End-to-end: TQS vs Prometheus on remote-written data\n\n\
         The same {} series / {samples} samples were remote-written to Prometheus 3.16 and to tiny-eye's HTTP API, \
         then each panel was compared at every step ({}..{} ms, step {} s). Equality: within {REL_TOL:e} relative; \
         a point on one side only is a failure.\n\nStorage states during the run:\n\n{}\n\n\
         | ID | Panel | PromQL | Result | Points | tiny-eye ms |\n| :--- | :--- | :--- | :--- | ---: | ---: |\n",
        series.len(),
        start + 10 * 60_000,
        end,
        STEP / 1000,
        states.iter().map(|s| format!("- {s}")).collect::<Vec<_>>().join("\n")
    );
    let mut failed = 0;
    for case in cases() {
        let (from, to) = (start + 10 * 60_000, end);
        let mut doc = json!({"tqs": "3", "id": case.id.to_lowercase(), "kind": if case.instant { "table" } else { "timeseries" },
            "time": {"from": from, "to": to, "step": format!("{}s", STEP / 1000)}, "queries": case.queries, "formulas": case.formulas});
        if case.instant {
            doc["time"] = json!({"from": end - 50 * 60_000, "to": end});
        }
        if let Some(t) = &case.top {
            doc["top"] = t.clone();
        }
        let promql = case.promql.to_string();
        let theirs = tokio::task::spawn_blocking(move || prom_query(&promql, from, to, case.instant)).await.unwrap();
        let (b, d2, out) = (base.clone(), doc.clone(), case.output);
        let (mine, ms) = tokio::task::spawn_blocking(move || tqs_query(&b, &d2, out)).await.unwrap();
        let mut bad = vec![];
        for (k, v) in &theirs {
            match mine.get(k) {
                Some(x) if equal(*x, *v) => {}
                Some(x) => bad.push(format!("{k:?}: prometheus {v} tiny-eye {x}")),
                None => bad.push(format!("{k:?}: prometheus {v}, tiny-eye has no point")),
            }
        }
        for (k, x) in &mine {
            if !theirs.contains_key(k) {
                bad.push(format!("{k:?}: tiny-eye {x}, prometheus has no point"));
            }
        }
        let pass = bad.is_empty() && !theirs.is_empty();
        if !pass {
            failed += 1;
            eprintln!("{} {} FAILED ({} of {} points; first: {:?})", case.id, case.what, bad.len(), theirs.len(), bad.first());
        }
        println!("[{}] {} {:<45} {} points, {:.1} ms", if pass { "PASS" } else { "FAIL" }, case.id, case.what, theirs.len(), ms);
        md.push_str(&format!(
            "| {} | {} | `{}` | {} | {} | {:.1} |\n",
            case.id,
            case.what,
            case.promql.replace('|', "\\|"),
            if pass { "PASS".to_string() } else { format!("**FAIL** {}", bad.len()) },
            theirs.len(),
            ms
        ));
    }
    let total = cases().len();
    md.push_str(&format!("\n**{}/{total} panels equal to Prometheus.**\n", total - failed));
    std::fs::create_dir_all(root.join("docs/deep-dive/evidence")).unwrap();
    std::fs::write(root.join("docs/deep-dive/evidence/e2e.md"), md).unwrap();
    assert_eq!(failed, 0, "{failed} panel(s) differ from Prometheus; see docs/deep-dive/evidence/e2e.md");
}
