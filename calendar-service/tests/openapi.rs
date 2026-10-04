mod support;
use std::collections::BTreeSet;

use serde_json::Value;
use support::TestApp;

/// Every (method, path) the service serves, except `/api/openapi.json` and `/api/docs`.
const ROUTES: &[(&str, &str)] = &[
    ("get", "/health"),
    ("get", "/calendar/v1/calendars"),
    ("post", "/calendar/v1/calendars"),
    ("get", "/calendar/v1/calendars/{id}"),
    ("patch", "/calendar/v1/calendars/{id}"),
    ("delete", "/calendar/v1/calendars/{id}"),
    ("post", "/calendar/v1/calendars/{id}/events"),
    ("get", "/calendar/v1/calendars/{id}/events"),
    ("get", "/calendar/v1/calendars/{id}/changes"),
    ("get", "/calendar/v1/events/{id}"),
    ("put", "/calendar/v1/events/{id}"),
    ("delete", "/calendar/v1/events/{id}"),
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
async fn calendar_operations_take_both_credentials() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    let mut seen = 0;
    for (path, item) in doc["paths"].as_object().unwrap() {
        for m in METHODS {
            let Some(op) = item.get(m).filter(|_| path.starts_with("/calendar/v1")) else {
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
async fn event_schema_and_headers() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    assert!(
        doc["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "events")
    );
    let required = doc["components"]["schemas"]["Event"]["required"]
        .as_array()
        .unwrap();
    for f in ["tz", "rrule", "recurring_event_id", "original_start"] {
        assert!(required.iter().any(|r| r == f), "{f}");
        let ty = &doc["components"]["schemas"]["Event"]["properties"][f]["type"];
        assert!(ty.as_array().unwrap().iter().any(|t| t == "null"), "{f}");
    }
    for (m, path, status) in [
        ("post", "/calendar/v1/calendars/{id}/events", "201"),
        ("get", "/calendar/v1/events/{id}", "200"),
        ("put", "/calendar/v1/events/{id}", "200"),
    ] {
        let r = &doc["paths"][path][m]["responses"][status];
        assert!(r["headers"]["ETag"].is_object(), "{m} {path}");
    }
}

#[tokio::test]
async fn document_metadata() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    assert_eq!(doc["info"]["title"], "Me calendar-service");
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    let s = &doc["components"]["securitySchemes"];
    assert_eq!(s["service_secret"]["type"], "apiKey");
    assert_eq!(s["service_secret"]["in"], "header");
    assert_eq!(s["service_secret"]["name"], "X-Service-Secret");
    assert_eq!(s["access_token"]["scheme"], "bearer");
    assert_eq!(s["access_token"]["bearerFormat"], "JWT");
    assert!(doc["components"]["schemas"]["ErrorBody"].is_object());
}
