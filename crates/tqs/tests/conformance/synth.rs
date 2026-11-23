//! Synthetic telemetry with known ground truth.
//!
//! Traces cross regions (gateway/checkout in us-east call payment/ledger in eu-west)
//! and some straddle a flush boundary, which are the two cases a trace store gets wrong.
//! Counters have resets, and the generator keeps every sample so the true increase is
//! known even after an ingest node "crashes" and loses its unflushed batch.

use rand::{RngExt, SeedableRng, rngs::StdRng};

pub const WINDOW_MS: i64 = 60_000;
pub const WINDOWS: i64 = 3;
pub const NODES_PER_REGION: usize = 2;
pub const T0_MS: i64 = 1_762_740_000_000; // 2025-11-10T02:00:00Z, aligned to a minute

#[derive(Clone, Debug)]
pub struct Span {
    pub region: &'static str,
    pub trace_id: [u8; 16],
    pub span_id: u64,
    pub parent_span_id: Option<u64>,
    pub start_ns: i64,
    pub duration_ns: u64,
    pub service: &'static str,
    pub name: &'static str,
    pub status: u8,
    pub http_route: &'static str,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub region: &'static str,
    pub series_id: u64,
    pub ts_ms: i64,
    pub value: f64,
}

fn normal(rng: &mut StdRng) -> f64 {
    // Box-Muller; rand 0.9 ships no Normal distribution without rand_distr.
    let (u1, u2): (f64, f64) = (rng.random::<f64>().max(1e-12), rng.random());
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

fn lognormal_ns(rng: &mut StdRng, median_ms: f64) -> u64 {
    (median_ms * 1e6 * (0.6 * normal(rng)).exp()) as u64
}

pub fn traces(n: usize, seed: u64) -> Vec<Span> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(n * 6);
    let horizon_ns = WINDOW_MS * WINDOWS * 1_000_000 - 10_000_000_000;
    for _ in 0..n {
        let trace_id: [u8; 16] = rng.random();
        let root_start = T0_MS * 1_000_000 + rng.random_range(0..horizon_ns);
        let failed = rng.random_bool(0.03);
        let mut next_id = || rng.random::<u64>() | 1;
        let (gw, co, pay, led, db) = (next_id(), next_id(), next_id(), next_id(), next_id());
        let mut rng2 = StdRng::seed_from_u64(gw);
        let mut d = |m: f64| lognormal_ns(&mut rng2, m);
        let (d_led, d_db) = (d(8.0), d(3.0));
        let d_pay = d_led + d(40.0);
        let d_co = d_pay + d_db + d(15.0);
        // ~5 % of roots take seconds, so their later spans land in the next flush window.
        let d_gw = d_co + if failed || gw % 20 == 0 { 3_000_000_000 + d(500.0) } else { d(5.0) };
        let s = |region, span_id, parent, off: u64, dur, service, name, status| Span {
            region,
            trace_id,
            span_id,
            parent_span_id: parent,
            start_ns: root_start + off as i64,
            duration_ns: dur,
            service,
            name,
            status,
            http_route: "/api/checkout",
        };
        let pay_off = d_gw - d_co;
        out.push(s("us-east", gw, None, 0, d_gw, "gateway", "POST /api/checkout", if failed { 2 } else { 1 }));
        out.push(s("us-east", co, Some(gw), 1_000, d_co, "checkout", "checkout.place", 1));
        out.push(s("us-east", db, Some(co), 2_000, d_db, "checkout", "SELECT orders", 1));
        out.push(s("eu-west", pay, Some(co), pay_off, d_pay, "payment", "payment.charge", if failed { 2 } else { 1 }));
        out.push(s("eu-west", led, Some(pay), pay_off + 1_000, d_led, "ledger", "ledger.post", 1));
    }
    out
}

pub fn window_of_ns(ns: i64) -> i64 {
    ((ns / 1_000_000) - T0_MS).div_euclid(WINDOW_MS).clamp(0, WINDOWS - 1)
}

pub struct CounterSet {
    pub samples: Vec<Sample>,
    /// Prometheus-style increase from each series' first sample, using every sample.
    pub true_increase: std::collections::HashMap<u64, f64>,
}

pub const SCRAPE_MS: i64 = 5_000;

pub fn counters(series_per_region: usize, seed: u64) -> CounterSet {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut samples = Vec::new();
    let mut true_increase = std::collections::HashMap::new();
    for region in ["us-east", "eu-west"] {
        for s in 0..series_per_region {
            let labels = format!("region={region},service=svc{},code={},pod=pod-{}", s % 7, [200, 404, 500][s % 3], s);
            let series_id = crate::write::series_id(region, "http_requests_total", &labels);
            let mut value: f64 = rng.random_range(0.0..1e6f64).floor();
            let reset_at = if rng.random_bool(0.03) { Some(rng.random_range(3..30)) } else { None };
            let mut inc = 0.0;
            let mut ts = T0_MS + rng.random_range(0..SCRAPE_MS);
            let mut i = 0;
            while ts < T0_MS + WINDOW_MS * WINDOWS {
                if i > 0 {
                    if Some(i) == reset_at {
                        value = rng.random_range(0.0..5.0f64).floor();
                        inc += value;
                    } else {
                        let step = rng.random_range(0.0..40.0f64).floor();
                        value += step;
                        inc += step;
                    }
                }
                samples.push(Sample { region, series_id, ts_ms: ts, value });
                ts += SCRAPE_MS;
                i += 1;
            }
            true_increase.insert(series_id, inc);
        }
    }
    CounterSet { samples, true_increase }
}
