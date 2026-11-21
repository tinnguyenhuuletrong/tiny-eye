//! Dashboards round-trip through the API with the YAML file as the source of truth:
//! load at start → GET → the UI edits (layout, order) → PUT the new state → the file changes →
//! GET serves it. Invalid states are refused with problem codes and JSON paths, and nothing is
//! written.

#![allow(clippy::unwrap_used)]

use engine::{Config, Node};
use serde_json::{Value, json};

fn req(method: &str, url: &str, body: Option<&Value>) -> (u16, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut r = match (method, body) {
        ("GET", _) => agent.get(url).call().unwrap(),
        (_, Some(b)) => agent.put(url).header("Content-Type", "application/json").send(b.to_string()).unwrap(),
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
