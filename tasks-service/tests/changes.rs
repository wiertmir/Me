mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, TestApp};

const LISTS: &str = "/tasks/v1/lists";

async fn list(app: &TestApp) -> String {
    let (_, _, b) = app.call(Method::GET, LISTS, ALICE, None).await;
    b[0]["id"].as_str().unwrap().to_string()
}

async fn post(app: &TestApp, list: &str) -> Value {
    let (s, _, b) = app
        .call(
            Method::POST,
            &format!("{LISTS}/{list}/tasks"),
            ALICE,
            Some(json!({"summary": "Buy milk"})),
        )
        .await;
    assert_eq!(s, 201, "{b}");
    b
}

fn path(task: &Value) -> String {
    format!("/tasks/v1/tasks/{}", task["id"].as_str().unwrap())
}

async fn delete(app: &TestApp, task: &Value) {
    let (s, _, _) = app.call(Method::DELETE, &path(task), ALICE, None).await;
    assert_eq!(s, 204);
}

async fn changes(app: &TestApp, list: &str, query: &str) -> (u16, Value) {
    let (s, _, b) = app
        .call(
            Method::GET,
            &format!("{LISTS}/{list}/changes{query}"),
            ALICE,
            None,
        )
        .await;
    (s.as_u16(), b)
}

fn ids(b: &Value) -> Vec<&str> {
    b["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn full_listing_has_live_tasks_only() {
    let app = TestApp::spawn().await;
    let list = list(&app).await;
    let a = post(&app, &list).await;
    let b = post(&app, &list).await;
    let c = post(&app, &list).await;
    delete(&app, &b).await;
    let (s, got) = changes(&app, &list, "").await;
    assert_eq!(s, 200);
    assert_eq!(got["sync_token"], 4);
    assert_eq!(
        ids(&got),
        [a["id"].as_str().unwrap(), c["id"].as_str().unwrap()]
    );
    assert!(got["tasks"][0].get("deleted").is_none());
}

#[tokio::test]
async fn incremental() {
    let app = TestApp::spawn().await;
    let list = list(&app).await;
    let a = post(&app, &list).await;
    let b = post(&app, &list).await;
    let (_, got) = changes(&app, &list, "").await;
    let t = got["sync_token"].as_i64().unwrap();
    let c = post(&app, &list).await;
    let (s, _, _) = app
        .call(Method::PUT, &path(&a), ALICE, Some(json!({"summary": "x"})))
        .await;
    assert_eq!(s, 200);
    delete(&app, &b).await;
    let (s, got) = changes(&app, &list, &format!("?since={t}")).await;
    assert_eq!(s, 200);
    assert_eq!(got["sync_token"], t + 3);
    // Ordered by revision: C, then A, then B's tombstone.
    assert_eq!(
        ids(&got),
        [
            c["id"].as_str().unwrap(),
            a["id"].as_str().unwrap(),
            b["id"].as_str().unwrap()
        ]
    );
    assert!(got["tasks"][0]["etag"].is_string());
    assert_eq!(
        got["tasks"][2],
        json!({"id": b["id"], "uid": b["uid"], "deleted": true})
    );
}

#[tokio::test]
async fn since_current_is_empty() {
    let app = TestApp::spawn().await;
    let list = list(&app).await;
    post(&app, &list).await;
    let (s, got) = changes(&app, &list, "?since=1").await;
    assert_eq!(s, 200);
    assert_eq!(got, json!({"sync_token": 1, "tasks": []}));
}

#[tokio::test]
async fn since_from_the_future_is_410() {
    let app = TestApp::spawn().await;
    let list = list(&app).await;
    post(&app, &list).await;
    let (s, got) = changes(&app, &list, "?since=2").await;
    assert_eq!(s, 410);
    assert_eq!(got["code"], "sync_token_invalid");
}

#[tokio::test]
async fn bad_since_is_422() {
    let app = TestApp::spawn().await;
    let list = list(&app).await;
    for q in ["?since=abc", "?since=-1"] {
        let (s, got) = changes(&app, &list, q).await;
        assert_eq!((s, &got["code"]), (422, &json!("validation")), "{q}");
    }
}

#[tokio::test]
async fn other_users_list_is_404() {
    let app = TestApp::spawn().await;
    let list = list(&app).await;
    let (s, _, got) = app
        .call(Method::GET, &format!("{LISTS}/{list}/changes"), BOB, None)
        .await;
    assert_eq!(s, 404);
    assert_eq!(got["code"], "not_found");
}

#[tokio::test]
async fn since_zero_includes_tombstones() {
    let app = TestApp::spawn().await;
    let list = list(&app).await;
    let a = post(&app, &list).await;
    let b = post(&app, &list).await;
    delete(&app, &b).await;
    let (s, got) = changes(&app, &list, "?since=0").await;
    assert_eq!(s, 200);
    assert_eq!(got["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(
        got["tasks"][1],
        json!({"id": b["id"], "uid": b["uid"], "deleted": true})
    );
    let (_, got) = changes(&app, &list, "").await;
    assert_eq!(ids(&got), [a["id"].as_str().unwrap()]);
}
