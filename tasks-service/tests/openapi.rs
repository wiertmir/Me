mod support;
use std::collections::BTreeSet;

use serde_json::Value;
use support::TestApp;

/// Every (method, path) the service serves, except `/api/openapi.json` and `/api/docs`.
const ROUTES: &[(&str, &str)] = &[
    ("get", "/health"),
    ("get", "/tasks/v1/lists"),
    ("post", "/tasks/v1/lists"),
    ("get", "/tasks/v1/lists/{id}"),
    ("patch", "/tasks/v1/lists/{id}"),
    ("delete", "/tasks/v1/lists/{id}"),
    ("post", "/tasks/v1/lists/{id}/tasks"),
    ("get", "/tasks/v1/lists/{id}/changes"),
    ("get", "/tasks/v1/tasks/{id}"),
    ("put", "/tasks/v1/tasks/{id}"),
    ("delete", "/tasks/v1/tasks/{id}"),
];

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

async fn doc(app: &TestApp) -> Value {
    app.http
        .get(format!("{}/api/openapi.json", app.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn every_route_is_documented() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    let mut documented = BTreeSet::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for m in METHODS {
            if item.get(m).is_some() {
                documented.insert((m.to_string(), path.clone()));
            }
        }
    }
    let expected: BTreeSet<(String, String)> = ROUTES
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    assert_eq!(
        expected.difference(&documented).collect::<Vec<_>>(),
        Vec::<&(String, String)>::new(),
        "listed but not documented"
    );
    assert_eq!(
        documented.difference(&expected).collect::<Vec<_>>(),
        Vec::<&(String, String)>::new(),
        "documented but not listed"
    );
}

#[tokio::test]
async fn task_operations_take_both_credentials() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    let mut seen = 0;
    for (path, item) in doc["paths"].as_object().unwrap() {
        for m in METHODS {
            let Some(op) = item.get(m).filter(|_| path.starts_with("/tasks/v1")) else {
                continue;
            };
            seen += 1;
            let schemes: BTreeSet<&str> = op["security"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|req| req.as_object().unwrap().keys())
                .map(String::as_str)
                .collect();
            assert_eq!(
                schemes,
                BTreeSet::from(["access_token", "service_secret"]),
                "{m} {path}"
            );
            // a bearer token is checked against keys that may be unreachable
            assert!(op["responses"]["503"].is_object(), "{m} {path}");
        }
    }
    assert_eq!(seen, ROUTES.len() - 1);
}

#[tokio::test]
async fn document_metadata() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    assert_eq!(doc["info"]["title"], "Me tasks-service");
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    let s = &doc["components"]["securitySchemes"];
    assert_eq!(s["service_secret"]["type"], "apiKey");
    assert_eq!(s["service_secret"]["in"], "header");
    assert_eq!(s["service_secret"]["name"], "X-Service-Secret");
    assert_eq!(s["access_token"]["scheme"], "bearer");
    assert_eq!(s["access_token"]["bearerFormat"], "JWT");
    assert!(doc["components"]["schemas"]["ErrorBody"].is_object());
}
