//! Dashboards round-trip through the API with the YAML file as the source of truth:
//! load at start → GET → the UI edits (layout, order) → PUT the new state → the file changes →
//! GET serves it. Invalid states are refused with problem codes and JSON paths, and nothing is
//! written.

#![allow(clippy::unwrap_used)]

use engine::{Config, Node};
use serde_json::{Value, json};

fn req(method: &str, url: &str, body: Option<&Value>) -> (u16, Value) {
    req_as(None, method, url, body)
}

fn req_as(token: Option<&str>, method: &str, url: &str, body: Option<&Value>) -> (u16, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let auth = format!("Bearer {}", token.unwrap_or(""));
    let mut r = match (method, body) {
        ("GET", _) => agent.get(url).header("Authorization", &auth).call().unwrap(),
        (_, Some(b)) => {
            agent.put(url).header("Authorization", &auth).header("Content-Type", "application/json").send(b.to_string()).unwrap()
        }
        _ => unreachable!(),
    };
    let status = r.status().as_u16();
    let text = r.body_mut().read_to_string().unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_state_round_trips_through_the_yaml_file() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir().unwrap();
    let dash_dir = dir.path().join("dashboards");
    std::fs::create_dir_all(&dash_dir).unwrap();
    std::fs::copy(root.join("examples/demo/dashboards/shop.yaml"), dash_dir.join("shop.yaml")).unwrap();
    let d = dir.path().display();
    let yaml = format!(
        "duckdb: {{ extension_dir: {} }}\nstorage: {{ url: {d}/o }}\ncatalog: {{ url: {d}/c.ducklake }}\ningest: {{ wal_dir: {d}/w }}\ndashboards: {{ dir: {} }}\n",
        root.join(".tools/duckdb_ext").display(),
        dash_dir.display()
    );
    let cfg = Config::from_layers(Some(&yaml), &[]).unwrap();
    let dashboards = engine::dashboards::load(&cfg.dashboards.dir).unwrap();
    let node = Node::open(cfg).await.unwrap();
    let state = server::AppState::new(node, dashboards).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1/dashboards/shop", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server::app(state)).await.unwrap() });

    tokio::task::spawn_blocking(move || {
        // GET: the file as written, ui included, no dashboard vars merged into queries.
        let (s, mut dash) = req("GET", &base, None);
        assert_eq!(s, 200);
        assert_eq!(dash["panels"][0]["ui"], json!({"display": "stat", "unit": "req/s", "w": 3, "h": 2}));
        assert!(dash["panels"][0]["query"].get("vars").is_none());

        // The UI's new state: first panel wider and taller, last panel moved to the top.
        dash["panels"][0]["ui"]["w"] = json!(6);
        dash["panels"][0]["ui"]["h"] = json!(3);
        let panels = dash["panels"].as_array_mut().unwrap();
        let last = panels.pop().unwrap();
        panels.insert(0, last);
        let (s, saved) = req("PUT", &base, Some(&dash));
        assert_eq!(s, 200, "{saved}");
        let file = std::fs::read_to_string(dash_dir.join("shop.yaml")).unwrap();
        assert!(file.contains("w: 6") && file.contains("h: 3"), "the YAML file holds the new layout");
        assert!(file.starts_with("# yaml-language-server: $schema="), "the file keeps its own leading comments");
        assert!(file.find("slow-checkouts").unwrap() < file.find("id: requests").unwrap(), "and the new order");
        let (_, again) = req("GET", &base, None);
        assert_eq!(again, saved, "GET serves what was saved");
        assert_eq!(engine::dashboards::load(dash_dir.to_str().unwrap()).unwrap().len(), 1, "a restart loads the same file");

        // Invalid states: every problem with its path, nothing written.
        let before = std::fs::read_to_string(dash_dir.join("shop.yaml")).unwrap();
        let mut bad = again.clone();
        bad["panels"][1]["ui"]["w"] = json!(13);
        bad["panels"][1]["query"]["time"] = json!({"from": "-1h", "step": "0s"});
        let (s, p) = req("PUT", &base, Some(&bad));
        assert_eq!(s, 400);
        let paths: Vec<&str> = p["problems"].as_array().unwrap().iter().map(|x| x["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"panels[1].ui.w") && paths.iter().any(|x| x.starts_with("panels[1].query.time")), "{p}");
        let mut unknown = again.clone();
        unknown["panels"][0]["ui"]["colour"] = json!("red");
        assert_eq!(req("PUT", &base, Some(&unknown)).0, 400, "unknown ui keys are refused");
        let mut renamed = again.clone();
        renamed["id"] = json!("other");
        assert_eq!(req("PUT", &base, Some(&renamed)).0, 400, "body id must match the URL");
        assert_eq!(std::fs::read_to_string(dash_dir.join("shop.yaml")).unwrap(), before);
    })
    .await
    .unwrap();
}

/// A node serving `dash_dir` on a free port, with `tokens` (empty = auth off). Returns the
/// dashboards API base URL.
async fn start(dir: &std::path::Path, dash_dir: &std::path::Path, tokens: &[(&str, server::api::Scope)]) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let d = dir.display();
    let yaml = format!(
        "duckdb: {{ extension_dir: {} }}\nstorage: {{ url: {d}/o }}\ncatalog: {{ url: {d}/c.ducklake }}\ningest: {{ wal_dir: {d}/w }}\ndashboards: {{ dir: {} }}\n",
        root.join(".tools/duckdb_ext").display(),
        dash_dir.display()
    );
    let cfg = Config::from_layers(Some(&yaml), &[]).unwrap();
    let dashboards = engine::dashboards::load(&cfg.dashboards.dir).unwrap();
    let node = Node::open(cfg).await.unwrap();
    let mut state = server::AppState::new(node, dashboards).unwrap();
    state.tokens = std::sync::Arc::new(tokens.iter().map(|(t, s)| (t.to_string(), vec![*s])).collect());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1/dashboards", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server::app(state)).await.unwrap() });
    base
}

fn shop_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir().unwrap();
    let dash_dir = dir.path().join("dashboards");
    std::fs::create_dir_all(&dash_dir).unwrap();
    std::fs::copy(root.join("examples/demo/dashboards/shop.yaml"), dash_dir.join("shop.yaml")).unwrap();
    (dir, dash_dir)
}

/// Review finding: an unknown token is 401, a known token without the scope is 403.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_tokens_are_401_and_missing_scopes_403() {
    use server::api::Scope;
    let (dir, dash_dir) = shop_dir();
    let base = start(dir.path(), &dash_dir, &[("q-token", Scope::Query), ("a-token", Scope::Admin)]).await;
    tokio::task::spawn_blocking(move || {
        let url = format!("{base}/shop");
        assert_eq!(req_as(None, "GET", &url, None).0, 401, "no token");
        assert_eq!(req_as(Some("nope"), "GET", &url, None).0, 401, "unknown token");
        let (s, dash) = req_as(Some("q-token"), "GET", &url, None);
        assert_eq!(s, 200);
        let (s, body) = req_as(Some("q-token"), "PUT", &url, Some(&dash));
        assert_eq!(s, 403, "a query token cannot save: {body}");
        assert_eq!(req_as(Some("a-token"), "PUT", &url, Some(&dash)).0, 200, "admin can");
    })
    .await
    .unwrap();
}

/// Review findings: concurrent saves are serialised (file and memory agree, no shared temp
/// file), and a write failure is a 500, not a 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_saves_agree_and_io_failures_are_500() {
    let (dir, dash_dir) = shop_dir();
    let base = start(dir.path(), &dash_dir, &[]).await;
    let url = format!("{base}/shop");
    let u = url.clone();
    let dash = tokio::task::spawn_blocking(move || req("GET", &u, None).1).await.unwrap();
    let saves: Vec<_> = (1..=12)
        .map(|w| {
            let (u, mut d) = (url.clone(), dash.clone());
            d["panels"][0]["ui"]["w"] = json!(w);
            tokio::task::spawn_blocking(move || req("PUT", &u, Some(&d)))
        })
        .collect();
    for s in saves {
        let (status, body) = s.await.unwrap();
        assert_eq!(status, 200, "{body}");
    }
    let u = url.clone();
    let served = tokio::task::spawn_blocking(move || req("GET", &u, None).1).await.unwrap();
    let on_disk = engine::dashboards::load(dash_dir.to_str().unwrap()).unwrap();
    assert_eq!(on_disk.len(), 1);
    assert_eq!(json!(on_disk[0].panels[0].ui.as_ref().unwrap().w), served["panels"][0]["ui"]["w"], "memory and file agree");
    let names: Vec<String> = std::fs::read_dir(&dash_dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["shop.yaml".to_string()], "no temp files left");

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dash_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (status, body) = tokio::task::spawn_blocking(move || req("PUT", &url, Some(&served))).await.unwrap();
    std::fs::set_permissions(&dash_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, 500, "a read-only directory is the server's problem: {body}");
}
