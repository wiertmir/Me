mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, TestApp};
use uuid::Uuid;

const CALS: &str = "/calendar/v1/calendars";

fn timed() -> Value {
    json!({"summary": "Dentist", "all_day": false, "start": "2026-10-05T09:00:00",
           "end": "2026-10-05T10:00:00", "tz": "Europe/Warsaw", "reminders": [10]})
}

/// `base` with `extra`'s fields laid over it.
fn with(mut base: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

async fn cal(app: &TestApp, user: uuid::Uuid) -> String {
    let (_, _, b) = app.call(Method::GET, CALS, user, None).await;
    b[0]["id"].as_str().unwrap().to_string()
}

async fn post(app: &TestApp, user: uuid::Uuid, cal: &str, body: Value) -> (u16, Value) {
    let (s, _, b) = app
        .call(
            Method::POST,
            &format!("{CALS}/{cal}/events"),
            user,
            Some(body),
        )
        .await;
    (s.as_u16(), b)
}

fn path(ev: &Value) -> String {
    format!("/calendar/v1/events/{}", ev["id"].as_str().unwrap())
}

async fn sync_token(app: &TestApp) -> i64 {
    let (_, _, b) = app.call(Method::GET, CALS, ALICE, None).await;
    b[0]["sync_token"].as_i64().unwrap()
}

#[tokio::test]
async fn create_and_read() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let (s, h, ev) = app
        .call(
            Method::POST,
            &format!("{CALS}/{cal}/events"),
            ALICE,
            Some(timed()),
        )
        .await;
    assert_eq!(s, 201);
    assert_eq!(ev["uid"], ev["id"]);
    assert_eq!(ev["etag"], "\"1\"");
    assert_eq!(h["etag"], "\"1\"");
    assert_eq!(ev["calendar_id"], cal.as_str());
    let (s, h, got) = app.call(Method::GET, &path(&ev), ALICE, None).await;
    assert_eq!((s.as_u16(), &got), (200, &ev));
    assert_eq!(h["etag"], "\"1\"");
}

#[tokio::test]
async fn all_day() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let body =
        json!({"summary": "Holiday", "all_day": true, "start": "2026-10-05", "end": "2026-10-06"});
    let (s, ev) = post(&app, ALICE, &cal, body).await;
    assert_eq!(s, 201);
    assert!(ev["tz"].is_null());
}

#[tokio::test]
async fn calendar_token_moves() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let (_, ev) = post(&app, ALICE, &cal, timed()).await;
    assert_eq!(sync_token(&app).await, 1);
    app.call(Method::PUT, &path(&ev), ALICE, Some(timed()))
        .await;
    assert_eq!(sync_token(&app).await, 2);
    app.call(Method::DELETE, &path(&ev), ALICE, None).await;
    assert_eq!(sync_token(&app).await, 3);
}

#[tokio::test]
async fn replace() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let (_, ev) = post(&app, ALICE, &cal, timed()).await;
    let (s, h, put) = app
        .call(
            Method::PUT,
            &path(&ev),
            ALICE,
            Some(with(timed(), json!({"summary": "Dentist!"}))),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(put["etag"], "\"2\"");
    assert_eq!(h["etag"], "\"2\"");
    let (_, _, got) = app.call(Method::GET, &path(&ev), ALICE, None).await;
    assert_eq!(got["summary"], "Dentist!");
}

#[tokio::test]
async fn if_match() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let (_, ev) = post(&app, ALICE, &cal, timed()).await;
    let p = path(&ev);
    app.call(Method::PUT, &p, ALICE, Some(timed())).await;
    let put = |m: Method, etag: &'static str, body: Option<Value>| {
        let mut r = app
            .req(m, &p)
            .bearer_auth(app.token(ALICE))
            .header("If-Match", etag);
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
    let (s, b) = put(
        Method::PUT,
        "\"1\"",
        Some(with(timed(), json!({"summary": "X"}))),
    )
    .await;
    assert_eq!((s, &b["code"]), (412, &json!("etag_mismatch")));
    let (_, _, got) = app.call(Method::GET, &p, ALICE, None).await;
    assert_eq!(got["summary"], "Dentist");
    let (s, _) = put(Method::PUT, " \"2\" ", Some(timed())).await;
    assert_eq!(s, 200);
    let (s, _) = put(Method::DELETE, "\"1\"", None).await;
    assert_eq!(s, 412);
    let (s, _, _) = app.call(Method::DELETE, &p, ALICE, None).await;
    assert_eq!(s, 204);
}

#[tokio::test]
async fn delete() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let (_, ev) = post(&app, ALICE, &cal, timed()).await;
    let p = path(&ev);
    let (s, _, _) = app.call(Method::DELETE, &p, ALICE, None).await;
    assert_eq!(s, 204);
    let (s, _, _) = app.call(Method::GET, &p, ALICE, None).await;
    assert_eq!(s, 404);
    let (s, _, _) = app.call(Method::PUT, &p, ALICE, Some(timed())).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn validation() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let bad = [
        with(timed(), json!({"end": "2026-10-05T09:00:00"})),
        with(timed(), json!({"end": "2026-10-05T08:00:00"})),
        with(timed(), json!({"tz": null})),
        json!({"summary": "a", "all_day": true, "start": "2026-10-05", "end": "2026-10-06", "tz": "Europe/Warsaw"}),
        with(timed(), json!({"tz": "Mars/Olympus"})),
        with(timed(), json!({"start": "2026-10-05"})),
        with(timed(), json!({"summary": "x".repeat(501)})),
        with(timed(), json!({"location": "x".repeat(501)})),
        with(timed(), json!({"description": "x".repeat(10_001)})),
        with(timed(), json!({"reminders": [1, 2, 3, 4, 5, 6]})),
        with(timed(), json!({"reminders": [40_321]})),
        with(timed(), json!({"rrule": "FREQ=HOURLY"})),
        with(
            timed(),
            json!({"rrule": format!("FREQ=DAILY;{}", "X".repeat(500))}),
        ),
        with(timed(), json!({"exdates": ["nope"]})),
        with(timed(), json!({"uid": ""})),
        with(timed(), json!({"uid": "u".repeat(256)})),
    ];
    for b in bad {
        let (s, e) = post(&app, ALICE, &cal, b.clone()).await;
        assert_eq!((s, &e["code"]), (422, &json!("validation")), "{b}");
    }
    let (s, _, e) = app
        .call(Method::POST, &format!("{CALS}/{cal}/events"), ALICE, None)
        .await;
    assert_eq!((s.as_u16(), &e["code"]), (422, &json!("validation")));
    // the limits themselves are fine, and the body limit does not get in the way
    let ok = with(
        timed(),
        json!({"description": "é".repeat(10_000), "summary": "s".repeat(500), "reminders": [0, 40_320]}),
    );
    assert_eq!(post(&app, ALICE, &cal, ok).await.0, 201);
}

#[tokio::test]
async fn start_in_dst_gap_is_accepted() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    // 02:30 does not exist and maps to 03:30 (01:30Z); end is judged on instants, so 03:30 is equal, not after
    let gap = |end: &str| with(timed(), json!({"start": "2027-03-28T02:30:00", "end": end}));
    assert_eq!(
        post(&app, ALICE, &cal, gap("2027-03-28T04:30:00")).await.0,
        201
    );
    assert_eq!(
        post(&app, ALICE, &cal, gap("2027-03-28T03:30:00")).await.0,
        422
    );
}

#[tokio::test]
async fn uid() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let b = with(timed(), json!({"uid": "abc@phone"}));
    let (s, ev) = post(&app, ALICE, &cal, b.clone()).await;
    assert_eq!((s, &ev["uid"]), (201, &json!("abc@phone")));
    let (s, e) = post(&app, ALICE, &cal, b.clone()).await;
    assert_eq!((s, &e["code"]), (409, &json!("conflict")));
    app.call(Method::DELETE, &path(&ev), ALICE, None).await;
    assert_eq!(post(&app, ALICE, &cal, b).await.0, 201);
}

#[tokio::test]
async fn override_rules() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let (_, series) = post(
        &app,
        ALICE,
        &cal,
        with(timed(), json!({"rrule": "FREQ=WEEKLY"})),
    )
    .await;
    let sid = series["id"].clone();
    let ov = with(
        timed(),
        json!({"start": "2026-10-12T11:00:00", "end": "2026-10-12T12:00:00",
               "recurring_event_id": sid, "original_start": "2026-10-12T09:00:00"}),
    );
    let (s, o) = post(&app, ALICE, &cal, ov.clone()).await;
    assert_eq!((s, &o["uid"]), (201, &series["uid"]));
    let (s, e) = post(&app, ALICE, &cal, ov.clone()).await;
    assert_eq!((s, &e["code"]), (409, &json!("conflict")));
    let (s, _) = post(
        &app,
        ALICE,
        &cal,
        with(ov.clone(), json!({"rrule": "FREQ=DAILY"})),
    )
    .await;
    assert_eq!(s, 422);
    let (s, _) = post(
        &app,
        ALICE,
        &cal,
        with(timed(), json!({"original_start": "2026-10-12T09:00:00"})),
    )
    .await;
    assert_eq!(s, 422);
    let (_, plain) = post(&app, ALICE, &cal, timed()).await;
    let (s, _) = post(
        &app,
        ALICE,
        &cal,
        with(ov.clone(), json!({"recurring_event_id": plain["id"]})),
    )
    .await;
    assert_eq!(s, 422);
    // a series in another calendar
    let (_, _, other) = app
        .call(Method::POST, CALS, ALICE, Some(json!({"name": "Work"})))
        .await;
    let other = other["id"].as_str().unwrap();
    let (_, there) = post(
        &app,
        ALICE,
        other,
        with(timed(), json!({"rrule": "FREQ=WEEKLY"})),
    )
    .await;
    let (s, _) = post(
        &app,
        ALICE,
        &cal,
        with(ov, json!({"recurring_event_id": there["id"]})),
    )
    .await;
    assert_eq!(s, 422);
}

#[tokio::test]
async fn immutable_fields() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    let (_, ev) = post(&app, ALICE, &cal, timed()).await;
    let (s, _, e) = app
        .call(
            Method::PUT,
            &path(&ev),
            ALICE,
            Some(with(timed(), json!({"uid": "other"}))),
        )
        .await;
    assert_eq!((s.as_u16(), &e["code"]), (422, &json!("validation")));
    let (s, _, _) = app
        .call(
            Method::PUT,
            &path(&ev),
            ALICE,
            Some(with(timed(), json!({"uid": ev["uid"]}))),
        )
        .await;
    assert_eq!(s, 200);
}

async fn series_with_override(app: &TestApp) -> (String, Value, Value) {
    let cal = cal(app, ALICE).await;
    let (_, series) = post(
        app,
        ALICE,
        &cal,
        with(timed(), json!({"rrule": "FREQ=WEEKLY"})),
    )
    .await;
    let ov = with(
        timed(),
        json!({"recurring_event_id": series["id"], "original_start": "2026-10-12T09:00:00"}),
    );
    let (s, o) = post(app, ALICE, &cal, ov).await;
    assert_eq!(s, 201);
    (cal, series, o)
}

#[tokio::test]
async fn delete_series_deletes_overrides() {
    let app = TestApp::spawn().await;
    let (_, series, o) = series_with_override(&app).await;
    let (s, _, _) = app.call(Method::DELETE, &path(&series), ALICE, None).await;
    assert_eq!(s, 204);
    let (s, _, _) = app.call(Method::GET, &path(&o), ALICE, None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn deleting_a_calendar_deletes_its_events() {
    let app = TestApp::spawn().await;
    let (cal, series, o) = series_with_override(&app).await;
    let (s, _, _) = app
        .call(Method::DELETE, &format!("{CALS}/{cal}"), ALICE, None)
        .await;
    assert_eq!(s, 204);
    for ev in [&series, &o] {
        let (s, _, _) = app.call(Method::GET, &path(ev), ALICE, None).await;
        assert_eq!(s, 404);
    }
}

#[tokio::test]
async fn isolation() {
    let app = TestApp::spawn().await;
    let cal = cal(&app, ALICE).await;
    assert_eq!(post(&app, BOB, &cal, timed()).await.0, 404);
    let (_, ev) = post(&app, ALICE, &cal, timed()).await;
    let p = path(&ev);
    for (m, body) in [
        (Method::GET, None),
        (Method::PUT, Some(timed())),
        (Method::DELETE, None),
    ] {
        let (s, _, e) = app.call(m, &p, BOB, body).await;
        assert_eq!((s.as_u16(), &e["code"]), (404, &json!("not_found")));
    }
    let (s, _, _) = app.call(Method::GET, &p, ALICE, None).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn times_must_be_canonical() {
    let app = TestApp::spawn().await;
    let (cal, series, _) = series_with_override(&app).await;
    let (s, _) = post(
        &app,
        ALICE,
        &cal,
        with(timed(), json!({"start": "2026-10-05T9:00:00"})),
    )
    .await;
    assert_eq!(s, 422);
    let ov = with(
        timed(),
        json!({"recurring_event_id": series["id"], "original_start": "2026-10-12T9:00:00"}),
    );
    assert_eq!(post(&app, ALICE, &cal, ov).await.0, 422);
}

#[tokio::test]
async fn series_edits_keep_overrides() {
    let app = TestApp::spawn().await;
    let (_, series, o) = series_with_override(&app).await;
    let (s, _, _) = app
        .call(Method::PUT, &path(&series), ALICE, Some(timed()))
        .await;
    assert_eq!(s, 200);
    let (s, _, got) = app.call(Method::GET, &path(&o), ALICE, None).await;
    assert_eq!((s.as_u16(), &got), (200, &o));
}

#[tokio::test]
async fn override_rules_on_write() {
    let app = TestApp::spawn().await;
    let (cal, series, o) = series_with_override(&app).await;
    let ov = |extra: Value| {
        with(
            timed(),
            with(
                json!({"recurring_event_id": series["id"], "original_start": "2026-10-19T09:00:00"}),
                extra,
            ),
        )
    };
    let (s, _) = post(&app, ALICE, &cal, ov(json!({"uid": "other"}))).await;
    assert_eq!(s, 422);
    let (s, _) = post(
        &app,
        ALICE,
        &cal,
        ov(json!({"all_day": true, "start": "2026-10-19", "end": "2026-10-20", "tz": null, "original_start": "2026-10-19"})),
    )
    .await;
    assert_eq!(s, 422);
    let put_o = |extra: Value| {
        let (app, p, body) = (&app, path(&o), with(timed(), extra));
        async move { app.call(Method::PUT, &p, ALICE, Some(body)).await }
    };
    for extra in [
        json!({"rrule": "FREQ=DAILY"}),
        json!({"exdates": ["2026-10-19T09:00:00"]}),
        json!({"all_day": true, "start": "2026-10-12", "end": "2026-10-13", "tz": null}),
        json!({"recurring_event_id": Uuid::new_v4()}),
        json!({"original_start": "2026-10-19T09:00:00"}),
    ] {
        let (s, _, e) = put_o(extra.clone()).await;
        assert_eq!(
            (s.as_u16(), &e["code"]),
            (422, &json!("validation")),
            "{extra}"
        );
    }
}
