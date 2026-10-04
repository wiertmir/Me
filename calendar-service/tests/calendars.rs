mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, TestApp};

const LIST: &str = "/calendar/v1/calendars";

#[tokio::test]
async fn first_list_creates_personal() {
    let app = TestApp::spawn().await;
    let (s, _, b) = app.call(Method::GET, LIST, ALICE, None).await;
    assert_eq!(s, 200);
    let items = b.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "Personal");
    assert_eq!(items[0]["color"], "#3b82f6");
    assert_eq!(items[0]["sync_token"], 0);
    let (_, _, again) = app.call(Method::GET, LIST, ALICE, None).await;
    assert_eq!(again[0]["id"], items[0]["id"]);
}

#[tokio::test]
async fn create_read_patch_delete() {
    let app = TestApp::spawn().await;
    let (s, _, c) = app
        .call(
            Method::POST,
            LIST,
            ALICE,
            Some(json!({"name": "Work", "color": "#ff0000"})),
        )
        .await;
    assert_eq!(s, 201);
    let path = format!("{LIST}/{}", c["id"].as_str().unwrap());
    let (s, _, got) = app.call(Method::GET, &path, ALICE, None).await;
    assert_eq!((s.as_u16(), &got), (200, &c));
    let (s, _, p) = app
        .call(Method::PATCH, &path, ALICE, Some(json!({"name": "Job"})))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        (&p["name"], &p["color"]),
        (&json!("Job"), &json!("#ff0000"))
    );
    let (s, _, _) = app.call(Method::DELETE, &path, ALICE, None).await;
    assert_eq!(s, 204);
    let (s, _, _) = app.call(Method::GET, &path, ALICE, None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn validation() {
    let app = TestApp::spawn().await;
    let long = "x".repeat(101);
    let bad = [
        json!({"name": ""}),
        json!({"name": long}),
        json!({"name": "a", "color": "red"}),
        json!({"name": "a", "color": "#12345G"}),
    ];
    for body in bad {
        let (s, _, b) = app.call(Method::POST, LIST, ALICE, Some(body)).await;
        assert_eq!((s.as_u16(), &b["code"]), (422, &json!("validation")));
    }
    // PATCH validates too
    let (_, _, c) = app
        .call(Method::POST, LIST, ALICE, Some(json!({"name": "a"})))
        .await;
    let path = format!("{LIST}/{}", c["id"].as_str().unwrap());
    let (s, _, _) = app
        .call(Method::PATCH, &path, ALICE, Some(json!({"color": "red"})))
        .await;
    assert_eq!(s, 422);
}

#[tokio::test]
async fn limit_of_100() {
    let app = TestApp::spawn().await;
    // The first list creates "Personal", so 99 more fill the quota.
    app.call(Method::GET, LIST, ALICE, None).await;
    for i in 0..99 {
        let (s, _, _) = app
            .call(
                Method::POST,
                LIST,
                ALICE,
                Some(json!({"name": format!("c{i}")})),
            )
            .await;
        assert_eq!(s, 201);
    }
    let (s, _, b) = app
        .call(
            Method::POST,
            LIST,
            ALICE,
            Some(json!({"name": "one too many"})),
        )
        .await;
    assert_eq!((s.as_u16(), &b["code"]), (409, &json!("conflict")));
}

#[tokio::test]
async fn isolation() {
    let app = TestApp::spawn().await;
    let (_, _, c) = app
        .call(Method::POST, LIST, ALICE, Some(json!({"name": "Mine"})))
        .await;
    let path = format!("{LIST}/{}", c["id"].as_str().unwrap());
    for (m, body) in [
        (Method::GET, None),
        (Method::PATCH, Some(json!({"name": "x"}))),
        (Method::DELETE, None),
    ] {
        let (s, _, b) = app.call(m, &path, BOB, body).await;
        assert_eq!((s.as_u16(), &b["code"]), (404, &json!("not_found")));
    }
    let (_, _, list) = app.call(Method::GET, LIST, BOB, None).await;
    assert!(list.as_array().unwrap().iter().all(|x| x["id"] != c["id"]));
    // untouched for Alice
    let (s, _, _) = app.call(Method::GET, &path, ALICE, None).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn non_uuid_id_is_404() {
    let app = TestApp::spawn().await;
    let (s, _, _): (_, _, Value) = app
        .call(Method::GET, &format!("{LIST}/abc"), ALICE, None)
        .await;
    assert_eq!(s, 404);
}
