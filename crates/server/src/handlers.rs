//! One function per `operationId` in api/openapi.yaml. The generated router calls these; a
//! missing one is a compile error.

use crate::AppState;
use crate::api::types::*;
use crate::receivers;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use engine::query::QueryError;
use engine::{IngestError, Rows};

pub async fn healthz() -> &'static str {
    "ok\n"
}

pub async fn self_metrics(State(st): State<AppState>) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], st.node.metrics.render(st.node.hot_bytes())).into_response()
}

pub async fn openapi_spec() -> Response {
    ([(header::CONTENT_TYPE, "application/yaml")], crate::api::OPENAPI_YAML).into_response()
}

fn problems(ps: Vec<tqs::Problem>) -> ProblemResponse {
    ProblemResponse {
        valid: ps.is_empty(),
        problems: ps.into_iter().map(|p| Problem { code: p.code.into(), path: p.path, message: p.message }).collect(),
    }
}

pub async fn run_query(State(st): State<AppState>, body: String) -> Response {
    match st.node.query(&body).await {
        Ok(o) => Json(QueryResponse {
            tqs: "3".into(),
            id: o.id,
            snapshot_id: o.snapshot_id,
            freshness_s: o.freshness_s,
            partial_regions: vec![],
            series: o.series.into_iter().map(|s| Series { output: s.output, key: s.key, labels: s.labels, points: s.points }).collect(),
            rows: o.rows,
            rows_total: o.rows_total,
            truncated: o.truncated,
            elapsed_ms: o.elapsed_ms,
        })
        .into_response(),
        Err(QueryError::Invalid(ps)) => (StatusCode::BAD_REQUEST, Json(problems(ps))).into_response(),
        Err(QueryError::Failed(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("query failed: {e:#}\n")).into_response(),
    }
}

pub async fn validate_query(body: String) -> Json<ProblemResponse> {
    Json(problems(engine::Node::parse_and_validate(&body, engine::node::now_ms()).err().unwrap_or_default()))
}

pub async fn discover_catalog(State(st): State<AppState>) -> Response {
    match st.node.catalog().await {
        Ok(c) => Json(CatalogResponse {
            metrics: c
                .metrics
                .into_iter()
                .map(|m| CatalogMetric {
                    name: m.name,
                    r#type: m.r#type,
                    series: m.series,
                    label_keys: m.label_keys,
                    label_values: m.label_values,
                })
                .collect(),
            services: c
                .services
                .into_iter()
                .map(|s| CatalogService { name: s.name, span_names: s.span_names, log_lines: s.log_lines })
                .collect(),
            truncated: c.truncated,
        })
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response(),
    }
}

fn to_api(d: &engine::dashboards::Dashboard) -> Dashboard {
    Dashboard {
        id: d.id.clone(),
        title: d.title.clone(),
        vars: Some(d.vars.clone()),
        panels: d
            .panels
            .iter()
            .map(|p| Panel {
                query: p.query.clone(),
                ui: p.ui.as_ref().map(|u| PanelUi {
                    display: u.display.clone(),
                    unit: u.unit.clone(),
                    w: u.w.map(i64::from),
                    h: u.h.map(i64::from),
                }),
            })
            .collect(),
    }
}

pub async fn list_dashboards(State(st): State<AppState>) -> Json<Vec<DashboardSummary>> {
    let ds = st.dashboards.read().map(|d| d.clone()).unwrap_or_default();
    Json(ds.iter().map(|d| DashboardSummary { id: d.id.clone(), title: d.title.clone(), panels: d.panels.len() as i64 }).collect())
}

pub async fn get_dashboard(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let found = st.dashboards.read().ok().and_then(|ds| ds.iter().find(|d| d.id == id).cloned());
    match found {
        Some(d) => Json(to_api(&d)).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown dashboard\n").into_response(),
    }
}

/// The UI's new state of a dashboard → validated → `<dashboards dir>/<id>.yaml` → served.
pub async fn save_dashboard(State(st): State<AppState>, Path(id): Path<String>, body: String) -> Response {
    let bad = |ps: Vec<tqs::Problem>| (StatusCode::BAD_REQUEST, Json(problems(ps))).into_response();
    let d: engine::dashboards::Dashboard = match serde_json::from_str(&body) {
        Ok(d) => d,
        Err(e) => return bad(vec![tqs::Problem { code: "schema", path: "$".into(), message: e.to_string() }]),
    };
    if d.id != id {
        return bad(vec![tqs::Problem {
            code: "id",
            path: "id".into(),
            message: format!("body id {:?} differs from the URL id {id:?}", d.id),
        }]);
    }
    let dir = st.node.cfg.dashboards.dir.clone();
    let d2 = d.clone();
    match tokio::task::spawn_blocking(move || engine::dashboards::save(&dir, &d2)).await {
        Ok(Ok(path)) => {
            tracing::info!(dashboard = %d.id, file = %path.display(), "dashboard saved");
            if let Ok(mut ds) = st.dashboards.write() {
                match ds.iter_mut().find(|x| x.id == d.id) {
                    Some(slot) => *slot = d.clone(),
                    None => ds.push(d.clone()),
                }
            }
            Json(to_api(&d)).into_response()
        }
        Ok(Err(ps)) => bad(ps),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
    }
}

async fn admit(st: &AppState, rows: anyhow::Result<Rows>, ok: StatusCode) -> Response {
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e:#}\n")).into_response(),
    };
    match st.node.ingest(rows).await {
        Ok(()) => ok.into_response(),
        Err(IngestError::Overloaded) => (StatusCode::TOO_MANY_REQUESTS, "ingest arena full; retry later\n").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n")).into_response(),
    }
}

fn hdr(h: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

pub async fn prom_remote_write(State(st): State<AppState>, body: Bytes) -> Response {
    let (t, r) = (st.node.cfg.node.tenant.clone(), st.node.cfg.node.region.clone());
    admit(&st, receivers::prom_remote_write(&body, &t, &r), StatusCode::NO_CONTENT).await
}

pub async fn otlp_metrics(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let (t, r) = (st.node.cfg.node.tenant.clone(), st.node.cfg.node.region.clone());
    let rows = receivers::decompress(hdr(&headers, header::CONTENT_ENCODING), &body)
        .and_then(|b| receivers::otlp_metrics(hdr(&headers, header::CONTENT_TYPE), &b, &t, &r))
        .map(|(rows, skipped)| {
            if skipped > 0 {
                tracing::debug!(skipped, "OTLP points skipped (delta temporality / exponential histogram)");
            }
            rows
        });
    admit(&st, rows, StatusCode::OK).await
}

pub async fn otlp_traces(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let rows = receivers::decompress(hdr(&headers, header::CONTENT_ENCODING), &body)
        .and_then(|b| receivers::otlp_traces(hdr(&headers, header::CONTENT_TYPE), &b));
    admit(&st, rows, StatusCode::OK).await
}

pub async fn otlp_logs(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let rows = receivers::decompress(hdr(&headers, header::CONTENT_ENCODING), &body)
        .and_then(|b| receivers::otlp_logs(hdr(&headers, header::CONTENT_TYPE), &b));
    admit(&st, rows, StatusCode::OK).await
}

pub async fn loki_push(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let rows = receivers::decompress(hdr(&headers, header::CONTENT_ENCODING), &body).and_then(|b| receivers::loki_push(&b));
    admit(&st, rows, StatusCode::NO_CONTENT).await
}
