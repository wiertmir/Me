mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, BOB, SERVICE_SECRET, TestApp};

const LISTS: &str = "/tasks/v1/lists";

async fn list(app: &TestApp) -> String {
    let (_, _, b) = app.call(Method::GET, LISTS, ALICE, None).await;
    b[0]["id"].as_str().unwrap().to_string()
}

async fn post(app: &TestApp, list: &str, body: Value) -> Value {
    let (s, _, b) = app
        .call(
            Method::POST,
            &format!("{LISTS}/{list}/tasks"),
            ALICE,
            Some(body),
        )
        .await;
    assert_eq!(s, 201, "{b}");
    b
}

fn path(task: &Value) -> String {
    format!("/tasks/v1/tasks/{}", task["id"].as_str().unwrap())
}

async fn put(app: &TestApp, task: &Value, body: Value) {
    let (s, _, b) = app.call(Method::PUT, &path(task), ALICE, Some(body)).await;
    assert_eq!(s, 200, "{b}");
}

async fn changes(app: &TestApp, list: &str, query: &str) -> Value {
    let (s, _, b) = app
        .call(
            Method::GET,
            &format!("{LISTS}/{list}/changes{query}"),
            ALICE,
            None,
        )
        .await;
    assert_eq!(s, 200, "{b}");
    b
}

/// The list's live tasks, by revision.
async fn tasks(app: &TestApp, list: &str) -> Vec<Value> {
    changes(app, list, "").await["tasks"]
        .as_array()
        .unwrap()
        .clone()
}

fn dues(v: &[Value]) -> Vec<&str> {
    let mut d: Vec<&str> = v.iter().map(|t| t["due"].as_str().unwrap()).collect();
    d.sort();
    d
}

/// The task with this `due`.
fn due<'a>(v: &'a [Value], due: &str) -> &'a Value {
    v.iter().find(|t| t["due"] == due).unwrap()
}

fn live(app: &TestApp) -> i64 {
    app.db()
        .query_row("SELECT COUNT(*) FROM tasks WHERE deleted = 0", [], |r| {
            r.get(0)
        })
        .unwrap()
}

/// Adds `n` plain live rows to `list`, behind the service's back.
fn fill(app: &TestApp, list: &str, n: u32) {
    app.db()
        .execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {n})
             INSERT INTO tasks (id, list_id, uid, summary, description, reminders, revision, created_at, updated_at)
             SELECT 'id' || i, '{list}', 'uid' || i, '', '', '[]', 1, 0, 0 FROM n"
        ))
        .unwrap();
}

#[tokio::test]
async fn nothing_before_due_has_passed() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    app.set_now("2026-10-07T23:59:59Z");
    assert_eq!(tasks(&app, &l).await.len(), 1);
}

#[tokio::test]
async fn next_task_when_due_passes() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"summary": "Water plants", "description": "all of them", "priority": 3,
            "due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    app.set_now("2026-10-08T00:00:00Z");
    let got = tasks(&app, &l).await;
    assert_eq!(dues(&got), ["2026-10-07", "2026-10-14"]);
    let (old, new) = (due(&got, "2026-10-07"), due(&got, "2026-10-14"));
    assert_eq!(old["id"], head["id"]);
    assert_eq!(old["rrule"], Value::Null);
    assert_eq!(new["rrule"], "FREQ=WEEKLY");
    assert_eq!(old["recurrence_id"], head["id"]);
    assert_eq!(new["recurrence_id"], head["id"]);
    assert_eq!(new["completed"], false);
    assert_ne!(new["id"], head["id"]);
    assert_ne!(new["uid"], head["uid"]);
    assert_eq!(new["uid"], new["id"]);
    assert_eq!(new["summary"], "Water plants");
    assert_eq!(new["description"], "all of them");
    assert_eq!(new["priority"], 3);
}

#[tokio::test]
async fn timed_head_passes_at_its_instant() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-06T09:00:00", "tz": "Europe/Warsaw", "rrule": "FREQ=DAILY",
            "reminders": [15]}),
    )
    .await;
    app.set_now("2026-10-06T06:59:59Z");
    assert_eq!(tasks(&app, &l).await.len(), 1);
    app.set_now("2026-10-06T07:00:00Z");
    let got = tasks(&app, &l).await;
    assert_eq!(dues(&got), ["2026-10-06T09:00:00", "2026-10-07T09:00:00"]);
    let new = due(&got, "2026-10-07T09:00:00");
    assert_eq!(new["tz"], "Europe/Warsaw");
    assert_eq!(new["reminders"], json!([15]));
}

#[tokio::test]
async fn gap_creates_every_missed_one() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-06", "rrule": "FREQ=DAILY"}),
    )
    .await;
    app.set_now("2026-10-10T00:00:00Z");
    let got = tasks(&app, &l).await;
    assert_eq!(
        dues(&got),
        [
            "2026-10-06",
            "2026-10-07",
            "2026-10-08",
            "2026-10-09",
            "2026-10-10"
        ]
    );
    for t in &got {
        let rule = if t["due"] == "2026-10-10" {
            json!("FREQ=DAILY")
        } else {
            Value::Null
        };
        assert_eq!(t["rrule"], rule, "{}", t["due"]);
    }
}

#[tokio::test]
async fn daily_time_keeps_wall_clock_across_dst() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-24T09:00:00", "tz": "Europe/Warsaw", "rrule": "FREQ=DAILY"}),
    )
    .await;
    app.set_now("2026-10-26T12:00:00Z");
    assert_eq!(
        dues(&tasks(&app, &l).await),
        [
            "2026-10-24T09:00:00",
            "2026-10-25T09:00:00",
            "2026-10-26T09:00:00",
            "2026-10-27T09:00:00"
        ]
    );
}

#[tokio::test]
async fn completed_head_still_produces_the_next() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    put(
        &app,
        &head,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY", "completed": true}),
    )
    .await;
    app.set_now("2026-10-08T00:00:00Z");
    let got = tasks(&app, &l).await;
    assert_eq!(dues(&got), ["2026-10-07", "2026-10-14"]);
    assert_eq!(due(&got, "2026-10-07")["completed"], true);
    assert_eq!(due(&got, "2026-10-14")["completed"], false);
}

#[tokio::test]
async fn until_ends_the_chain() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-06", "rrule": "FREQ=DAILY;UNTIL=20261008"}),
    )
    .await;
    app.set_now("2026-11-01T00:00:00Z");
    let want = ["2026-10-06", "2026-10-07", "2026-10-08"];
    let got = tasks(&app, &l).await;
    assert_eq!(dues(&got), want);
    assert_eq!(
        due(&got, "2026-10-08")["rrule"],
        "FREQ=DAILY;UNTIL=20261008"
    );
    assert_eq!(dues(&tasks(&app, &l).await), want);
}

#[tokio::test]
async fn count_ends_the_chain() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-06", "rrule": "FREQ=DAILY;COUNT=3"}),
    )
    .await;
    app.set_now("2026-11-01T00:00:00Z");
    let want = ["2026-10-06", "2026-10-07", "2026-10-08"];
    let got = tasks(&app, &l).await;
    assert_eq!(dues(&got), want);
    assert_eq!(due(&got, "2026-10-08")["rrule"], "FREQ=DAILY;COUNT=1");
    assert_eq!(dues(&tasks(&app, &l).await), want);
}

// The design's rule (a copy carries COUNT less one) holds for a due that the rule itself does not produce.
#[tokio::test]
async fn count_is_the_number_of_tasks_when_due_is_off_the_rule() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-06", "rrule": "FREQ=WEEKLY;BYDAY=MO;COUNT=3"}),
    )
    .await;
    app.set_now("2026-12-01T00:00:00Z");
    let got = tasks(&app, &l).await;
    assert_eq!(dues(&got), ["2026-10-06", "2026-10-12", "2026-10-19"]);
    assert_eq!(
        due(&got, "2026-10-19")["rrule"],
        "FREQ=WEEKLY;BYDAY=MO;COUNT=1"
    );
}

#[tokio::test]
async fn removing_the_rule_stops_the_chain() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    put(&app, &head, json!({"due": "2026-10-07", "rrule": null})).await;
    app.set_now("2026-10-08T00:00:00Z");
    assert_eq!(tasks(&app, &l).await.len(), 1);
}

#[tokio::test]
async fn deleting_the_head_stops_the_chain() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    let (s, _, _) = app.call(Method::DELETE, &path(&head), ALICE, None).await;
    assert_eq!(s, 204);
    app.set_now("2026-10-08T00:00:00Z");
    assert_eq!(tasks(&app, &l).await.len(), 0);
}

#[tokio::test]
async fn editing_the_head_shapes_the_next() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"summary": "old", "due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    put(
        &app,
        &head,
        json!({"summary": "new", "due": "2026-10-07", "rrule": "FREQ=MONTHLY"}),
    )
    .await;
    app.set_now("2026-10-08T00:00:00Z");
    let got = tasks(&app, &l).await;
    assert_eq!(dues(&got), ["2026-10-07", "2026-11-07"]);
    let new = due(&got, "2026-11-07");
    assert_eq!(new["summary"], "new");
    assert_eq!(new["rrule"], "FREQ=MONTHLY");
}

#[tokio::test]
async fn subtasks_are_copied_open() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    let a = post(
        &app,
        &l,
        json!({"summary": "a", "parent_id": head["id"], "completed": true}),
    )
    .await;
    let b = post(
        &app,
        &l,
        json!({"summary": "b", "parent_id": head["id"], "due": "2026-10-06", "priority": 2}),
    )
    .await;
    app.set_now("2026-10-08T00:00:00Z");
    let got = tasks(&app, &l).await;
    assert_eq!(got.len(), 6);
    let new = due(&got, "2026-10-14");
    let kids = |parent: &Value| -> Vec<&Value> {
        got.iter()
            .filter(|t| t["parent_id"] == parent["id"])
            .collect()
    };
    let (old_kids, new_kids) = (kids(&head), kids(new));
    assert_eq!(old_kids.len(), 2);
    for (old, sent) in old_kids.iter().zip([&a, &b]) {
        assert_eq!(old["id"], sent["id"]);
        assert_eq!(old["completed"], sent["completed"]);
        assert_eq!(old["etag"], sent["etag"]);
    }
    assert_eq!(new_kids.len(), 2);
    for (copy, old) in new_kids.iter().zip([&a, &b]) {
        assert_ne!(copy["id"], old["id"]);
        assert_ne!(copy["uid"], old["uid"]);
        assert_eq!(copy["completed"], false);
        assert_eq!(copy["completed_at"], Value::Null);
        for field in ["summary", "due", "tz", "priority", "reminders"] {
            assert_eq!(copy[field], old[field], "{field}");
        }
    }
}

#[tokio::test]
async fn created_tasks_are_in_the_changes_feed() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    let token = changes(&app, &l, "").await["sync_token"].as_i64().unwrap();
    app.set_now("2026-10-08T00:00:00Z");
    let got = changes(&app, &l, &format!("?since={token}")).await;
    let got = got["tasks"].as_array().unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0]["due"], "2026-10-14");
    assert_eq!(got[1]["id"], head["id"]);
    assert_eq!(got[1]["rrule"], Value::Null);
    assert_ne!(got[0]["etag"], got[1]["etag"]);
    let (_, _, lists) = app.call(Method::GET, LISTS, ALICE, None).await;
    assert_eq!(lists[0]["sync_token"], token + 2);
}

#[tokio::test]
async fn service_secret_caller_also_catches_up() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    app.set_now("2026-10-08T00:00:00Z");
    let resp = app
        .req(Method::GET, LISTS)
        .header("X-Service-Secret", SERVICE_SECRET)
        .header("X-User-Id", ALICE.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(live(&app), 2);
}

#[tokio::test]
async fn bob_does_not_advance_alice() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    app.set_now("2026-10-08T00:00:00Z");
    let (s, _, _) = app.call(Method::GET, LISTS, BOB, None).await;
    assert_eq!(s, 200);
    assert_eq!(live(&app), 1);
    let (s, _, _) = app.call(Method::GET, LISTS, ALICE, None).await;
    assert_eq!(s, 200);
    assert_eq!(live(&app), 2);
}

#[tokio::test]
async fn full_list_creates_nothing_then_resumes() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    fill(&app, &l, 9_999);
    app.set_now("2026-10-08T00:00:00Z");
    let (s, _, _) = app.call(Method::GET, LISTS, ALICE, None).await;
    assert_eq!(s, 200);
    assert_eq!(live(&app), 10_000);
    app.db()
        .execute("UPDATE tasks SET deleted = 1 WHERE id = 'id1'", [])
        .unwrap();
    let (s, _, _) = app.call(Method::GET, LISTS, ALICE, None).await;
    assert_eq!(s, 200);
    assert_eq!(live(&app), 10_000);
    let next: i64 = app
        .db()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE due = '2026-10-14' AND rrule = 'FREQ=WEEKLY'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(next, 1);
}

// Several worker threads, so the requests really run at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_create_one_next_task() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    app.set_now("2026-10-08T00:00:00Z");
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..10 {
        set.spawn(
            app.req(Method::GET, LISTS)
                .bearer_auth(app.token(ALICE))
                .send(),
        );
    }
    for resp in set.join_all().await {
        assert_eq!(resp.unwrap().status(), 200);
    }
    assert_eq!(dues(&tasks(&app, &l).await), ["2026-10-07", "2026-10-14"]);
}

#[tokio::test]
async fn ancient_due_creates_thirty_and_the_head() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "1900-01-01", "rrule": "FREQ=DAILY"}),
    )
    .await;
    let started = std::time::Instant::now();
    let got = tasks(&app, &l).await;
    let took = started.elapsed();
    let d = dues(&got);
    assert_eq!(d.len(), 32);
    assert_eq!(d[..2], ["1900-01-01", "2026-09-05"]);
    assert_eq!(d[31], "2026-10-05");
    assert_eq!(due(&got, "2026-10-05")["rrule"], "FREQ=DAILY");
    assert_eq!(got.iter().filter(|t| !t["rrule"].is_null()).count(), 1);
    assert!(took.as_secs() < 5, "{took:?}");
}

#[tokio::test]
async fn next_occurrence_outside_the_year_window_ends_the_chain() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    post(
        &app,
        &l,
        json!({"due": "2200-06-01", "rrule": "FREQ=YEARLY"}),
    )
    .await;
    app.set_now("2200-06-02T00:00:00Z");
    assert_eq!(tasks(&app, &l).await.len(), 1);
}

#[tokio::test]
async fn unreadable_stored_rule_is_skipped() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let broken = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=DAILY"}),
    )
    .await;
    app.db()
        .execute(
            "UPDATE tasks SET rrule = 'nonsense' WHERE id = ?1",
            [broken["id"].as_str().unwrap()],
        )
        .unwrap();
    app.set_now("2026-10-08T00:00:00Z");
    let (s, _, _) = app.call(Method::GET, LISTS, ALICE, None).await;
    assert_eq!(s, 200);
    assert_eq!(
        dues(&tasks(&app, &l).await),
        ["2026-10-07", "2026-10-07", "2026-10-08"]
    );
}

#[tokio::test]
async fn no_room_for_subtasks_creates_nothing() {
    let app = TestApp::spawn().await;
    let l = list(&app).await;
    let head = post(
        &app,
        &l,
        json!({"due": "2026-10-07", "rrule": "FREQ=WEEKLY"}),
    )
    .await;
    for _ in 0..2 {
        post(&app, &l, json!({"parent_id": head["id"]})).await;
    }
    fill(&app, &l, 9_995);
    assert_eq!(live(&app), 9_998);
    app.set_now("2026-10-08T00:00:00Z");
    let (s, _, _) = app.call(Method::GET, LISTS, ALICE, None).await;
    assert_eq!(s, 200);
    assert_eq!(live(&app), 9_998);
}
