mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, TestApp};

const LISTS: &str = "/tasks/v1/lists";

async fn list(app: &TestApp) -> String {
    let (_, _, b) = app.call(Method::GET, LISTS, ALICE, None).await;
    b[0]["id"].as_str().unwrap().to_string()
}

async fn try_post(app: &TestApp, list: &str, body: Value) -> (u16, Value) {
    let (s, _, b) = app
        .call(
            Method::POST,
            &format!("{LISTS}/{list}/tasks"),
            ALICE,
            Some(body),
        )
        .await;
    (s.as_u16(), b)
}

async fn post(app: &TestApp, list: &str, body: Value) -> Value {
    let (s, b) = try_post(app, list, body).await;
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

#[tokio::test]
async fn create_and_read() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let (s, h, t) = app
        .call(
            Method::POST,
            &format!("{LISTS}/{l}/tasks"),
            ALICE,
            Some(json!({"summary": "Buy milk"})),
        )
        .await;
    assert_eq!(s, 201);
    assert_eq!(t["summary"], "Buy milk");
    for k in [
        "due",
        "tz",
        "completed_at",
        "parent_id",
        "rrule",
        "recurrence_id",
    ] {
        assert_eq!(t[k], Value::Null, "{k}");
    }
    assert_eq!(t["priority"], 0);
    assert_eq!(t["completed"], false);
    assert_eq!(t["reminders"], json!([]));
    assert_eq!(t["uid"], t["id"]);
    assert_eq!(t["etag"], "\"1\"");
    assert_eq!(h["etag"], t["etag"].as_str().unwrap());
    let (s, h, got) = app.call(Method::GET, &path(&t), ALICE, None).await;
    assert_eq!(s, 200);
    assert_eq!(got, t);
    assert_eq!(h["etag"], t["etag"].as_str().unwrap());
}

#[tokio::test]
async fn due_forms() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(&app, &l, json!({"due": "2026-10-07"})).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-07T09:00:00", "tz": "Europe/Warsaw"}),
    )
    .await;
    for bad in [
        json!({"due": "2026-10-07T09:00:00"}),
        json!({"due": "2026-10-07", "tz": "Europe/Warsaw"}),
        json!({"tz": "Europe/Warsaw"}),
        json!({"due": "2026-10-07T09:00:00", "tz": "Mars/Olympus"}),
        json!({"due": "1899-12-31"}),
        json!({"due": "2201-01-01"}),
    ] {
        let (s, b) = try_post(&app, &l, bad.clone()).await;
        assert_eq!((s, &b["code"]), (422, &json!("validation")), "{bad}");
    }
}

#[tokio::test]
async fn validation() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    for bad in [
        json!({"summary": "s".repeat(501)}),
        json!({"description": "d".repeat(10_001)}),
        json!({"priority": 10}),
        json!({"due": "2026-10-07", "reminders": [1, 2, 3, 4, 5, 6]}),
        json!({"due": "2026-10-07", "reminders": [40_321]}),
        json!({"reminders": [10]}),
        json!({"rrule": "FREQ=DAILY"}),
        json!({"due": "2026-10-07", "rrule": "FREQ=HOURLY"}),
        json!({"uid": ""}),
    ] {
        let (s, b) = try_post(&app, &l, bad.clone()).await;
        assert_eq!((s, &b["code"]), (422, &json!("validation")), "{bad}");
    }
}

#[tokio::test]
async fn duplicate_uid_is_conflict() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let first = post(&app, &l, json!({"uid": "x"})).await;
    assert_eq!(try_post(&app, &l, json!({"uid": "x"})).await.0, 409);
    app.call(Method::DELETE, &path(&first), ALICE, None).await;
    assert_eq!(try_post(&app, &l, json!({"uid": "x"})).await.0, 201);
}

#[tokio::test]
async fn completion() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let t = post(&app, &l, json!({"summary": "a"})).await;
    let (s, b) = put(&app, &t, json!({"summary": "a", "completed": true})).await;
    assert_eq!(s, 200);
    assert_eq!(b["completed"], true);
    assert_eq!(b["completed_at"], "2026-10-05T12:00:00Z");
    app.set_now("2026-10-06T00:00:00Z");
    let (_, b) = put(&app, &t, json!({"summary": "b", "completed": true})).await;
    assert_eq!(b["completed_at"], "2026-10-05T12:00:00Z");
    let (_, b) = put(&app, &t, json!({"summary": "b", "completed": false})).await;
    assert_eq!(
        (&b["completed"], &b["completed_at"]),
        (&json!(false), &Value::Null)
    );
    let done = post(&app, &l, json!({"completed": true})).await;
    assert_eq!(done["completed_at"], "2026-10-06T00:00:00Z");
}

#[tokio::test]
async fn put_cannot_change_uid() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let t = post(&app, &l, json!({"uid": "x"})).await;
    let (s, b) = put(&app, &t, json!({"uid": "y"})).await;
    assert_eq!((s, &b["code"]), (422, &json!("validation")));
    assert_eq!(put(&app, &t, json!({"uid": "x"})).await.0, 200);
}

#[tokio::test]
async fn if_match() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let t = post(&app, &l, json!({"summary": "a"})).await;
    let p = path(&t);
    let old = t["etag"].as_str().unwrap().to_string();
    let send = |m: Method, etag: Option<String>, body: Option<Value>| {
        let mut r = app.req(m, &p).bearer_auth(app.token(ALICE));
        if let Some(e) = etag {
            r = r.header("If-Match", e);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        async move {
            let resp = r.send().await.unwrap();
            (
                resp.status().as_u16(),
                resp.json::<Value>().await.unwrap_or(Value::Null),
            )
        }
    };
    let body = || Some(json!({"summary": "b"}));
    let (s, b) = send(Method::PUT, Some(old.clone()), body()).await;
    assert_eq!(s, 200);
    let new = b["etag"].as_str().unwrap().to_string();
    assert_ne!(new, old);
    let (s, b) = send(
        Method::PUT,
        Some(old.clone()),
        Some(json!({"summary": "c"})),
    )
    .await;
    assert_eq!((s, &b["code"]), (412, &json!("etag_mismatch")));
    let (_, _, got) = app.call(Method::GET, &p, ALICE, None).await;
    assert_eq!(got["summary"], "b");
    assert_eq!(send(Method::PUT, None, body()).await.0, 200);
    let (s, _, got) = app.call(Method::GET, &p, ALICE, None).await;
    assert_eq!(s, 200);
    let cur = got["etag"].as_str().unwrap().to_string();
    assert_eq!(send(Method::DELETE, Some(old), None).await.0, 412);
    assert_eq!(send(Method::DELETE, Some(cur), None).await.0, 204);
    let t2 = post(&app, &l, json!({})).await;
    let (s, _, _) = app.call(Method::DELETE, &path(&t2), ALICE, None).await;
    assert_eq!(s, 204);
}

#[tokio::test]
async fn delete_then_404() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let t = post(&app, &l, json!({})).await;
    let p = path(&t);
    let (s, _, _) = app.call(Method::DELETE, &p, ALICE, None).await;
    assert_eq!(s, 204);
    for m in [Method::GET, Method::PUT, Method::DELETE] {
        let body = (m == Method::PUT).then(|| json!({}));
        let (s, _, b) = app.call(m, &p, ALICE, body).await;
        assert_eq!((s.as_u16(), &b["code"]), (404, &json!("not_found")));
    }
}

#[tokio::test]
async fn rule_sets_recurrence_id() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let t = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    assert_eq!(t["recurrence_id"], t["id"]);
    let (s, b) = put(&app, &t, json!({"due": "2026-10-07", "rrule": null})).await;
    assert_eq!(s, 200);
    assert_eq!(b["rrule"], Value::Null);
    assert_eq!(b["recurrence_id"], t["id"]);
}

#[tokio::test]
async fn other_users_task_is_404() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let t = post(&app, &l, json!({})).await;
    for m in [Method::GET, Method::PUT, Method::DELETE] {
        let body = (m == Method::PUT).then(|| json!({}));
        let (s, _, b) = app.call(m, &path(&t), BOB, body).await;
        assert_eq!((s.as_u16(), &b["code"]), (404, &json!("not_found")));
    }
    let (s, _, b) = app
        .call(
            Method::POST,
            &format!("{LISTS}/{l}/tasks"),
            BOB,
            Some(json!({})),
        )
        .await;
    assert_eq!((s.as_u16(), &b["code"]), (404, &json!("not_found")));
}

#[tokio::test]
async fn ten_thousand_tasks_then_conflict() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    app.db()
        .execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
             INSERT INTO tasks (id, list_id, uid, summary, description, reminders, revision, created_at, updated_at)
             SELECT 'id' || i, '{l}', 'uid' || i, '', '', '[]', 1, 0, 0 FROM n"
        ))
        .unwrap();
    let (s, b) = try_post(&app, &l, json!({})).await;
    assert_eq!((s, &b["code"]), (409, &json!("conflict")));
}
