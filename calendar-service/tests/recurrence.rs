mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, TestApp};

const CALS: &str = "/calendar/v1/calendars";

fn timed(start: &str, end: &str) -> Value {
    json!({"summary": "x", "all_day": false, "start": start, "end": end, "tz": "Europe/Warsaw"})
}

fn all_day(start: &str, end: &str) -> Value {
    json!({"summary": "x", "all_day": true, "start": start, "end": end})
}

fn with(mut base: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

fn weekly() -> Value {
    with(
        timed("2026-10-19T09:00:00", "2026-10-19T10:00:00"),
        json!({"rrule": "FREQ=WEEKLY"}),
    )
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

async fn put(app: &TestApp, ev: &Value, body: Value) {
    let (s, _, b) = app
        .call(
            Method::PUT,
            &format!("/calendar/v1/events/{}", ev["id"].as_str().unwrap()),
            ALICE,
            Some(body),
        )
        .await;
    assert_eq!(s, 200, "{b}");
}

/// `query` is sent as written (the caller encodes).
async fn range_raw(app: &TestApp, cal: &str, query: &str) -> (u16, Value) {
    let (s, _, b) = app
        .call(
            Method::GET,
            &format!("{CALS}/{cal}/events?{query}"),
            ALICE,
            None,
        )
        .await;
    (s.as_u16(), b)
}

async fn range(app: &TestApp, cal: &str, from: &str, to: &str, tz: &str) -> (u16, Value) {
    let enc = |s: &str| s.replace(':', "%3A").replace('+', "%2B");
    range_raw(
        app,
        cal,
        &format!("from={}&to={}&tz={tz}", enc(from), enc(to)),
    )
    .await
}

async fn occurrences(app: &TestApp, cal: &str, from: &str, to: &str) -> Vec<Value> {
    let (s, b) = range(app, cal, from, to, "UTC").await;
    assert_eq!(s, 200, "{b}");
    b.as_array().unwrap().clone()
}

const OCT: (&str, &str) = ("2026-10-01T00:00:00Z", "2026-11-05T00:00:00Z");

#[tokio::test]
async fn single_events_in_range() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    post(
        &app,
        &cal,
        timed("2026-10-05T09:00:00", "2026-10-05T10:00:00"),
    )
    .await;
    post(
        &app,
        &cal,
        timed("2026-10-20T09:00:00", "2026-10-20T10:00:00"),
    )
    .await;
    let got = occurrences(&app, &cal, "2026-10-01T00:00:00Z", "2026-10-10T00:00:00Z").await;
    assert_eq!(got.len(), 1);
    assert!(got[0]["original_start"].is_null());
    assert_eq!(got[0]["start_utc"], "2026-10-05T07:00:00Z");
    assert_eq!(got[0]["end_utc"], "2026-10-05T08:00:00Z");
}

#[tokio::test]
async fn weekly_across_dst_change() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(&app, &cal, weekly()).await;
    let got = occurrences(&app, &cal, OCT.0, OCT.1).await;
    assert_eq!(got.len(), 3);
    for (o, utc) in got.iter().zip(["07:00", "08:00", "08:00"]) {
        assert!(o["start"].as_str().unwrap().ends_with("T09:00:00"));
        assert_eq!(o["id"], series["id"]);
        assert!(
            o["start_utc"]
                .as_str()
                .unwrap()
                .contains(&format!("T{utc}:00Z"))
        );
    }
    assert_eq!(got[1]["end"], "2026-10-26T10:00:00");
    assert_eq!(got[1]["original_start"], "2026-10-26T09:00:00");
}

#[tokio::test]
async fn exdate_cancels() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(&app, &cal, weekly()).await;
    put(
        &app,
        &series,
        with(weekly(), json!({"exdates": ["2026-10-26T09:00:00"]})),
    )
    .await;
    assert_eq!(occurrences(&app, &cal, OCT.0, OCT.1).await.len(), 2);
}

async fn override_to(app: &TestApp, cal: &str, series: &Value, start: &str, end: &str) -> Value {
    post(
        app,
        cal,
        with(
            timed(start, end),
            json!({"recurring_event_id": series["id"], "original_start": "2026-10-26T09:00:00",
                   "uid": series["uid"]}),
        ),
    )
    .await
}

#[tokio::test]
async fn override_replaces() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(&app, &cal, weekly()).await;
    let ov = override_to(
        &app,
        &cal,
        &series,
        "2026-10-26T11:00:00",
        "2026-10-26T12:00:00",
    )
    .await;
    let got = occurrences(&app, &cal, OCT.0, OCT.1).await;
    assert_eq!(got.len(), 3);
    assert_eq!(got[1]["id"], ov["id"]);
    assert_eq!(got[1]["start"], "2026-10-26T11:00:00");
    assert_eq!(got[1]["original_start"], "2026-10-26T09:00:00");
}

#[tokio::test]
async fn override_moved_out_of_range() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(&app, &cal, weekly()).await;
    let ov = override_to(
        &app,
        &cal,
        &series,
        "2026-12-01T09:00:00",
        "2026-12-01T10:00:00",
    )
    .await;
    assert_eq!(occurrences(&app, &cal, OCT.0, OCT.1).await.len(), 2);
    let got = occurrences(&app, &cal, "2026-11-30T00:00:00Z", "2026-12-02T00:00:00Z").await;
    // the series' own 11-30 occurrence is in this range too
    assert!(got.iter().any(|o| o["id"] == ov["id"]));
}

#[tokio::test]
async fn stale_override_is_hidden() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(&app, &cal, weekly()).await;
    override_to(
        &app,
        &cal,
        &series,
        "2026-10-26T11:00:00",
        "2026-10-26T12:00:00",
    )
    .await;
    let moved = with(
        timed("2026-10-19T10:00:00", "2026-10-19T11:00:00"),
        json!({"rrule": "FREQ=WEEKLY"}),
    );
    put(&app, &series, moved).await;
    let got = occurrences(&app, &cal, OCT.0, OCT.1).await;
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|o| o["id"] == series["id"]));
    let got = occurrences(&app, &cal, "2026-10-26T10:00:00Z", "2026-10-26T11:30:00Z").await;
    assert!(got.iter().all(|o| o["id"] == series["id"]));
}

#[tokio::test]
async fn override_of_a_former_series_is_hidden() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let series = post(&app, &cal, weekly()).await;
    override_to(
        &app,
        &cal,
        &series,
        "2026-10-26T11:00:00",
        "2026-10-26T12:00:00",
    )
    .await;
    put(
        &app,
        &series,
        timed("2026-10-19T09:00:00", "2026-10-19T10:00:00"),
    )
    .await;
    let got = occurrences(&app, &cal, OCT.0, OCT.1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["id"], series["id"]);
    assert!(got[0]["original_start"].is_null());
}

#[tokio::test]
async fn all_day_uses_query_tz() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    post(&app, &cal, all_day("2026-10-05", "2026-10-06")).await;
    let (from, to) = ("2026-10-05T22:30:00Z", "2026-10-05T23:30:00Z");
    assert_eq!(
        range(&app, &cal, from, to, "UTC")
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (s, b) = range(&app, &cal, from, to, "Europe/Warsaw").await;
    assert_eq!((s, b.as_array().unwrap().len()), (200, 0));
}

#[tokio::test]
async fn crosses_range_start() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    post(
        &app,
        &cal,
        timed("2026-10-04T23:00:00", "2026-10-05T01:00:00"),
    )
    .await;
    let got = occurrences(&app, &cal, "2026-10-04T22:00:00Z", "2026-10-05T22:00:00Z").await;
    assert_eq!(got.len(), 1);
}

#[tokio::test]
async fn old_daily_series() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let body = with(
        timed("2006-10-02T08:00:00", "2006-10-02T09:00:00"),
        json!({"rrule": "FREQ=DAILY"}),
    );
    post(&app, &cal, body).await;
    let got = occurrences(&app, &cal, "2026-10-05T00:00:00Z", "2026-10-12T00:00:00Z").await;
    assert_eq!(got.len(), 7);
}

#[tokio::test]
async fn monthly_on_the_31st() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    post(
        &app,
        &cal,
        with(
            all_day("2026-01-31", "2026-02-01"),
            json!({"rrule": "FREQ=MONTHLY"}),
        ),
    )
    .await;
    let got = occurrences(&app, &cal, "2026-01-01T00:00:00Z", "2026-06-01T00:00:00Z").await;
    let starts: Vec<_> = got.iter().map(|o| o["start"].as_str().unwrap()).collect();
    assert_eq!(starts, ["2026-01-31", "2026-03-31", "2026-05-31"]);
    assert_eq!(got[0]["end"], "2026-02-01");
}

#[tokio::test]
async fn range_validation() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let ok = "from=2026-10-01T00%3A00%3A00Z&to=2026-10-02T00%3A00%3A00Z";
    for q in [
        "to=2026-10-02T00%3A00%3A00Z".to_string(),
        "from=2026-10-02T00%3A00%3A00Z&to=2026-10-01T00%3A00%3A00Z".to_string(),
        "from=2026-01-01T00%3A00%3A00Z&to=2027-01-03T00%3A00%3A00Z".to_string(),
        format!("{ok}&tz=Mars/Olympus"),
        "from=2026-10-05T00:00:00+02:00&to=2026-10-06T00%3A00%3A00Z".to_string(),
        "from=2026-10-01T00%3A00%3A00&to=2026-10-02T00%3A00%3A00Z".to_string(),
        "from=garbage&to=%FF".to_string(),
    ] {
        let (s, b) = range_raw(&app, &cal, &q).await;
        assert_eq!((s, b["code"].as_str()), (422, Some("validation")), "{q}");
    }
    // 366 days exactly is fine
    let (s, _) = range_raw(
        &app,
        &cal,
        "from=2026-01-01T00%3A00%3A00Z&to=2027-01-02T00%3A00%3A00Z",
    )
    .await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn encoded_offset_is_accepted() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let (s, _) = range_raw(
        &app,
        &cal,
        "from=2026-10-05T00%3A00%3A00%2B02%3A00&to=2026-10-06T00%3A00%3A00%2B02%3A00",
    )
    .await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn cap() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    for _ in 0..14 {
        let body = with(
            timed("2026-01-01T09:00:00", "2026-01-01T10:00:00"),
            json!({"rrule": "FREQ=DAILY"}),
        );
        post(&app, &cal, body).await;
    }
    let (s, b) = range(
        &app,
        &cal,
        "2026-01-01T00:00:00Z",
        "2027-01-01T00:00:00Z",
        "UTC",
    )
    .await;
    assert_eq!((s, b["code"].as_str()), (422, Some("too_many_occurrences")));
}

#[tokio::test]
async fn isolation() {
    let app = TestApp::spawn().await;
    let cal = cal(&app).await;
    let (s, _, _) = app
        .call(
            Method::GET,
            &format!(
                "{CALS}/{cal}/events?from=2026-10-01T00%3A00%3A00Z&to=2026-10-02T00%3A00%3A00Z"
            ),
            BOB,
            None,
        )
        .await;
    assert_eq!(s, 404);
}
