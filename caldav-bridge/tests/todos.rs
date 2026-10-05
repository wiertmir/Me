mod support;

use axum::http::{HeaderMap, Method, StatusCode};
use icalendar::parser::{read_calendar, unfold};
use roxmltree::Document;
use serde_json::{Value, json};
use support::{ALICE, CALDAV, DAV, Stack, named, responses, text};

const ITEM_PROPS: &str = r#"<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/">
  <D:prop><D:getetag/><D:getcontenttype/><D:resourcetype/><CS:getctag/></D:prop></D:propfind>"#;

/// The path of alice's task list, without a trailing slash.
async fn list(s: &Stack) -> String {
    let (_, lists) = s.rest(Method::GET, "/tasks/v1/lists", ALICE, None).await;
    format!(
        "/dav/calendars/alice/t-{}",
        lists[0]["id"].as_str().unwrap()
    )
}

fn vtodo(uid: &str, lines: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTODO\r\nUID:{uid}\r\n\
         DTSTAMP:20261005T080000Z\r\n{lines}\r\nEND:VTODO\r\nEND:VCALENDAR\r\n"
    )
}

async fn put(
    s: &Stack,
    list: &str,
    uid: &str,
    lines: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap) {
    let path = format!("{list}/{uid}.ics");
    let (status, h, _) = s
        .dav("PUT", &path, "alice", headers, &vtodo(uid, lines))
        .await;
    // What is stored is not the body as sent, so the answer names no etag for it.
    assert!(!h.contains_key("etag"));
    (status, h)
}

async fn etag(s: &Stack, list: &str, uid: &str) -> String {
    let (st, h, _) = get(s, list, uid).await;
    assert_eq!(st, StatusCode::OK);
    h["etag"].to_str().unwrap().to_owned()
}

async fn get(s: &Stack, list: &str, uid: &str) -> (StatusCode, HeaderMap, String) {
    s.dav("GET", &format!("{list}/{uid}.ics"), "alice", &[], "")
        .await
}

/// The task as tasks-service has it; `Null` when it has none with that uid.
async fn stored(s: &Stack, list: &str, uid: &str) -> Value {
    let id = list.rsplit("/t-").next().unwrap();
    let (st, found) = s
        .rest(
            Method::GET,
            &format!("/tasks/v1/lists/{id}/by-uid?uid={uid}"),
            ALICE,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    found[0].clone()
}

struct Line {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

/// The properties of the one `VTODO` in an answer, and the `TRIGGER`s of its alarms.
struct Todo {
    props: Vec<Line>,
    alarms: Vec<String>,
}

impl Todo {
    fn parse(body: &str) -> Self {
        let unfolded = unfold(body);
        let cal = read_calendar(&unfolded).unwrap();
        assert_eq!(cal.components.len(), 1);
        let todo = &cal.components[0];
        assert_eq!(todo.name, "VTODO");
        let props = todo.properties.iter().map(|p| {
            let params = p.params.iter().map(|q| {
                let val = q.val.as_ref().map(|v| v.to_string());
                (q.key.to_string(), val.unwrap_or_default())
            });
            Line {
                name: p.name.to_string(),
                params: params.collect(),
                value: p.val.to_string(),
            }
        });
        let alarms = todo.components.iter().map(|a| {
            assert_eq!(a.name, "VALARM");
            assert_eq!(a.find_prop("ACTION").unwrap().val, "DISPLAY");
            a.find_prop("TRIGGER").unwrap().val.to_string()
        });
        Self {
            props: props.collect(),
            alarms: alarms.collect(),
        }
    }

    fn val(&self, name: &str) -> Option<&str> {
        let mut all = self.props.iter().filter(|p| p.name == name);
        let first = all.next()?;
        assert!(all.next().is_none(), "{name} more than once");
        Some(&first.value)
    }

    fn param(&self, name: &str, key: &str) -> Option<&str> {
        let p = self.props.iter().find(|p| p.name == name)?;
        let found = p.params.iter().find(|q| q.0 == key)?;
        Some(&found.1)
    }
}

async fn read(s: &Stack, list: &str, uid: &str) -> Todo {
    let (st, _, body) = get(s, list, uid).await;
    assert_eq!(st, StatusCode::OK);
    Todo::parse(&body)
}

#[tokio::test]
async fn create_read_change_delete() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let body = "SUMMARY:Buy milk\r\nDUE;VALUE=DATE:20261007\r\nPRIORITY:5";
    let (st, _) = put(&s, &l, "t1", body, &[]).await;
    assert_eq!(st, StatusCode::CREATED);
    let etag = etag(&s, &l, "t1").await;

    let (st, h, text) = get(&s, &l, "t1").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h["content-type"], "text/calendar; charset=utf-8");
    assert_eq!(h["etag"], etag.as_str());
    let t = Todo::parse(&text);
    assert_eq!(t.val("UID"), Some("t1"));
    assert_eq!(t.val("SUMMARY"), Some("Buy milk"));
    assert_eq!(t.val("DUE"), Some("20261007"));
    assert_eq!(t.param("DUE", "VALUE"), Some("DATE"));
    assert_eq!(t.val("PRIORITY"), Some("5"));
    assert_eq!(t.val("STATUS"), Some("NEEDS-ACTION"));
    assert!(t.val("DTSTAMP").is_some() && t.val("LAST-MODIFIED").is_some());

    let task = stored(&s, &l, "t1").await;
    assert_eq!(task["due"], "2026-10-07");
    assert_eq!(task["tz"], Value::Null);
    assert_eq!(task["priority"], 5);

    let body = "SUMMARY:Buy oat milk\r\nDUE;VALUE=DATE:20261007\r\nPRIORITY:5";
    let (st, _) = put(&s, &l, "t1", body, &[("if-match", &etag)]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let etag2 = self::etag(&s, &l, "t1").await;
    assert_ne!(etag2, etag);
    assert_eq!(
        read(&s, &l, "t1").await.val("SUMMARY"),
        Some("Buy oat milk")
    );

    let path = format!("{l}/t1.ics");
    let (st, _, _) = s
        .dav("DELETE", &path, "alice", &[("if-match", &etag2)], "")
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(get(&s, &l, "t1").await.0, StatusCode::NOT_FOUND);
    let (st, _, _) = s.dav("DELETE", &path, "alice", &[], "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn timed_due_keeps_its_zone() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let due = "DUE;TZID=Europe/Warsaw:20261007T090000";
    assert_eq!(put(&s, &l, "t1", due, &[]).await.0, StatusCode::CREATED);
    let task = stored(&s, &l, "t1").await;
    assert_eq!(task["due"], "2026-10-07T09:00:00");
    assert_eq!(task["tz"], "Europe/Warsaw");
    let t = read(&s, &l, "t1").await;
    assert_eq!(t.val("DUE"), Some("20261007T090000"));
    assert_eq!(t.param("DUE", "TZID"), Some("Europe/Warsaw"));

    let due = "DUE:20261007T070000Z";
    assert_eq!(put(&s, &l, "t2", due, &[]).await.0, StatusCode::CREATED);
    let task = stored(&s, &l, "t2").await;
    assert_eq!(task["due"], "2026-10-07T07:00:00");
    assert_eq!(task["tz"], "UTC");
}

#[tokio::test]
async fn complete_and_reopen() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let done = "SUMMARY:x\r\nSTATUS:COMPLETED";
    assert_eq!(put(&s, &l, "t1", done, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t1").await["completed"], true);
    let t = read(&s, &l, "t1").await;
    assert_eq!(t.val("STATUS"), Some("COMPLETED"));
    assert_eq!(t.val("PERCENT-COMPLETE"), Some("100"));
    assert!(t.val("COMPLETED").unwrap().ends_with('Z'));

    let open = "SUMMARY:x\r\nSTATUS:NEEDS-ACTION";
    assert_eq!(put(&s, &l, "t1", open, &[]).await.0, StatusCode::NO_CONTENT);
    assert_eq!(stored(&s, &l, "t1").await["completed"], false);
    let t = read(&s, &l, "t1").await;
    assert_eq!(t.val("STATUS"), Some("NEEDS-ACTION"));
    assert_eq!(t.val("COMPLETED"), None);
    assert_eq!(t.val("PERCENT-COMPLETE"), None);

    let percent = "SUMMARY:y\r\nPERCENT-COMPLETE:100";
    assert_eq!(put(&s, &l, "t2", percent, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t2").await["completed"], true);
    let time = "SUMMARY:z\r\nCOMPLETED:20261005T090000Z";
    assert_eq!(put(&s, &l, "t3", time, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t3").await["completed"], true);
}

#[tokio::test]
async fn reminder_round_trips() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let alarm = |trigger: &str| {
        format!("BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:x\r\n{trigger}\r\nEND:VALARM")
    };
    // A second alarm at a fixed time, one after the due and one of 90 seconds are dropped.
    let body = [
        "DUE;TZID=Europe/Warsaw:20261007T090000".to_owned(),
        alarm("TRIGGER:-PT15M"),
        alarm("TRIGGER;VALUE=DATE-TIME:20261007T060000Z"),
        alarm("TRIGGER:PT5M"),
        alarm("TRIGGER:-PT90S"),
    ]
    .join("\r\n");
    assert_eq!(put(&s, &l, "t1", &body, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t1").await["reminders"], json!([15]));
    assert_eq!(read(&s, &l, "t1").await.alarms, ["-PT15M"]);

    // Hours, days and "at the due" count too; without a due there is nothing to remind before.
    let body = [
        "DUE;VALUE=DATE:20261007".to_owned(),
        alarm("TRIGGER;RELATED=END:-P1DT2H"),
        alarm("TRIGGER:PT0S"),
    ]
    .join("\r\n");
    assert_eq!(put(&s, &l, "t2", &body, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t2").await["reminders"], json!([1560, 0]));
    let body = alarm("TRIGGER:-PT15M");
    assert_eq!(put(&s, &l, "t3", &body, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t3").await["reminders"], json!([]));
}

#[tokio::test]
async fn subtask_from_related_to() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    assert_eq!(
        put(&s, &l, "p1", "SUMMARY:parent", &[]).await.0,
        StatusCode::CREATED
    );
    let child = "SUMMARY:child\r\nRELATED-TO:p1";
    assert_eq!(put(&s, &l, "c1", child, &[]).await.0, StatusCode::CREATED);
    let parent_id = stored(&s, &l, "p1").await["id"].clone();
    assert_eq!(stored(&s, &l, "c1").await["parent_id"], parent_id);
    assert_eq!(read(&s, &l, "c1").await.val("RELATED-TO"), Some("p1"));

    let empty = "SUMMARY:empty\r\nRELATED-TO:";
    assert_eq!(put(&s, &l, "e1", empty, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "e1").await["parent_id"], Value::Null);
    let orphan = "SUMMARY:orphan\r\nRELATED-TO:nope";
    assert_eq!(put(&s, &l, "o1", orphan, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "o1").await["parent_id"], Value::Null);
    // The service takes no subtask of a subtask: created without a parent.
    let deep = "SUMMARY:deep\r\nRELATED-TO:c1";
    assert_eq!(put(&s, &l, "d1", deep, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "d1").await["parent_id"], Value::Null);

    let (st, _) = put(&s, &l, "c1", "SUMMARY:child, changed", &[]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let task = stored(&s, &l, "c1").await;
    assert_eq!(task["parent_id"], parent_id);
    assert_eq!(task["summary"], "child, changed");
    let (st, _) = put(&s, &l, "c1", "SUMMARY:child\r\nRELATED-TO:o1", &[]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(stored(&s, &l, "c1").await["parent_id"], parent_id);
}

#[tokio::test]
async fn rule_is_stored_and_never_sent() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let body = "SUMMARY:water\r\nDUE;VALUE=DATE:20261007\r\nRRULE:FREQ=WEEKLY";
    assert_eq!(put(&s, &l, "t1", body, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t1").await["rrule"], "FREQ=WEEKLY");
    assert_eq!(read(&s, &l, "t1").await.val("RRULE"), None);

    let body = "SUMMARY:water the plants\r\nDUE;VALUE=DATE:20261007";
    assert_eq!(put(&s, &l, "t1", body, &[]).await.0, StatusCode::NO_CONTENT);
    let task = stored(&s, &l, "t1").await;
    assert_eq!(task["rrule"], "FREQ=WEEKLY");
    assert_eq!(task["summary"], "water the plants");
    // Whatever the body says.
    let body = "SUMMARY:water\r\nDUE;VALUE=DATE:20261007\r\nRRULE:FREQ=DAILY";
    assert_eq!(put(&s, &l, "t1", body, &[]).await.0, StatusCode::NO_CONTENT);
    assert_eq!(stored(&s, &l, "t1").await["rrule"], "FREQ=WEEKLY");
}

#[tokio::test]
async fn rule_without_a_due_is_dropped() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let body = "SUMMARY:water\r\nRRULE:FREQ=DAILY";
    assert_eq!(put(&s, &l, "t1", body, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t1").await["rrule"], Value::Null);

    // A stored series ends when the client takes its due away.
    let body = "SUMMARY:water\r\nDUE;VALUE=DATE:20261007\r\nRRULE:FREQ=WEEKLY";
    assert_eq!(put(&s, &l, "t2", body, &[]).await.0, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t2").await["rrule"], "FREQ=WEEKLY");
    let (st, _) = put(&s, &l, "t2", "SUMMARY:water", &[]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let task = stored(&s, &l, "t2").await;
    assert_eq!(task["due"], Value::Null);
    assert_eq!(task["rrule"], Value::Null);
}

#[tokio::test]
async fn listing_and_reports() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let mut etags = Vec::new();
    for uid in ["t1", "t2", "t3"] {
        let (st, _) = put(&s, &l, uid, "SUMMARY:x", &[]).await;
        assert_eq!(st, StatusCode::CREATED);
        etags.push(etag(&s, &l, uid).await);
    }
    let href = |uid: &str| format!("{l}/{uid}.ics");

    let collection = format!("{l}/");
    let (st, _, xml) = s
        .dav(
            "PROPFIND",
            &collection,
            "alice",
            &[("depth", "1")],
            ITEM_PROPS,
        )
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 4);
    assert_eq!(r[0].0, collection);
    for (uid, etag) in ["t1", "t2", "t3"].iter().zip(&etags) {
        let (_, item) = r.iter().find(|(h, _)| *h == href(uid)).unwrap();
        assert_eq!(&text(*item, DAV, "getetag"), etag);
    }
    let (st, _, xml) = s
        .dav("PROPFIND", &collection, "alice", &[], ITEM_PROPS)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    assert_eq!(responses(&Document::parse(&xml).unwrap()).len(), 1);

    let multiget = format!(
        r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><D:getetag/><C:calendar-data/></D:prop>
        <D:href>{}</D:href><D:href>https://me.example:8443{}</D:href><D:href>{}</D:href></C:calendar-multiget>"#,
        href("t1"),
        href("t3"),
        href("gone")
    );
    let (st, _, xml) = s
        .dav("REPORT", &collection, "alice", &[("depth", "1")], &multiget)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 3);
    for (uid, etag) in [("t1", &etags[0]), ("t3", &etags[2])] {
        let (_, item) = r.iter().find(|(h, _)| *h == href(uid)).unwrap();
        assert_eq!(&text(*item, DAV, "getetag"), etag);
        let data = text(*item, CALDAV, "calendar-data");
        assert_eq!(Todo::parse(&data).val("UID"), Some(uid));
    }
    let (_, gone) = r.iter().find(|(h, _)| *h == href("gone")).unwrap();
    assert!(text(*gone, DAV, "status").contains("404"));
    assert!(named(*gone, DAV, "propstat").is_none());

    let query = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><D:getetag/><C:calendar-data/></D:prop>
        <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VTODO"/></C:comp-filter></C:filter>
        </C:calendar-query>"#;
    let (st, _, xml) = s
        .dav("REPORT", &collection, "alice", &[("depth", "1")], query)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 3);
    for (_, item) in &r {
        assert!(text(*item, CALDAV, "calendar-data").contains("BEGIN:VTODO"));
    }

    let other =
        r#"<D:sync-collection xmlns:D="DAV:"><D:prop><D:getetag/></D:prop></D:sync-collection>"#;
    let (st, _, _) = s.dav("REPORT", &collection, "alice", &[], other).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn subtask_in_a_report_names_its_parent() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    put(&s, &l, "p1", "SUMMARY:parent", &[]).await;
    put(&s, &l, "c1", "SUMMARY:child\r\nRELATED-TO:p1", &[]).await;
    let query = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><C:calendar-data/></D:prop></C:calendar-query>"#;
    let (_, _, xml) = s.dav("REPORT", &format!("{l}/"), "alice", &[], query).await;
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    let (_, child) = r.iter().find(|(h, _)| h.ends_with("/c1.ics")).unwrap();
    let data = text(*child, CALDAV, "calendar-data");
    assert_eq!(Todo::parse(&data).val("RELATED-TO"), Some("p1"));
}

#[tokio::test]
async fn item_propfind() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    put(&s, &l, "t1", "SUMMARY:x", &[]).await;
    let stored_etag = etag(&s, &l, "t1").await;
    let path = format!("{l}/t1.ics");
    let (st, _, xml) = s
        .dav("PROPFIND", &path, "alice", &[("depth", "0")], ITEM_PROPS)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, path);
    assert_eq!(text(r[0].1, DAV, "getetag"), stored_etag);
    assert!(text(r[0].1, DAV, "getcontenttype").starts_with("text/calendar"));
    let kind = named(r[0].1, DAV, "resourcetype").unwrap();
    assert!(!kind.has_children());
    // An item has no change marker of its own.
    let missing = doc
        .descendants()
        .find(|n| n.tag_name().name() == "propstat" && text(*n, DAV, "status").contains("404"))
        .unwrap();
    assert!(named(missing, "http://calendarserver.org/ns/", "getctag").is_some());
    // Nor a name, also when everything is asked for.
    let (_, _, xml) = s.dav("PROPFIND", &path, "alice", &[], "").await;
    let doc = Document::parse(&xml).unwrap();
    assert!(named(doc.root_element(), DAV, "getetag").is_some());
    assert!(named(doc.root_element(), DAV, "displayname").is_none());
    let name = r#"<D:propfind xmlns:D="DAV:"><D:prop><D:displayname/></D:prop></D:propfind>"#;
    let (_, _, xml) = s.dav("PROPFIND", &path, "alice", &[], name).await;
    assert!(xml.contains("404 Not Found"), "{xml}");

    let gone = format!("{l}/nope.ics");
    let (st, _, _) = s.dav("PROPFIND", &gone, "alice", &[], ITEM_PROPS).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn preconditions() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let (st, _) = put(&s, &l, "t1", "SUMMARY:x", &[("if-none-match", "*")]).await;
    assert_eq!(st, StatusCode::CREATED);
    let etag = etag(&s, &l, "t1").await;

    let (st, _) = put(&s, &l, "t1", "SUMMARY:y", &[("if-none-match", "*")]).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let (st, _) = put(&s, &l, "t1", "SUMMARY:y", &[("if-match", "\"0\"")]).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let path = format!("{l}/t1.ics");
    let (st, _, _) = s
        .dav("DELETE", &path, "alice", &[("if-match", "\"0\"")], "")
        .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let (st, _) = put(&s, &l, "t2", "SUMMARY:y", &[("if-match", &etag)]).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);

    assert_eq!(stored(&s, &l, "t1").await["summary"], "x");
    assert_eq!(stored(&s, &l, "t2").await, Value::Null);
}

#[tokio::test]
async fn name_must_be_the_uid() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let (st, _, body) = s
        .dav(
            "PUT",
            &format!("{l}/x.ics"),
            "alice",
            &[],
            &vtodo("y", "SUMMARY:x"),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body.contains("valid-calendar-object-resource"), "{body}");
}

#[tokio::test]
async fn vevent_in_a_todo_collection_is_refused() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let event = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:e1\r\n\
                 DTSTART:20261007T070000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let two = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:e1\r\nEND:VTODO\r\n\
               BEGIN:VTODO\r\nUID:e1\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
    for body in [event, two] {
        let (st, _, answer) = s
            .dav("PUT", &format!("{l}/e1.ics"), "alice", &[], body)
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(answer.contains("supported-calendar-data"), "{answer}");
    }
    for body in ["", "hello", "BEGIN:VTODO\r\nUID:e1\r\nEND:VTODO\r\n"] {
        let (st, _, answer) = s
            .dav("PUT", &format!("{l}/e1.ics"), "alice", &[], body)
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
        assert!(answer.contains("valid-calendar-data"), "{answer}");
    }
}

#[tokio::test]
async fn timezone_component_is_accepted() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Warsaw\r\n\
                BEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\n\
                END:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VTODO\r\nUID:t1\r\n\
                DUE;TZID=Europe/Warsaw:20261007T090000\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
    let (st, _, _) = s
        .dav("PUT", &format!("{l}/t1.ics"), "alice", &[], body)
        .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(stored(&s, &l, "t1").await["tz"], "Europe/Warsaw");
}

async fn refused_as_invalid(lines: &str) {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let (st, _, body) = s
        .dav(
            "PUT",
            &format!("{l}/t1.ics"),
            "alice",
            &[],
            &vtodo("t1", lines),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body.contains("valid-calendar-data"), "{body}");
    assert_eq!(stored(&s, &l, "t1").await, Value::Null);
}

#[tokio::test]
async fn floating_due_is_refused() {
    refused_as_invalid("DUE:20261007T090000").await;
}

#[tokio::test]
async fn unknown_tzid_is_refused() {
    refused_as_invalid("DUE;TZID=Mars/Olympus:20261007T090000").await;
}

#[tokio::test]
async fn service_refusal_is_403() {
    refused_as_invalid(&format!("SUMMARY:{}", "x".repeat(501))).await;
}

#[tokio::test]
async fn change_made_elsewhere_shows() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    put(&s, &l, "t1", "SUMMARY:x", &[]).await;
    let collection = format!("{l}/");
    let marks = async || {
        let (_, _, xml) = s
            .dav(
                "PROPFIND",
                &collection,
                "alice",
                &[("depth", "1")],
                ITEM_PROPS,
            )
            .await;
        let doc = Document::parse(&xml).unwrap();
        let r = responses(&doc);
        let ctag = text(r[0].1, "http://calendarserver.org/ns/", "getctag");
        (ctag, text(r[1].1, DAV, "getetag"))
    };
    let before = marks().await;
    let id = stored(&s, &l, "t1").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (st, _) = s
        .rest(
            Method::PUT,
            &format!("/tasks/v1/tasks/{id}"),
            ALICE,
            Some(json!({"summary": "changed elsewhere"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let after = marks().await;
    assert_ne!(after.0, before.0);
    assert_ne!(after.1, before.1);
    assert_eq!(
        read(&s, &l, "t1").await.val("SUMMARY"),
        Some("changed elsewhere")
    );
}

#[tokio::test]
async fn dropped_properties_stay_dropped() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let body = "SUMMARY:x\r\nCATEGORIES:a,b\r\nDTSTART;TZID=Europe/Warsaw:20261006T090000\r\n\
                X-FOO:bar\r\nPRIORITY:42\r\nSTATUS:IN-PROCESS";
    assert_eq!(put(&s, &l, "t1", body, &[]).await.0, StatusCode::CREATED);
    let t = read(&s, &l, "t1").await;
    for name in ["CATEGORIES", "DTSTART", "X-FOO", "DUE"] {
        assert_eq!(t.val(name), None, "{name}");
    }
    assert_eq!(t.val("STATUS"), Some("NEEDS-ACTION"));
    assert_eq!(stored(&s, &l, "t1").await["priority"], 0);
}

#[tokio::test]
async fn uid_with_reserved_characters_round_trips() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let uid = "a b/c@x%1";
    let path = format!("{l}/a%20b%2Fc%40x%251.ics");
    let (st, _, _) = s
        .dav("PUT", &path, "alice", &[], &vtodo(uid, "SUMMARY:x"))
        .await;
    assert_eq!(st, StatusCode::CREATED);

    let (_, _, xml) = s
        .dav(
            "PROPFIND",
            &format!("{l}/"),
            "alice",
            &[("depth", "1")],
            ITEM_PROPS,
        )
        .await;
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 2);
    let href = &r[1].0;
    assert_eq!(href, &path);
    let (st, _, body) = s.dav("GET", href, "alice", &[], "").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(Todo::parse(&body).val("UID"), Some(uid));

    let multiget = format!(
        r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><D:getetag/></D:prop><D:href>{href}</D:href></C:calendar-multiget>"#
    );
    let (_, _, xml) = s
        .dav("REPORT", &format!("{l}/"), "alice", &[], &multiget)
        .await;
    let doc = Document::parse(&xml).unwrap();
    assert!(named(doc.root_element(), DAV, "getetag").is_some());
}

#[tokio::test]
async fn text_escaping_and_long_lines_round_trip() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let summary = "a, b; c\\d";
    let description = format!("{}\n{}", "słowo and more ".repeat(10), "x".repeat(149));
    assert_eq!(description.chars().count(), 300);
    let body = format!(
        "SUMMARY:a\\, b\\; c\\\\d\r\nDESCRIPTION:{}",
        description.replace('\n', "\\n")
    );
    assert_eq!(put(&s, &l, "t1", &body, &[]).await.0, StatusCode::CREATED);
    let task = stored(&s, &l, "t1").await;
    assert_eq!(task["summary"], summary);
    assert_eq!(task["description"], description.as_str());

    let (_, _, ics) = get(&s, &l, "t1").await;
    assert!(ics.split("\r\n").all(|line| line.len() <= 75), "{ics}");
    let t = Todo::parse(&ics);
    assert_eq!(t.val("SUMMARY"), Some(summary));
    assert_eq!(t.val("DESCRIPTION"), Some(description.as_str()));
}

#[tokio::test]
async fn calendar_data_is_xml_escaped() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let summary = "<b>&]]>";
    let (st, _) = put(&s, &l, "t1", &format!("SUMMARY:{summary}"), &[]).await;
    assert_eq!(st, StatusCode::CREATED);
    let multiget = format!(
        r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><C:calendar-data/></D:prop><D:href>{l}/t1.ics</D:href></C:calendar-multiget>"#
    );
    let (_, _, xml) = s
        .dav("REPORT", &format!("{l}/"), "alice", &[], &multiget)
        .await;
    let doc = Document::parse(&xml).unwrap();
    let data = text(doc.root_element(), CALDAV, "calendar-data");
    assert_eq!(Todo::parse(&data).val("SUMMARY"), Some(summary));
}

#[tokio::test]
async fn oversize_put_is_413() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    let lines = format!("DESCRIPTION:{}", "x".repeat(1024 * 1024));
    let (st, _) = put(&s, &l, "t1", &lines, &[]).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn other_users_list_is_404() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    put(&s, &l, "t1", "SUMMARY:x", &[]).await;
    // Bob's own name with alice's list: the service does not know it as his.
    let path = format!("{}/t1.ics", l.replace("/alice/", "/bob/"));
    for method in ["GET", "DELETE", "PROPFIND"] {
        let (st, _, _) = s.dav(method, &path, "bob", &[], "").await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{method}");
    }
    let (st, _, _) = s
        .dav("PUT", &path, "bob", &[], &vtodo("t1", "SUMMARY:mine now"))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _) = s.dav("GET", &format!("{l}/t1.ics"), "bob", &[], "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(stored(&s, &l, "t1").await["summary"], "x");
}

#[tokio::test]
async fn methods_need_the_right_kind_of_path() {
    let s = Stack::spawn().await;
    let l = list(&s).await;
    for method in ["GET", "PUT", "DELETE"] {
        let (st, _, _) = s.dav(method, &format!("{l}/"), "alice", &[], "").await;
        assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
    let (st, _, _) = s
        .dav("REPORT", &format!("{l}/t1.ics"), "alice", &[], "")
        .await;
    assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
}
