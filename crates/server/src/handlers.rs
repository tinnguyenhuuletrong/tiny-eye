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
            metrics: c.metrics.into_iter().map(|m| CatalogMetric { name: m.name, series: m.series, label_keys: m.label_keys }).collect(),
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

pub async fn list_dashboards(State(st): State<AppState>) -> Json<Vec<DashboardSummary>> {
    Json(
        st.dashboards
            .iter()
            .map(|d| DashboardSummary { id: d.id.clone(), title: d.title.clone(), panels: d.panels.len() as i64 })
            .collect(),
    )
}

pub async fn get_dashboard(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.dashboards.iter().find(|d| d.id == id) {
        Some(d) => Json(Dashboard {
            id: d.id.clone(),
            title: d.title.clone(),
            vars: Some(d.vars.clone()),
            panels: d.panels.iter().map(|p| Panel { display: p.display.clone(), unit: p.unit.clone(), query: p.query.clone() }).collect(),
        })
        .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown dashboard\n").into_response(),
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
