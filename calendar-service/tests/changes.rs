mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, TestApp};

const CALS: &str = "/calendar/v1/calendars";

fn timed() -> Value {
    json!({"summary": "Dentist", "all_day": false, "start": "2026-10-05T09:00:00",
           "end": "2026-10-05T10:00:00", "tz": "Europe/Warsaw"})
}

async fn cal(app: &TestApp) -> String {
    let (_, _, b) = app.call(Method::GET, CALS, ALICE, None).await;
    b[0]["id"].as_str().unwrap().to_string()
}

async fn post(app: &TestApp, cal: &str, body: Value) -> Value {
    let (s, _, b) = app
        .call(
            Method::POST,
            &format!("{CALS}/{cal}/events"),
            ALICE,
            Some(body),
        )
        .await;
    assert_eq!(s, 201, "{b}");
    b
}

fn path(ev: &Value) -> String {
    format!("/calendar/v1/events/{}", ev["id"].as_str().unwrap())
}

async fn delete(app: &TestApp, ev: &Value) {
    let (s, _, _) = app.call(Method::DELETE, &path(ev), ALICE, None).await;
    assert_eq!(s, 204);
}

async fn changes(app: &TestApp, cal: &str, query: &str) -> (u16, Value) {
    let (s, _, b) = app
        .call(
            Method::GET,
            &format!("{CALS}/{cal}/changes{query}"),
            ALICE,
            None,
        )
        .await;
    (s.as_u16(), b)
}

fn ids(b: &Value) -> Vec<&str> {
    b["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn full_listing() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let a = post(&app, &cal, timed()).await;
    let b = post(&app, &cal, timed()).await;
    delete(&app, &a).await;
    let (s, got) = changes(&app, &cal, "").await;
    assert_eq!(s, 200);
    assert_eq!(got["sync_token"], 3);
    assert_eq!(ids(&got), [b["id"].as_str().unwrap()]);
    assert!(got["events"][0].get("deleted").is_none());
}

#[tokio::test]
async fn incremental() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let a = post(&app, &cal, timed()).await;
    let t = 1;
    let b = post(&app, &cal, timed()).await;
    let (s, _, _) = app.call(Method::PUT, &path(&a), ALICE, Some(timed())).await;
    assert_eq!(s, 200);
    let (s, got) = changes(&app, &cal, &format!("?since={t}")).await;
    assert_eq!(s, 200);
    assert_eq!(got["sync_token"], 3);
    // Ordered by revision: B (2), then A (3).
    assert_eq!(
        ids(&got),
        [b["id"].as_str().unwrap(), a["id"].as_str().unwrap()]
    );
    assert!(got["events"][0]["etag"].is_string());
}

#[tokio::test]
async fn tombstones() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let a = post(&app, &cal, timed()).await;
    delete(&app, &a).await;
    let (s, got) = changes(&app, &cal, "?since=1").await;
    assert_eq!(s, 200);
    assert_eq!(
        got["events"],
        json!([{"id": a["id"], "uid": a["uid"], "deleted": true}])
    );
}

#[tokio::test]
async fn created_and_deleted_since_is_one_tombstone() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let a = post(&app, &cal, timed()).await;
    delete(&app, &a).await;
    let (s, got) = changes(&app, &cal, "?since=0").await;
    assert_eq!(s, 200);
    assert_eq!(
        got["events"],
        json!([{"id": a["id"], "uid": a["uid"], "deleted": true}])
    );
}

#[tokio::test]
async fn series_stored_not_expanded() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(
        &app,
        &cal,
        json!({"summary": "Standup", "all_day": false,
        "start": "2026-10-05T09:00:00", "end": "2026-10-05T09:15:00", "tz": "Europe/Warsaw",
        "rrule": "FREQ=DAILY;COUNT=30"}),
    )
    .await;
    post(
        &app,
        &cal,
        json!({"summary": "Moved", "all_day": false,
        "start": "2026-10-06T10:00:00", "end": "2026-10-06T10:15:00", "tz": "Europe/Warsaw",
        "recurring_event_id": series["id"], "original_start": "2026-10-06T09:00:00"}),
    )
    .await;
    let (s, got) = changes(&app, &cal, "").await;
    assert_eq!(s, 200);
    assert_eq!(got["events"].as_array().unwrap().len(), 2);
    assert_eq!(got["events"][0]["rrule"], "FREQ=DAILY;COUNT=30");
}

#[tokio::test]
async fn deleting_a_series_tombstones_its_overrides() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(
        &app,
        &cal,
        json!({"summary": "Standup", "all_day": false,
        "start": "2026-10-05T09:00:00", "end": "2026-10-05T09:15:00", "tz": "Europe/Warsaw",
        "rrule": "FREQ=DAILY;COUNT=30"}),
    )
    .await;
    let over = post(
        &app,
        &cal,
        json!({"summary": "Moved", "all_day": false,
        "start": "2026-10-06T10:00:00", "end": "2026-10-06T10:15:00", "tz": "Europe/Warsaw",
        "recurring_event_id": series["id"], "original_start": "2026-10-06T09:00:00"}),
    )
    .await;
    delete(&app, &series).await;
    let (s, got) = changes(&app, &cal, "?since=2").await;
    assert_eq!(s, 200);
    let items = got["events"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        items
            .iter()
            .all(|e| e["deleted"] == true && e.as_object().unwrap().len() == 3)
    );
    let mut got_ids = ids(&got);
    got_ids.sort();
    let mut want = [series["id"].as_str().unwrap(), over["id"].as_str().unwrap()];
    want.sort();
    assert_eq!(got_ids, want);
}

#[tokio::test]
async fn nothing_new() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    post(&app, &cal, timed()).await;
    let (s, got) = changes(&app, &cal, "?since=1").await;
    assert_eq!(s, 200);
    assert_eq!(got, json!({"sync_token": 1, "events": []}));
}

#[tokio::test]
async fn future_token() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    post(&app, &cal, timed()).await;
    let (s, got) = changes(&app, &cal, "?since=2").await;
    assert_eq!(s, 410);
    assert_eq!(got["code"], "sync_token_invalid");
}

#[tokio::test]
async fn bad_since() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    for q in ["?since=abc", "?since=-1", "?since=", "?since=1.5"] {
        let (s, got) = changes(&app, &cal, q).await;
        assert_eq!((s, &got["code"]), (422, &json!("validation")), "{q}");
    }
}

#[tokio::test]
async fn isolation() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let (s, _, got) = app
        .call(Method::GET, &format!("{CALS}/{cal}/changes"), BOB, None)
        .await;
    assert_eq!(s, 404);
    assert_eq!(got["code"], "not_found");
}
