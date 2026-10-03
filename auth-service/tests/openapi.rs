mod support;
use std::collections::BTreeSet;

use serde_json::Value;
use support::TestApp;

/// Every (method, path) the service serves, except `/api/openapi.json` and `/api/docs`.
const ROUTES: &[(&str, &str)] = &[
    ("get", "/health"),
    ("post", "/api/signin"),
    ("post", "/api/signout"),
    ("post", "/api/password/change"),
    ("post", "/api/signup"),
    ("post", "/api/email/verify"),
    ("post", "/api/email/resend"),
    ("post", "/api/password/forgot"),
    ("post", "/api/password/reset"),
    ("get", "/api/me"),
    ("patch", "/api/me"),
    ("get", "/api/me/sessions"),
    ("delete", "/api/me/sessions/{id}"),
    ("get", "/api/me/identities"),
    ("delete", "/api/me/identities/{provider}"),
    ("post", "/api/me/identities/confirm"),
    ("get", "/api/admin/users"),
    ("post", "/api/admin/users"),
    ("patch", "/api/admin/users/{id}"),
    ("post", "/api/admin/users/{id}/reset-password"),
    ("get", "/oauth/authorize"),
    ("get", "/api/auth-requests/{challenge}"),
    ("post", "/api/auth-requests/{challenge}/accept"),
    ("get", "/.well-known/openid-configuration"),
    ("get", "/.well-known/jwks.json"),
    ("post", "/oauth/token"),
    ("get", "/oauth/userinfo"),
    ("post", "/oauth/revoke"),
    ("get", "/api/providers"),
    ("post", "/api/social/link-intent"),
    ("post", "/api/social/exchange"),
    ("get", "/social/{provider}/start"),
    ("get", "/social/{provider}/callback"),
    ("get", "/api/me/app-passwords"),
    ("post", "/api/me/app-passwords"),
    ("delete", "/api/me/app-passwords/{id}"),
    ("post", "/api/app-passwords/verify"),
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

fn operations(doc: &Value) -> Vec<(String, String, &Value)> {
    let mut out = vec![];
    for (path, item) in doc["paths"].as_object().unwrap() {
        for m in METHODS {
            if let Some(op) = item.get(m) {
                out.push((m.to_string(), path.clone(), op));
            }
        }
    }
    out
}

#[tokio::test]
async fn every_route_is_documented() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    let documented: BTreeSet<(String, String)> = operations(&doc)
        .into_iter()
        .map(|(m, p, _)| (m, p))
        .collect();
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
async fn document_metadata() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    assert_eq!(doc["info"]["title"], "Me auth-service");
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    let tags: BTreeSet<&str> = doc["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "auth",
        "account",
        "admin",
        "oauth",
        "social",
        "app-passwords",
    ] {
        assert!(tags.contains(t), "missing tag {t}");
    }
    let s = &doc["components"]["securitySchemes"];
    assert_eq!(s["service_secret"]["type"], "apiKey");
    assert_eq!(s["service_secret"]["in"], "header");
    assert_eq!(s["service_secret"]["name"], "X-Service-Secret");
    assert_eq!(s["session"]["scheme"], "bearer");
    assert_eq!(s["access_token"]["scheme"], "bearer");
    assert_eq!(s["access_token"]["bearerFormat"], "JWT");
    assert!(doc["components"]["schemas"]["ErrorBody"].is_object());
    assert!(doc["components"]["schemas"]["OAuthError"].is_object());
}

#[tokio::test]
async fn operations_have_summary_security_and_error_schema() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    for (m, p, op) in operations(&doc) {
        let id = format!("{m} {p}");
        assert!(
            !op["summary"].as_str().unwrap_or_default().is_empty(),
            "{id}: summary"
        );
        if p != "/health" {
            assert!(
                !op["tags"].as_array().unwrap_or(&vec![]).is_empty(),
                "{id}: tags"
            );
        }
        let sec = op["security"]
            .as_array()
            .unwrap_or_else(|| panic!("{id}: security"));
        let names: BTreeSet<&str> = sec
            .iter()
            .flat_map(|r| r.as_object().unwrap().keys().map(String::as_str))
            .collect();
        if p == "/oauth/userinfo" {
            assert_eq!(names, BTreeSet::from(["access_token"]), "{id}");
        } else if p.starts_with("/api/") {
            assert!(names.contains("service_secret"), "{id}: service_secret");
            let session_routes = [
                "/api/signout",
                "/api/password/change",
                "/api/me",
                "/api/me/sessions",
                "/api/me/sessions/{id}",
                "/api/me/identities",
                "/api/me/identities/{provider}",
                "/api/me/identities/confirm",
                "/api/admin/users",
                "/api/admin/users/{id}",
                "/api/admin/users/{id}/reset-password",
                "/api/auth-requests/{challenge}/accept",
                "/api/social/link-intent",
                "/api/me/app-passwords",
                "/api/me/app-passwords/{id}",
            ];
            assert_eq!(
                names.contains("session"),
                session_routes.contains(&p.as_str()),
                "{id}: session"
            );
            // Both schemes are required together: a single requirement object.
            assert_eq!(sec.len(), 1, "{id}");
            // Every 4xx/5xx is the shared ErrorBody.
            let responses = op["responses"].as_object().unwrap();
            let errors: Vec<_> = responses
                .iter()
                .filter(|(c, _)| c.starts_with(['4', '5']))
                .collect();
            assert!(!errors.is_empty(), "{id}: no error responses");
            for (code, r) in errors {
                let r_ref = r["content"]["application/json"]["schema"]["$ref"].as_str();
                assert_eq!(r_ref, Some("#/components/schemas/ErrorBody"), "{id} {code}");
                assert!(
                    !r["description"].as_str().unwrap_or_default().is_empty(),
                    "{id} {code}: description"
                );
            }
        } else {
            assert_eq!(
                sec,
                &vec![serde_json::json!({})],
                "{id}: public operations declare no security"
            );
        }
    }
}

#[tokio::test]
async fn request_bodies_have_an_example_and_oauth_errors_use_their_schema() {
    let app = TestApp::spawn().await;
    let doc = doc(&app).await;
    for (m, p, op) in operations(&doc) {
        if let Some(body) = op.get("requestBody") {
            let content = body["content"].as_object().unwrap();
            let (ct, c) = content.iter().next().unwrap();
            let schema = &c["schema"];
            let name = schema["$ref"].as_str().unwrap().rsplit('/').next().unwrap();
            let has = c.get("example").is_some()
                || c.get("examples").is_some()
                || doc["components"]["schemas"][name].get("example").is_some()
                || doc["components"]["schemas"][name].get("examples").is_some();
            assert!(has, "{m} {p} ({ct}): request example");
        }
    }
    let token = &doc["paths"]["/oauth/token"]["post"]["responses"]["400"];
    assert_eq!(
        token["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/OAuthError"
    );
}

#[tokio::test]
async fn docs_page_loads() {
    let app = TestApp::spawn().await;
    let r = app
        .http
        .get(format!("{}/api/docs", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(r.text().await.unwrap().contains("/api/openapi.json"));
}
