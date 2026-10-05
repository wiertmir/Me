mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, TestApp};
use uuid::Uuid;

const LISTS: &str = "/tasks/v1/lists";

async fn new_list(app: &TestApp, who: Uuid) -> String {
    let (_, _, b) = app
        .call(Method::POST, LISTS, who, Some(json!({"name": "L"})))
        .await;
    b["id"].as_str().unwrap().to_string()
}

async fn try_post(app: &TestApp, who: Uuid, list: &str, body: Value) -> (u16, Value) {
    let (s, _, b) = app
        .call(
            Method::POST,
            &format!("{LISTS}/{list}/tasks"),
            who,
            Some(body),
        )
        .await;
    (s.as_u16(), b)
}

async fn post(app: &TestApp, list: &str, body: Value) -> Value {
    let (s, b) = try_post(app, ALICE, list, body).await;
    assert_eq!(s, 201, "{b}");
    b
}

fn path(task: &Value) -> String {
    format!("/tasks/v1/tasks/{}", task["id"].as_str().unwrap())
}

async fn put(app: &TestApp, task: &Value, body: Value) -> (u16, Value) {
    let (s, _, b) = app.call(Method::PUT, &path(task), ALICE, Some(body)).await;
    (s.as_u16(), b)
}

async fn status(app: &TestApp, method: Method, uri: &str) -> u16 {
    app.call(method, uri, ALICE, None).await.0.as_u16()
}

async fn sync_token(app: &TestApp, list: &str) -> i64 {
    let (_, _, b) = app
        .call(Method::GET, &format!("{LISTS}/{list}"), ALICE, None)
        .await;
    b["sync_token"].as_i64().unwrap()
}

#[tokio::test]
async fn create_subtask() {
    let app = TestApp::spawn().await;
    let l = new_list(&app, ALICE).await;
    let p = post(&app, &l, json!({"summary": "parent"})).await;
    let s = post(&app, &l, json!({"summary": "child", "parent_id": p["id"]})).await;
    assert_eq!(s["parent_id"], p["id"]);
}

#[tokio::test]
async fn refused_parents() {
    let app = TestApp::spawn().await;
    let l = new_list(&app, ALICE).await;
    let other = new_list(&app, ALICE).await;
    let p = post(&app, &l, json!({})).await;
    let sub = post(&app, &l, json!({"parent_id": p["id"]})).await;
    let elsewhere = post(&app, &other, json!({})).await;
    let gone = post(&app, &l, json!({})).await;
    assert_eq!(status(&app, Method::DELETE, &path(&gone)).await, 204);
    let bob_list = new_list(&app, BOB).await;
    let (_, bobs) = try_post(&app, BOB, &bob_list, json!({})).await;
    for parent in [
        sub["id"].clone(),
        elsewhere["id"].clone(),
        gone["id"].clone(),
        json!("7b0c8e1e-4a6f-4c63-9a53-0d6a2f1d9a11"),
        bobs["id"].clone(),
    ] {
        let (s, b) = try_post(&app, ALICE, &l, json!({"parent_id": parent})).await;
        assert_eq!(s, 422, "{parent}: {b}");
        assert_eq!(b["code"], "validation");
    }
}

#[tokio::test]
async fn subtask_cannot_repeat() {
    let app = TestApp::spawn().await;
    let l = new_list(&app, ALICE).await;
    let p = post(&app, &l, json!({})).await;
    let (s, _) = try_post(
        &app,
        ALICE,
        &l,
        json!({"parent_id": p["id"], "due": "2026-10-07", "rrule": "FREQ=DAILY"}),
    )
    .await;
    assert_eq!(s, 422);
    let sub = post(&app, &l, json!({"parent_id": p["id"], "due": "2026-10-07"})).await;
    let (s, _) = put(
        &app,
        &sub,
        json!({"due": "2026-10-07", "rrule": "FREQ=DAILY"}),
    )
    .await;
    assert_eq!(s, 422);
}

#[tokio::test]
async fn put_cannot_change_parent() {
    let app = TestApp::spawn().await;
    let l = new_list(&app, ALICE).await;
    let p = post(&app, &l, json!({})).await;
    let q = post(&app, &l, json!({})).await;
    let sub = post(&app, &l, json!({"parent_id": p["id"]})).await;
    let (s, _) = put(&app, &sub, json!({"parent_id": q["id"]})).await;
    assert_eq!(s, 422);
    let (s, _) = put(&app, &q, json!({"parent_id": p["id"]})).await;
    assert_eq!(s, 422);
    let (s, b) = put(&app, &sub, json!({"parent_id": p["id"], "summary": "x"})).await;
    assert_eq!(s, 200, "{b}");
}

#[tokio::test]
async fn deleting_a_parent_deletes_subtasks() {
    let app = TestApp::spawn().await;
    let l = new_list(&app, ALICE).await;
    let p = post(&app, &l, json!({})).await;
    let a = post(&app, &l, json!({"parent_id": p["id"]})).await;
    let b = post(&app, &l, json!({"parent_id": p["id"]})).await;
    let before = sync_token(&app, &l).await;
    assert_eq!(status(&app, Method::DELETE, &path(&p)).await, 204);
    for t in [&p, &a, &b] {
        assert_eq!(status(&app, Method::GET, &path(t)).await, 404);
    }
    assert_eq!(sync_token(&app, &l).await, before + 3);
}

#[tokio::test]
async fn completing_a_parent_leaves_subtasks() {
    let app = TestApp::spawn().await;
    let l = new_list(&app, ALICE).await;
    let p = post(&app, &l, json!({})).await;
    let a = post(&app, &l, json!({"parent_id": p["id"]})).await;
    let (s, _) = put(&app, &p, json!({"completed": true})).await;
    assert_eq!(s, 200);
    let (_, _, got) = app.call(Method::GET, &path(&a), ALICE, None).await;
    assert_eq!(got["completed"], false);
}

#[tokio::test]
async fn deleting_a_list_deletes_parents_and_subtasks() {
    let app = TestApp::spawn().await;
    let l = new_list(&app, ALICE).await;
    let p = post(&app, &l, json!({})).await;
    let a = post(&app, &l, json!({"parent_id": p["id"]})).await;
    assert_eq!(
        status(&app, Method::DELETE, &format!("{LISTS}/{l}")).await,
        204
    );
    for t in [&p, &a] {
        assert_eq!(status(&app, Method::GET, &path(t)).await, 404);
    }
}
